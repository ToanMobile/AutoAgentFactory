//! `aaf cockpit [--repo <path> | --all]`: synchronous interactive TTY cockpit.
//!
//! Zero network ports, zero new crates, no raw memory access, and strictly
//! synchronous (no background refresh while waiting for input) so cooked-mode
//! terminal typing is never clobbered.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use aaf_domain::dag;
use aaf_domain::event::FactoryEvent;
use aaf_domain::ports::{Clock, EventSink, TaskStore};
use aaf_domain::run::RunRecord;
use aaf_domain::task::{Task, TaskState};
use aaf_engine::db::{Db, RoDbCache};
use aaf_engine::factory::schedule_order::blocked_upstream_tasks;
use aaf_engine::handover::{load_handover, render_markdown};
use aaf_engine::watch::{
    WatchQuery, WatchSnapshot, collect_watch_snapshot_with_tasks, format_blocked,
    sanitize_display as san,
};

use super::cockpit_actions::{
    DB_FILE, action_approve, action_rework, action_steer, action_unblock, action_undo,
    action_unfreeze, io_err, load_ro_task, open_db_ro, run_tail_view, wln,
};
use super::handover::load_repo_config;
use super::scope::resolve_scope;
use super::task_ops::compute_critical_path;
use crate::ctx::{Ctx, SystemClock};
use crate::output::{CmdError, CmdOutput, CmdResult};

const TASK_EVENTS_LIMIT: u32 = 20;
const RECENT_EVENTS_SHOWN: usize = 5;
const EVENT_EXCERPT_CHARS: usize = 100;

#[derive(Debug, clap::Args)]
pub struct CockpitArgs {
    /// Inspect a specific repository path instead of the current directory.
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// Aggregate across all repositories in the database.
    #[arg(long, conflicts_with = "repo")]
    pub all: bool,
}

pub fn run(args: &CockpitArgs, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = check_entry_gates(ctx) {
        return Ok(refused);
    }
    let (stdin, stdout) = (io::stdin(), io::stdout());
    let (mut reader, mut writer) = (stdin.lock(), stdout.lock());
    run_cockpit(
        ctx,
        args.repo.as_deref(),
        args.all,
        &mut reader,
        &mut writer,
    )
}

fn check_entry_gates(ctx: &Ctx) -> Option<CmdOutput> {
    if let Some(run_id) = &ctx.run_id {
        return Some(CmdOutput::policy(
            "refused",
            format!("aaf cockpit is a human action: refused inside agent run {run_id}"),
        ));
    }
    if !ctx.stdin_is_tty {
        return Some(CmdOutput::policy(
            "refused",
            "aaf cockpit requires an interactive TTY",
        ));
    }
    None
}

