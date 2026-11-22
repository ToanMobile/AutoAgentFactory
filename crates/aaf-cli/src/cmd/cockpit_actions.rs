//! Mutating/approval actions and interactive sub-prompts for `aaf cockpit`.
//!
//! Every mutating action requires explicit `y/N` confirmation (default NO on
//! empty input or EOF) before opening a read-write DB handle or mutating state.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, ErrorKind, Write};
use std::path::{Path, PathBuf};

use aaf_domain::blocked::BlockedReason;
use aaf_domain::budget::{BudgetCheck, RootBudget};
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::gate::ChangeKind;
use aaf_domain::ports::{BudgetStore, Clock, EventSink, PortError, RunStore, TaskStore};
use aaf_domain::task::{Task, TaskState};
use aaf_engine::db::{Db, TurnSnapshot, private};
use aaf_engine::executors::stream_log::read_tail_lines;
use aaf_engine::factory::adapters::SystemClock;
use aaf_engine::factory::prompt::PLAN_APPROVED_PREFIX;
use aaf_engine::factory::snapshots::{preview_latest_undo, steer_task, undo_latest_turn};
use aaf_engine::factory::unfreeze;
use aaf_engine::gate::{GIT_TIMEOUT, GateError, base, head_lock};
use aaf_engine::git::worktree::{validate_task_id, worktree_path};
use aaf_engine::watch::sanitize_display as san;
use serde_json::{Value, json};

use crate::ctx::Ctx;
use crate::output::CmdError;

pub(crate) const DB_FILE: &str = "aaf.db";
const PLAN_APPROVAL_PREFIX: &str = "plan approval required:";
const PLAN_EVENT_LOOKBACK: u32 = 100;
const TAIL_MAX_LINES: usize = 50;

pub(crate) fn io_err(context: &str, err: impl std::fmt::Display) -> CmdError {
    CmdError::new(format!("{context}: {err}"))
}

pub(crate) fn wln(w: &mut dyn Write, line: &str) -> Result<(), CmdError> {
    writeln!(w, "{line}").map_err(|e| io_err("write", e))
}

pub(crate) fn open_db_ro(ctx: &Ctx) -> Result<Option<Db>, CmdError> {
    let path = ctx.data_dir.join(DB_FILE);
    if !path.is_file() {
        return Ok(None);
    }
    Db::open_read_only(&path)
        .map(Some)
        .map_err(|e| io_err("database", e))
}

fn open_db_rw(ctx: &Ctx) -> Result<Db, CmdError> {
    private::create_dir(&ctx.data_dir)
        .map_err(|e| io_err(&format!("data dir {}", ctx.data_dir.display()), e))?;
    Db::open(&ctx.data_dir.join(DB_FILE)).map_err(|e| io_err("database", e))
}

/// Prompts for explicit confirmation, defaulting to `false` on empty input,
/// EOF, or anything other than `y` / `yes` (case-insensitive).
pub(crate) fn confirm(
    prompt: &str,
    reader: &mut dyn BufRead,
    writer: &mut dyn Write,
) -> Result<bool, CmdError> {
    write!(writer, "{prompt}").map_err(|e| io_err("write", e))?;
    writer.flush().map_err(|e| io_err("flush", e))?;
    let mut answer = String::new();
    if reader
        .read_line(&mut answer)
        .map_err(|e| io_err("read", e))?
        == 0
    {
        return Ok(false);
    }
    let t = answer.trim();
    Ok(t.eq_ignore_ascii_case("y") || t.eq_ignore_ascii_case("yes"))
}

pub(crate) fn load_ro_task(
    ctx: &Ctx,
    id: &str,
    action: &str,
    w: &mut dyn Write,
) -> Result<Option<(Db, Task)>, CmdError> {
    if let Err(e) = validate_task_id(id) {
        wln(w, &format!("invalid task id: {}", san(&e.to_string())))?;
        return Ok(None);
    }
    let Some(db) = open_db_ro(ctx)? else {
        wln(w, "database not found")?;
        return Ok(None);
    };
    match db.get_task(id) {
        Ok(task) => Ok(Some((db, task))),
        Err(e) => {
            wln(w, &format!("{action}: {}", san(&e.to_string())))?;
            Ok(None)
        }
    }
}

fn change_code(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "A",
        ChangeKind::Modified => "M",
        ChangeKind::Deleted => "D",
        ChangeKind::Renamed => "R",
    }
}

fn resolve_task_tree(db: &Db, task: &Task, ctx: &Ctx) -> PathBuf {
    if let Ok(wt) = worktree_path(Path::new(&task.repo_root), &ctx.worktrees_dir, &task.id)
        && wt.is_dir()
    {
        return wt;
    }
    if let Ok(runs) = db.runs_for_task(&task.id)
        && let Some(path) = runs
            .iter()
            .rev()
            .map(|r| PathBuf::from(&r.worktree_path))
            .find(|p| !p.as_os_str().is_empty() && p.is_dir())
    {
        return path;
    }
    PathBuf::from(&task.repo_root)
}

pub(crate) fn run_tail_view(
    ctx: &Ctx,
    id: &str,
    reader: &mut dyn BufRead,
    w: &mut dyn Write,
) -> Result<(), CmdError> {
    if id.is_empty() {
        return wln(w, "usage: tail <id>");
    }
    let Some((db, _)) = load_ro_task(ctx, id, "tail", w)? else {
        return Ok(());
    };
    let runs = match db.runs_for_task(id) {
        Ok(r) => r,
        Err(e) => return wln(w, &format!("tail: {}", san(&e.to_string()))),
    };
    drop(db);
    let Some(latest) = runs
        .iter()
        .max_by_key(|r| (r.started_at, r.id.as_str()))
        .map(|r| r.id.clone())
    else {
        return wln(w, &format!("no runs found for {}", san(id)));
    };
    let (safe_id, safe_run) = (san(id), san(&latest));
    loop {
        wln(w, &format!("== tail {safe_id} (run {safe_run}) =="))?;
        match read_tail_lines(&ctx.data_dir, &latest, TAIL_MAX_LINES) {
            Ok(lines) if lines.is_empty() => wln(w, "  (no stream lines)")?,
            Ok(lines) => {
                for l in &lines {
                    wln(w, &format!("  {}", san(l)))?;
                }
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                wln(w, &format!("  (no stream log for run {safe_run})"))?;
            }
            Err(e) => wln(w, &format!("  stream error: {}", san(&e.to_string())))?,
        }
        write!(w, "tail (Enter=refresh, q=back)> ").map_err(|e| io_err("write", e))?;
        w.flush().map_err(|e| io_err("flush", e))?;
        let mut line = String::new();
        let n = reader.read_line(&mut line).map_err(|e| io_err("read", e))?;
        if n == 0 || matches!(line.trim(), "q" | "quit") {
            break;
        }
    }
    Ok(())
}

pub(crate) fn action_approve(
    ctx: &Ctx,
    id: &str,
    reviewer_arg: Option<&str>,
    reader: &mut dyn BufRead,
    w: &mut dyn Write,
) -> Result<(), CmdError> {
    let Some((db, task)) = load_ro_task(ctx, id, "approve", w)? else {
        return Ok(());
    };
    let target_tree = resolve_task_tree(&db, &task, ctx);
    drop(db);

    let cfg = match ctx.load_config(&target_tree) {
        Ok(c) => c,
        Err(e) => return wln(w, &format!("approve config: {}", san(&e.message))),
    };
    let base_info = match base::resolve(&target_tree, &cfg.repo.gate.base_ref, GIT_TIMEOUT) {
        Ok(b) => b,
        Err(e) => return wln(w, &format!("approve base: {}", san(&e.to_string()))),
    };
    let report = head_lock::evaluate(
        &ctx.data_dir,
        &target_tree,
        &cfg.repo,
        &base_info.diff_target,
    );
    let safe_id = san(id);
    if report.locked_changes.is_empty() {
        return wln(
            w,
            &format!("no locked file is changed for {safe_id}; nothing to approve"),
        );
    }
    wln(
        w,
        &format!(
            "locked changes for {safe_id} (lock_fp={}):",
            san(&report.lock_fp)
        ),
    )?;
    for ch in &report.locked_changes {
        wln(
            w,
            &format!("  {} {}", change_code(ch.change), san(&ch.path)),
        )?;
    }
    if !confirm(&format!("Confirm approve {safe_id} [y/N]: "), reader, w)? {
        return wln(w, "cancelled");
    }
    let reviewer = reviewer_arg
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("cockpit");
    match head_lock::approve(
        &ctx.data_dir,
        &target_tree,
        &cfg.repo,
        reviewer,
        false,
        &SystemClock,
    ) {
        Ok(a) => wln(
            w,
            &format!(
                "approved {safe_id} by {} (lock_fp={})",
                san(&a.reviewer),
                san(&a.lock_fp)
            ),
        ),
        Err(GateError::NothingLocked) => wln(w, "no locked file is changed; nothing approved"),
        Err(e) => wln(w, &format!("approve failed: {}", san(&e.to_string()))),
    }
}