/// Testable synchronous cockpit REPL driven by explicit `BufRead` and `Write`.
pub fn run_cockpit(
    ctx: &Ctx,
    repo: Option<&Path>,
    all: bool,
    reader: &mut dyn BufRead,
    writer: &mut dyn Write,
) -> Result<CmdOutput, CmdError> {
    if let Some(refused) = check_entry_gates(ctx) {
        return Ok(refused);
    }
    let (scope, scoped_repo, config_root) = resolve_scope(repo, all, ctx)?;
    let cfg = ctx.load_config(&config_root)?;
    let ro_db = RoDbCache::new(ctx.data_dir.join(DB_FILE));
    let mut needs_render = true;

    loop {
        if needs_render {
            let db = open_db_ro(&ro_db)?;
            let repo_str = scoped_repo
                .as_deref()
                .map(|p| p.to_string_lossy().into_owned());
            let all_tasks = match db.as_ref() {
                Some(d) => d
                    .list_tasks(repo_str.as_deref(), None)
                    .map_err(|e| CmdError::new(e.to_string()))?,
                None => Vec::new(),
            };
            let query = WatchQuery {
                scope: &scope,
                repo_root: scoped_repo.as_deref(),
                data_dir: &ctx.data_dir,
                selection: cfg.global.accounts.selection,
                executor: &cfg.repo.run.executor,
                now: SystemClock.now(),
            };
            let snap = collect_watch_snapshot_with_tasks(&query, db.as_ref(), &all_tasks)
                .map_err(|e| CmdError::new(e.to_string()))?;
            let extras = collect_extras(db.as_ref(), repo_str.as_deref(), &all_tasks);
            let rendered = format_panels(&snap, &extras);
            writer
                .write_all(rendered.as_bytes())
                .map_err(|e| io_err("write", e))?;
        }
        write!(writer, "cockpit> ").map_err(|e| io_err("write", e))?;
        writer.flush().map_err(|e| io_err("flush", e))?;

        let mut line = String::new();
        if reader.read_line(&mut line).map_err(|e| io_err("read", e))? == 0 {
            return Ok(CmdOutput::ok("ok", ""));
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed == "r" {
            needs_render = true;
            continue;
        }
        if matches!(trimmed, "q" | "quit") {
            return Ok(CmdOutput::ok("ok", ""));
        }
        needs_render =
            dispatch_command(ctx, &ro_db, scoped_repo.as_deref(), trimmed, reader, writer)?;
    }
}

fn dispatch_command(
    ctx: &Ctx,
    ro: &RoDbCache,
    scoped_repo: Option<&Path>,
    trimmed: &str,
    reader: &mut dyn BufRead,
    w: &mut dyn Write,
) -> Result<bool, CmdError> {
    let (cmd, rest) = split_first(trimmed);
    match cmd {
        "d" => show_task_details(ro, rest.trim(), w).map(|()| false),
        "t" => show_task_events(ro, rest.trim(), w).map(|()| false),
        "h" => show_task_handover(ctx, ro, rest.trim(), w).map(|()| false),
        "g" => show_dag(ro, scoped_repo, w).map(|()| false),
        "tail" => run_tail_view(ctx, ro, rest.trim(), reader, w).map(|()| false),
        "approve" => {
            let (id, rev) = split_first(rest);
            if id.is_empty() {
                wln(w, "usage: approve <id> [reviewer]").map(|()| false)
            } else {
                action_approve(ctx, ro, id, (!rev.is_empty()).then_some(rev), reader, w)
                    .map(|()| true)
            }
        }
        "rework" => {
            let (id, reason) = split_first(rest);
            if id.is_empty() || reason.is_empty() {
                wln(w, "usage: rework <id> [--rerun] <reason>").map(|()| false)
            } else {
                action_rework(ctx, ro, id, reason, reader, w).map(|()| true)
            }
        }
        "steer" => {
            let (id, note) = split_first(rest);
            if id.is_empty() || note.is_empty() {
                wln(w, "usage: steer <id> <note>").map(|()| false)
            } else {
                action_steer(ctx, id, note, reader, w).map(|()| true)
            }
        }
        "undo" if !rest.trim().is_empty() => {
            action_undo(ctx, ro, rest.trim(), reader, w).map(|()| true)
        }
        "undo" => wln(w, "usage: undo <id>").map(|()| false),
        "unblock" if !rest.trim().is_empty() => {
            action_unblock(ctx, ro, rest.trim(), reader, w).map(|()| true)
        }
        "unblock" => wln(w, "usage: unblock <id>").map(|()| false),
        "unfreeze" => action_unfreeze(ctx, reader, w).map(|()| true),
        _ => wln(
            w,
            &format!(
                "unknown command: {} (help: Enter/r=refresh, q=quit, d/t/h/tail <id>, g, approve/rework/steer/undo/unblock/unfreeze)",
                san(cmd)
            ),
        )
        .map(|()| false),
    }
}

fn split_first(input: &str) -> (&str, &str) {
    let trimmed = input.trim();
    match trimmed.find(char::is_whitespace) {
        Some(idx) => (&trimmed[..idx], trimmed[idx..].trim_start()),
        None => (trimmed, ""),
    }
}

struct CockpitExtras {
    scopes_by_id: HashMap<String, Vec<String>>,
    active_runs: Vec<RunRecord>,
    recent_events: Vec<FactoryEvent>,
}

fn collect_extras(db: Option<&Db>, repo_str: Option<&str>, tasks: &[Task]) -> CockpitExtras {
    let mut extras = CockpitExtras {
        scopes_by_id: HashMap::new(),
        active_runs: Vec::new(),
        recent_events: Vec::new(),
    };
    let Some(db) = db else { return extras };
    let mut active_ids = HashSet::new();
    for task in tasks {
        if !task.scope_paths.is_empty() {
            extras
                .scopes_by_id
                .insert(task.id.clone(), task.scope_paths.clone());
        }
        if !task.state.is_terminal() {
            active_ids.insert(task.id.as_str());
        }
    }
    extras.active_runs = db
        .running_runs(repo_str)
        .unwrap_or_default()
        .into_iter()
        .filter(|r| active_ids.contains(r.task_id.as_str()))
        .collect();
    extras.recent_events = db
        .recent_events(repo_str, RECENT_EVENTS_SHOWN as u32)
        .unwrap_or_default();
    extras
}

fn format_panels(snap: &WatchSnapshot, extras: &CockpitExtras) -> String {
    let mut buf = String::new();
    let frz = if snap.frozen { "yes" } else { "no" };
    let (sc, fails) = (san(&snap.scope), snap.consecutive_root_fails);
    let _ = writeln!(
        buf,
        "== aaf cockpit | scope: {sc} | frozen: {frz} (fails: {fails}) =="
    );
    let c = &snap.tasks.counts;
    let _ = writeln!(
        buf,
        "\n[1. Tasks / Board] total={} ready={} running={} gating={} review={} blocked={} backlog={} done={} cancelled={}",
        c.total, c.ready, c.running, c.gating, c.review, c.blocked, c.backlog, c.done, c.cancelled
    );
    if snap.tasks.active.is_empty() {
        let _ = writeln!(buf, "  (no active tasks)");
    }
    for t in &snap.tasks.active {
        let sc = extras.scopes_by_id.get(&t.id).map_or(String::new(), |p| {
            let clean: Vec<String> = p.iter().map(|s| san(s)).collect();
            format!(" [scope: {}]", clean.join(", "))
        });
        let blk = t
            .blocked_reason
            .as_deref()
            .map_or(String::new(), |r| format!(" [{}]", san(r)));
        let (id, pr, st, tt) = (
            san(&t.id),
            san(&t.priority),
            t.state.as_str(),
            san(&t.title),
        );
        let _ = writeln!(
            buf,
            "  {id}  {pr}  {st:<8}  cycles={}  {tt}{sc}{blk}",
            t.unblock_cycles
        );
    }
    let hs = &snap.heavy_slots;
    let drain = if hs.drain_active { "on" } else { "off" };
    let (mp, mc) = (san(&hs.memory_pressure), hs.measured_commands);
    let _ = writeln!(
        buf,
        "\n[2. Active Runs & Heavy Slots] frozen={frz} drain={drain} pressure={mp} measured={mc}"
    );
    if extras.active_runs.is_empty() {
        let _ = writeln!(buf, "  (no active runs)");
    }
    for r in &extras.active_runs {
        let (rid, tid, ex, br) = (
            san(&r.id),
            san(&r.task_id),
            san(&r.executor),
            san(&r.branch),
        );
        let _ = writeln!(buf, "  run {rid}  task={tid}  executor={ex}  branch={br}");
    }
    if hs.slots.is_empty() {
        let _ = writeln!(buf, "  (no slot files)");
    }
    for s in &hs.slots {
        let pg = s.pgid.map_or(String::new(), |p| format!("  pgid={p}"));
        let ram = s
            .reserved_ram_mb
            .map_or(String::new(), |r| format!("  ram={r}MB"));
        let lbl = s
            .label
            .as_deref()
            .map_or(String::new(), |l| format!("  label={}", san(l)));
        let _ = writeln!(buf, "  slot {}  {:<6}{pg}{ram}{lbl}", s.slot, san(&s.state));
    }
    let ac = &snap.accounts;
    let pin = ac
        .pinned_account
        .as_deref()
        .map_or_else(|| "none".into(), san);
    let _ = writeln!(
        buf,
        "\n[3. Accounts & Cooldowns] selection={} pinned={pin}",
        san(&ac.selection_mode)
    );
    if ac.entries.is_empty() {
        let _ = writeln!(buf, "  (no accounts)");
    }
    for a in &ac.entries {
        let cd = a
            .cooldown_until
            .map_or(String::new(), |ts| format!(" until={ts}"));
        let (id, prov, st) = (san(&a.id), san(&a.provider), san(&a.state));
        let _ = writeln!(buf, "  {id}  {prov}  {st}{cd}  tokens_5h={}", a.tokens_5h);
    }
    let _ = writeln!(buf, "\n[4. Recent Events / Alerts & Commands]");
    if snap.frozen {
        let _ = writeln!(buf, "  ALERT: factory is FROZEN (run `unfreeze` to clear)");
    }
    if let Some(r) = &snap.receipt {
        let _ = writeln!(
            buf,
            "  receipt: {:?} ({:?}) tree={} warnings={} created_at={}",
            r.status,
            r.mode,
            san(&r.tree_fp_short),
            r.warnings,
            r.created_at
        );
    }
    if extras.recent_events.is_empty() {
        let _ = writeln!(buf, "  (no recent events)");
    }
    for ev in &extras.recent_events {
        let tid = ev.task_id.as_deref().map_or_else(|| "-".into(), san);
        let excerpt: String = san(&ev.payload_json)
            .chars()
            .take(EVENT_EXCERPT_CHARS)
            .collect();
        let _ = writeln!(
            buf,
            "  [{}] {tid}  {}  {excerpt}",
            ev.created_at,
            ev.kind.as_str()
        );
    }
    let _ = writeln!(
        buf,
        "  help: Enter/r=refresh | q=quit | d/t/h/tail <id> | g=dag | approve/rework/steer/undo/unblock/unfreeze"
    );
    buf
}

fn show_task_details(ro: &RoDbCache, id: &str, w: &mut dyn Write) -> Result<(), CmdError> {
    if id.is_empty() {
        return wln(w, "usage: d <id>");
    }
    let Some((db, task)) = load_ro_task(ro, id, "task", w)? else {
        return Ok(());
    };
    let mut buf = String::new();
    let (id, pr, st, cy) = (
        san(&task.id),
        task.priority.as_str(),
        task.state.as_str(),
        task.unblock_cycles,
    );
    let _ = writeln!(buf, "== task {id} | {pr} | {st} | cycles={cy} ==");
    let _ = writeln!(buf, "  title: {}", san(&task.title));
    let _ = writeln!(buf, "  spec: {}", san(&task.spec.description));
    for a in &task.spec.acceptance {
        let _ = writeln!(buf, "  accept: {}", san(a));
    }
    if !task.scope_paths.is_empty() {
        let scopes: Vec<String> = task.scope_paths.iter().map(|p| san(p)).collect();
        let _ = writeln!(buf, "  scope_paths: {}", scopes.join(", "));
    }
    if let Some(reason) = &task.blocked_reason {
        let _ = writeln!(buf, "  blocked_reason: {}", san(&format_blocked(reason)));
    }
    for dep in db.deps_of(&task.id).unwrap_or_default() {
        let st = db.get_task(&dep).map_or("unknown", |t| t.state.as_str());
        let _ = writeln!(buf, "  dep: {} ({st})", san(&dep));
    }
    for note in &task.context_notes {
        let _ = writeln!(buf, "  note: {}", san(note));
    }
    w.write_all(buf.as_bytes()).map_err(|e| io_err("write", e))
}

fn show_task_events(ro: &RoDbCache, id: &str, w: &mut dyn Write) -> Result<(), CmdError> {
    if id.is_empty() {
        return wln(w, "usage: t <id>");
    }
    let Some((db, _)) = load_ro_task(ro, id, "events", w)? else {
        return Ok(());
    };
    let events = match db.events_for_task(id, TASK_EVENTS_LIMIT) {
        Ok(e) => e,
        Err(e) => return wln(w, &format!("events: {}", san(&e.to_string()))),
    };
    let mut buf = format!("== events for {} ==\n", san(id));
    if events.is_empty() {
        buf.push_str("  (no events)\n");
    }
    for ev in &events {
        let _ = writeln!(
            buf,
            "  [{}] {} {}",
            ev.created_at,
            ev.kind.as_str(),
            san(&ev.payload_json)
        );
    }
    w.write_all(buf.as_bytes()).map_err(|e| io_err("write", e))
}

fn show_task_handover(
    ctx: &Ctx,
    ro: &RoDbCache,
    id: &str,
    w: &mut dyn Write,
) -> Result<(), CmdError> {
    if id.is_empty() {
        return wln(w, "usage: h <id>");
    }
    let Some((db, task)) = load_ro_task(ro, id, "handover", w)? else {
        return Ok(());
    };
    // Same lookup as the handover command, web UI and Telegram. This used to
    // skip the current-repository fallback, so a task whose repo_root was gone
    // showed default-config readiness here (e.g. ignoring `proof.required`).
    let repo_cfg = load_repo_config(ctx, Path::new(&task.repo_root));
    match load_handover(&db, &ctx.data_dir, id, &repo_cfg) {
        Ok(data) => {
            for line in render_markdown(&data).lines() {
                wln(w, &san(line))?;
            }
            Ok(())
        }
        Err(e) => wln(w, &format!("handover: {}", san(&e))),
    }
}

fn show_dag(ro: &RoDbCache, repo_root: Option<&Path>, w: &mut dyn Write) -> Result<(), CmdError> {
    let Some(db) = open_db_ro(ro)? else {
        return wln(w, "== DAG ==\n  (no tasks)");
    };
    let repo_str = repo_root.map(|p| p.to_string_lossy().into_owned());
    let tasks = db
        .list_tasks(repo_str.as_deref(), None)
        .map_err(|e| io_err("dag", e))?;
    if tasks.is_empty() {
        return wln(w, "== DAG ==\n  (no tasks)");
    }
    let id_set: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
    let mut edges: Vec<(String, String)> = db
        .all_deps()
        .map_err(|e| io_err("dag", e))?
        .into_iter()
        .filter(|(t, b)| id_set.contains(t.as_str()) && id_set.contains(b.as_str()))
        .collect();
    edges.sort();
    edges.dedup();
    let node_ids: Vec<String> = tasks.iter().map(|t| t.id.clone()).collect();
    let topo = dag::topo_order(&node_ids, &edges).unwrap_or(node_ids);
    let crit = compute_critical_path(&topo, &edges);
    let crit_set: HashSet<&str> = crit.iter().map(String::as_str).collect();
    let active: Vec<Task> = tasks
        .iter()
        .filter(|t| !t.state.is_terminal() && t.state != TaskState::Blocked)
        .cloned()
        .collect();
    let blocked_up = blocked_upstream_tasks(&active, &tasks, &edges);
    let blocked_map: HashMap<&str, &str> = blocked_up
        .iter()
        .map(|(t, b)| (t.as_str(), b.as_str()))
        .collect();
    let by_id: HashMap<&str, &Task> = tasks.iter().map(|t| (t.id.as_str(), t)).collect();
    let mut blockers_of: HashMap<&str, Vec<&str>> = HashMap::new();
    for (t, b) in &edges {
        blockers_of.entry(t.as_str()).or_default().push(b.as_str());
    }
    let mut buf = String::from("== DAG ==\n");
    for id in &topo {
        let Some(t) = by_id.get(id.as_str()) else {
            continue;
        };
        let deps = blockers_of.get(id.as_str()).cloned().unwrap_or_default();
        let dep_str = if deps.is_empty() {
            String::new()
        } else {
            format!(
                " <- {}",
                deps.iter().map(|d| san(d)).collect::<Vec<_>>().join(", ")
            )
        };
        let crit_tag = if crit_set.contains(id.as_str()) {
            " [CRIT]"
        } else {
            ""
        };
        let blk_tag = blocked_map.get(id.as_str()).map_or(String::new(), |b| {
            format!(" [CASCADE-BLOCKED by {}]", san(b))
        });
        let (tid, pr, st, tt) = (
            san(&t.id),
            t.priority.as_str(),
            t.state.as_str(),
            san(&t.title),
        );
        let _ = writeln!(
            buf,
            "  {tid}  {pr}  {st:<8}  {tt}{dep_str}{crit_tag}{blk_tag}"
        );
    }
    if !crit.is_empty() {
        let clean: Vec<String> = crit.iter().map(|c| san(c)).collect();
        let _ = writeln!(buf, "  critical path: {}", clean.join(" -> "));
    }
    w.write_all(buf.as_bytes()).map_err(|e| io_err("write", e))
}