fn reset_exhausted_budget(db: &Db, root_id: &str) -> Result<bool, PortError> {
    match db.get_budget(root_id) {
        Ok(budget) if budget.check() != BudgetCheck::Ok => {
            db.put_budget(&RootBudget {
                tokens_used: 0,
                wall_secs_used: 0,
                cycles_used: 0,
                frozen: false,
                ..budget
            })?;
            Ok(true)
        }
        Ok(_) | Err(PortError::NotFound(_)) => Ok(false),
        Err(other) => Err(other),
    }
}

fn clear_stale_receipts(db: &Db, task: &Task, ctx: &Ctx) {
    let mut roots = vec![PathBuf::from(&task.repo_root)];
    if let Ok(wt) = worktree_path(Path::new(&task.repo_root), &ctx.worktrees_dir, &task.id) {
        roots.push(wt);
    }
    if let Ok(runs) = db.runs_for_task(&task.id) {
        for run in runs {
            if !run.worktree_path.is_empty() {
                roots.push(PathBuf::from(run.worktree_path));
            }
        }
    }
    for root in roots {
        let _ = fs::remove_file(aaf_fp::receipt_path(&ctx.data_dir, &root));
        let _ = fs::remove_dir_all(ctx.data_dir.join("approvals").join(aaf_fp::repo_id(&root)));
    }
}

pub(crate) fn action_rework(
    ctx: &Ctx,
    id: &str,
    rest: &str,
    reader: &mut dyn BufRead,
    w: &mut dyn Write,
) -> Result<(), CmdError> {
    let (rerun, reason) = if let Some(after) = rest.strip_prefix("--rerun")
        && (after.is_empty() || after.starts_with(char::is_whitespace))
    {
        (true, after.trim())
    } else {
        (false, rest.trim())
    };
    if reason.is_empty() {
        return wln(w, "usage: rework <id> [--rerun] <reason>");
    }
    let Some((db_ro, task)) = load_ro_task(ctx, id, "rework", w)? else {
        return Ok(());
    };
    drop(db_ro);
    let safe_id = san(id);
    if task.state != TaskState::Review {
        return wln(
            w,
            &format!("refused: {safe_id} is {}, not review", task.state.as_str()),
        );
    }
    let target = if rerun {
        TaskState::Ready
    } else {
        TaskState::Gating
    };
    wln(
        w,
        &format!("rework {safe_id} -> {}: {}", target.as_str(), san(reason)),
    )?;
    if !confirm(&format!("Confirm rework {safe_id} [y/N]: "), reader, w)? {
        return wln(w, "cancelled");
    }

    let db = open_db_rw(ctx)?;
    let fresh = db.get_task(id).map_err(|e| io_err("rework", e))?;
    if fresh.state != TaskState::Review {
        return wln(
            w,
            &format!("refused: {safe_id} is {}, not review", fresh.state.as_str()),
        );
    }
    let now = SystemClock.now();
    let budget_reset =
        reset_exhausted_budget(&db, &fresh.root_id).map_err(|e| io_err("budget", e))?;
    clear_stale_receipts(&db, &fresh, ctx);
    let note = format!(
        "rework: {}",
        reason.split_whitespace().collect::<Vec<_>>().join(" ")
    );
    db.append_context_note(id, &note, now)
        .map_err(|e| io_err("rework", e))?;
    db.set_state(id, target, None, now)
        .map_err(|e| io_err("rework", e))?;
    let p1 = json!({ "reason": reason, "to": target.as_str(), "rerun": rerun, "budget_reset": budget_reset, "by": "human" });
    db.emit(&FactoryEvent::new(EventKind::ReworkRequested, p1.to_string(), now).for_task(id))
        .map_err(|e| io_err("rework", e))?;
    let p2 = json!({ "from": TaskState::Review.as_str(), "to": target.as_str(), "why": "aaf cockpit rework", "by": "human" });
    db.emit(&FactoryEvent::new(EventKind::TaskStateChanged, p2.to_string(), now).for_task(id))
        .map_err(|e| io_err("rework", e))?;
    wln(w, &format!("{safe_id} sent back to {}", target.as_str()))
}

pub(crate) fn action_steer(
    ctx: &Ctx,
    id: &str,
    note: &str,
    reader: &mut dyn BufRead,
    w: &mut dyn Write,
) -> Result<(), CmdError> {
    if let Err(e) = validate_task_id(id) {
        return wln(w, &format!("invalid task id: {}", san(&e.to_string())));
    }
    let note = note.trim();
    if note.is_empty() {
        return wln(w, "usage: steer <id> <note>");
    }
    let safe_id = san(id);
    wln(w, &format!("steer {safe_id}: {}", san(note)))?;
    if !confirm(&format!("Confirm steer {safe_id} [y/N]: "), reader, w)? {
        return wln(w, "cancelled");
    }
    let db = open_db_rw(ctx)?;
    match steer_task(&db, id, note, SystemClock.now()) {
        Ok(recorded) => wln(w, &format!("steered {safe_id}: {}", san(&recorded))),
        Err(e) => wln(w, &format!("steer failed: {}", san(&e))),
    }
}

pub(crate) fn action_undo(
    ctx: &Ctx,
    id: &str,
    reader: &mut dyn BufRead,
    w: &mut dyn Write,
) -> Result<(), CmdError> {
    let Some((db_ro, task)) = load_ro_task(ctx, id, "undo", w)? else {
        return Ok(());
    };
    let snap: TurnSnapshot = match preview_latest_undo(&db_ro, Path::new(&task.repo_root), id) {
        Ok(s) => s,
        Err(e) => return wln(w, &format!("undo preview failed: {}", san(&e))),
    };
    drop(db_ro);

    let (safe_id, safe_oid, safe_ref) = (san(id), san(&snap.commit_oid), san(&snap.ref_name));
    wln(
        w,
        &format!("undo preview for {safe_id}:\n  ref: {safe_ref}\n  commit: {safe_oid}"),
    )?;
    if !confirm(
        &format!("Confirm undo {safe_id} to {safe_oid} [y/N]: "),
        reader,
        w,
    )? {
        return wln(w, "cancelled");
    }
    let db = open_db_rw(ctx)?;
    match undo_latest_turn(
        &db,
        &ctx.data_dir,
        Path::new(&task.repo_root),
        &ctx.worktrees_dir,
        id,
        SystemClock.now(),
    ) {
        Ok(u) => wln(
            w,
            &format!(
                "undid {safe_id} to {} ({})",
                san(&u.commit_oid),
                san(&u.ref_name)
            ),
        ),
        Err(e) => wln(w, &format!("undo failed: {}", san(&e))),
    }
}

fn latest_generated_plan(db: &Db, task_id: &str) -> Result<Option<String>, PortError> {
    for event in db.events_for_task(task_id, PLAN_EVENT_LOOKBACK)? {
        if event.kind != EventKind::PlanGenerated {
            continue;
        }
        if let Ok(payload) = serde_json::from_str::<Value>(&event.payload_json)
            && let Some(plan) = payload["plan"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
        {
            return Ok(Some(plan.to_string()));
        }
    }
    Ok(None)
}

fn maybe_approve_on_unblock(db: &Db, task: &Task, now: i64) -> Result<bool, PortError> {
    let Some(BlockedReason::NeedsHuman { why }) = &task.blocked_reason else {
        return Ok(false);
    };
    if !why.starts_with(PLAN_APPROVAL_PREFIX) {
        return Ok(false);
    }
    let plan = match latest_generated_plan(db, &task.id)? {
        Some(plan) => plan,
        None => why
            .strip_prefix(PLAN_APPROVAL_PREFIX)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(why.as_str())
            .to_string(),
    };
    db.append_context_note(&task.id, &format!("{PLAN_APPROVED_PREFIX} {plan}"), now)?;
    Ok(true)
}

pub(crate) fn action_unblock(
    ctx: &Ctx,
    id: &str,
    reader: &mut dyn BufRead,
    w: &mut dyn Write,
) -> Result<(), CmdError> {
    let Some((db_ro, task)) = load_ro_task(ctx, id, "unblock", w)? else {
        return Ok(());
    };
    drop(db_ro);
    let safe_id = san(id);
    if task.state != TaskState::Blocked {
        return wln(
            w,
            &format!("refused: {safe_id} is {}, not blocked", task.state.as_str()),
        );
    }
    if !confirm(&format!("Confirm unblock {safe_id} [y/N]: "), reader, w)? {
        return wln(w, "cancelled");
    }
    let db = open_db_rw(ctx)?;
    let fresh = db.get_task(id).map_err(|e| io_err("unblock", e))?;
    if fresh.state != TaskState::Blocked {
        return wln(
            w,
            &format!(
                "refused: {safe_id} is {}, not blocked",
                fresh.state.as_str()
            ),
        );
    }
    let now = SystemClock.now();
    let budget_reset =
        reset_exhausted_budget(&db, &fresh.root_id).map_err(|e| io_err("budget", e))?;
    let plan_approved =
        maybe_approve_on_unblock(&db, &fresh, now).map_err(|e| io_err("unblock", e))?;
    db.set_state(id, TaskState::Ready, None, now)
        .map_err(|e| io_err("unblock", e))?;
    let payload = json!({
        "to": "ready", "why": "aaf cockpit unblock", "by": "human",
        "was": fresh.blocked_reason, "budget_reset": budget_reset, "plan_approved": plan_approved,
    });
    db.emit(&FactoryEvent::new(EventKind::TaskStateChanged, payload.to_string(), now).for_task(id))
        .map_err(|e| io_err("unblock", e))?;
    wln(w, &format!("{safe_id} is ready"))
}

pub(crate) fn action_unfreeze(
    ctx: &Ctx,
    reader: &mut dyn BufRead,
    w: &mut dyn Write,
) -> Result<(), CmdError> {
    if !confirm("Confirm unfreeze factory [y/N]: ", reader, w)? {
        return wln(w, "cancelled");
    }
    let db = open_db_rw(ctx)?;
    unfreeze(&db, &SystemClock).map_err(|e| io_err("unfreeze", e))?;
    wln(w, "factory is not frozen")
}

/// Longest directed chain (`blocker -> ... -> dependent`) in a DAG.
pub(crate) fn compute_critical_path(topo: &[String], edges: &[(String, String)]) -> Vec<String> {
    if edges.is_empty() {
        return Vec::new();
    }
    let mut blockers: HashMap<&str, Vec<&str>> = HashMap::new();
    for (task, blocked_by) in edges {
        blockers
            .entry(task.as_str())
            .or_default()
            .push(blocked_by.as_str());
    }
    let mut best_len: HashMap<&str, usize> = HashMap::with_capacity(topo.len());
    let mut pred: HashMap<&str, &str> = HashMap::new();
    let (mut best_end, mut global_max) = (None, 0usize);
    for node in topo {
        let (mut len, mut prev) = (1usize, None);
        for &dep in blockers.get(node.as_str()).into_iter().flatten() {
            let cand = best_len.get(dep).copied().unwrap_or(0).saturating_add(1);
            if cand > len {
                (len, prev) = (cand, Some(dep));
            }
        }
        best_len.insert(node.as_str(), len);
        if let Some(p) = prev {
            pred.insert(node.as_str(), p);
        }
        if len > global_max {
            (global_max, best_end) = (len, Some(node.as_str()));
        }
    }
    if global_max < 2 {
        return Vec::new();
    }
    let (mut path, mut seen, mut cur) = (Vec::with_capacity(global_max), HashSet::new(), best_end);
    while let Some(node) = cur {
        if !seen.insert(node) {
            break;
        }
        path.push(node.to_string());
        cur = pred.get(node).copied();
    }
    path.reverse();
    path
}
