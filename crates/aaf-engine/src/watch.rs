//! 100% read-only live terminal dashboard snapshot and renderer (`aaf watch`).
//!
//! Opens zero network ports, acquires zero exclusive locks, never mutates the
//! database, and never creates `receipt.key` or any file in `AAF_DATA_DIR`.

use std::fmt::Write as _;
use std::fs::{self, File, TryLockError};
use std::path::Path;

use aaf_domain::account::{AccountProfile, AccountState};
use aaf_domain::blocked::BlockedReason;
use aaf_domain::config::AccountSelection;
use aaf_domain::gate::{GateMode, GateStatus, Severity};
use aaf_domain::ports::{
    AccountStore, KvStore, META_CONSECUTIVE_ROOT_FAILS, META_FACTORY_FROZEN, PortResult, RunStore,
    TaskStore,
};
use aaf_domain::task::{Task, TaskState};
use serde::Serialize;

use crate::accounts::selection;
use crate::db::Db;
use crate::factory::adapters::provider_for;
use crate::telemetry::TaskCounts;
use crate::throttle::paths::{drain_path, guest_record_path, throttle_dir};
use crate::throttle::probe::{self, GroupState};
use crate::throttle::stats::stats_path;
use crate::throttle::{LoadProbe, Pressure, SystemProbe};

const FIVE_HOURS_SECS: i64 = 5 * 3600;
const MAX_READ_BYTES: u64 = 1 << 20;
const BYTES_PER_MB: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WatchSnapshot {
    pub scope: String,
    pub repo_root: Option<String>,
    pub now: i64,
    pub frozen: bool,
    pub consecutive_root_fails: u32,
    pub tasks: WatchTasks,
    pub heavy_slots: WatchHeavySlots,
    pub accounts: WatchAccounts,
    pub receipt: Option<WatchReceipt>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct WatchTasks {
    pub counts: TaskCounts,
    pub active: Vec<WatchTask>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WatchTask {
    pub id: String,
    pub state: TaskState,
    pub priority: String,
    pub unblock_cycles: u32,
    pub blocked_reason: Option<String>,
    pub title: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct WatchHeavySlots {
    pub drain_active: bool,
    pub memory_pressure: String,
    pub measured_commands: usize,
    pub slots: Vec<WatchSlot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WatchSlot {
    pub slot: usize,
    pub state: String,
    pub pgid: Option<u32>,
    pub label: Option<String>,
    pub reserved_ram_mb: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct WatchAccounts {
    pub selection_mode: String,
    pub pinned_account: Option<String>,
    pub entries: Vec<WatchAccount>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WatchAccount {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub state: String,
    pub cooldown_until: Option<i64>,
    pub tokens_5h: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WatchReceipt {
    pub status: GateStatus,
    pub mode: GateMode,
    pub tree_fp_short: String,
    pub warnings: usize,
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy)]
pub struct WatchQuery<'a> {
    pub scope: &'a str,
    pub repo_root: Option<&'a Path>,
    pub data_dir: &'a Path,
    pub selection: AccountSelection,
    pub executor: &'a str,
    pub now: i64,
}

/// Collects a 100% read-only snapshot across tasks, heavy slots, accounts, and receipt.
pub fn collect_watch_snapshot(
    query: &WatchQuery<'_>,
    db: Option<&Db>,
) -> PortResult<WatchSnapshot> {
    let repo_str = query.repo_root.map(|p| p.to_string_lossy().into_owned());
    let (frozen, fails, all_tasks, states) = match db {
        Some(db) => {
            let frz = db.get_meta(META_FACTORY_FROZEN)?.as_deref() == Some("1");
            let fl = db.get_meta(META_CONSECUTIVE_ROOT_FAILS)?;
            let fl = fl.and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            let ts = db.list_tasks(repo_str.as_deref(), None)?;
            (frz, fl, ts, db.list_accounts()?)
        }
        None => (false, 0, Vec::new(), Vec::new()),
    };
    let pressure = SystemProbe::detect().pressure();
    Ok(WatchSnapshot {
        scope: query.scope.to_string(),
        repo_root: repo_str,
        now: query.now,
        frozen,
        consecutive_root_fails: fails,
        tasks: build_tasks(&all_tasks),
        heavy_slots: inspect_heavy_slots(query.data_dir, pressure),
        accounts: build_accounts(query, db, &states)?,
        receipt: query
            .repo_root
            .and_then(|r| read_receipt(query.data_dir, r)),
    })
}

fn build_tasks(all_tasks: &[Task]) -> WatchTasks {
    let (mut counts, mut active) = (TaskCounts::default(), Vec::new());
    for t in all_tasks {
        counts.total = counts.total.saturating_add(1);
        let bucket = match t.state {
            TaskState::Backlog => &mut counts.backlog,
            TaskState::Ready => &mut counts.ready,
            TaskState::Running => &mut counts.running,
            TaskState::Gating => &mut counts.gating,
            TaskState::Review => &mut counts.review,
            TaskState::Blocked => &mut counts.blocked,
            TaskState::Done => &mut counts.done,
            TaskState::Cancelled => &mut counts.cancelled,
        };
        *bucket = bucket.saturating_add(1);
        if !t.state.is_terminal() {
            active.push(WatchTask {
                id: t.id.clone(),
                state: t.state,
                priority: t.priority.as_str().to_string(),
                unblock_cycles: t.unblock_cycles,
                blocked_reason: t.blocked_reason.as_ref().map(format_blocked),
                title: t.title.clone(),
            });
        }
    }
    WatchTasks { counts, active }
}

fn format_blocked(reason: &BlockedReason) -> String {
    let detail = match reason {
        BlockedReason::Quota { reset_at: Some(ts) } => return format!("quota (reset_at={ts})"),
        BlockedReason::Quota { reset_at: None } => return "quota".to_string(),
        BlockedReason::Dependency { task_id } => task_id,
        BlockedReason::GateFailed { summary } => summary,
        BlockedReason::NeedsHuman { why } => why,
        BlockedReason::MergeConflict { detail }
        | BlockedReason::FlakyTest { detail }
        | BlockedReason::Stuck { detail }
        | BlockedReason::BudgetExceeded { detail }
        | BlockedReason::Infra { detail } => detail,
    };
    format!("{}: {detail}", reason.kind())
}

fn build_accounts(
    q: &WatchQuery<'_>,
    db: Option<&Db>,
    states: &[AccountState],
) -> PortResult<WatchAccounts> {
    let pinned_account = if q.selection != AccountSelection::LeastUsed
        && let (Some(db), Some(root), Some(prov)) = (db, q.repo_root, provider_for(q.executor))
    {
        let profs: Vec<AccountProfile> = states
            .iter()
            .filter(|s| s.profile.provider == prov)
            .map(|s| s.profile.clone())
            .collect();
        selection::resolve(root, prov, &profs, db)?.map(|b| b.account.id)
    } else {
        None
    };
    let leases = q.data_dir.join("leases");
    let since = q.now.saturating_sub(FIVE_HOURS_SECS);
    let mut entries = Vec::with_capacity(states.len());
    for a in states {
        let cd = a.in_cooldown(q.now);
        let st = if cd {
            "cooldown"
        } else if is_account_leased(&leases, &a.profile.id) {
            "leased"
        } else {
            "available"
        };
        let tokens_5h = match db {
            Some(db) => db.tokens_since(&a.profile.id, since)?,
            None => 0,
        };
        entries.push(WatchAccount {
            id: a.profile.id.clone(),
            name: a.profile.name.clone(),
            provider: a.profile.provider.as_str().to_string(),
            state: st.to_string(),
            cooldown_until: cd.then_some(a.cooldown_until).flatten(),
            tokens_5h,
        });
    }
    Ok(WatchAccounts {
        selection_mode: q.selection.as_str().to_string(),
        pinned_account,
        entries,
    })
}

fn inspect_heavy_slots(data_dir: &Path, pressure: Option<Pressure>) -> WatchHeavySlots {
    let mp = match pressure {
        Some(Pressure::Normal) => "normal",
        Some(Pressure::Warn) => "warn",
        Some(Pressure::Critical) => "critical",
        None => "unknown",
    };
    let stats = read_plain(&stats_path(data_dir));
    let val = serde_json::from_str::<serde_json::Value>(&stats).ok();
    let entries_obj = val.as_ref().and_then(|v| v.get("entries")?.as_object());
    let measured_commands = entries_obj.map_or(0, |m| {
        m.values()
            .filter(|v| v.as_array().is_some_and(|a| !a.is_empty()))
            .count()
    });
    let mut slots = Vec::new();
    let mut ps_groups: Option<Option<String>> = None;
    if let Ok(rd) = fs::read_dir(throttle_dir(data_dir)) {
        for e in rd.flatten() {
            let name = e.file_name();
            let Some(n) = name.to_str().and_then(parse_slot_idx) else {
                continue;
            };
            let locked = is_locked(&e.path());
            let mut text = read_plain(&e.path());
            if locked && text.trim().is_empty() {
                text = read_plain(&guest_record_path(data_dir, n));
            }
            let rec = parse_slot_record(&text);
            let (st, keep) = match (locked, rec.pgid) {
                (true, _) => ("busy", true),
                (false, Some(pgid)) => {
                    let ps_out = ps_groups.get_or_insert_with(|| {
                        probe::tool_output(probe::PS, &["-A", "-o", probe::PS_GROUP_COLUMNS])
                    });
                    let alive = ps_out
                        .as_deref()
                        .and_then(|out| probe::group_state(out, pgid))
                        .is_some_and(|gs| is_live_recorded_group(rec.lstart.as_deref(), &gs));
                    if alive {
                        ("orphan", true)
                    } else {
                        ("stale", true)
                    }
                }
                (false, None) => ("free", false),
            };
            slots.push(WatchSlot {
                slot: n,
                state: st.to_string(),
                pgid: keep.then_some(rec.pgid).flatten(),
                label: keep.then_some(rec.label).flatten(),
                reserved_ram_mb: keep.then_some(rec.ram_mb).flatten(),
            });
        }
    }
    slots.sort_by_key(|s| s.slot);
    WatchHeavySlots {
        drain_active: is_locked(&drain_path(data_dir)),
        memory_pressure: mp.to_string(),
        measured_commands,
        slots,
    }
}

struct SlotRecord {
    pgid: Option<u32>,
    label: Option<String>,
    ram_mb: Option<u64>,
    lstart: Option<String>,
}

fn is_live_recorded_group(recorded_start: Option<&str>, state: &GroupState) -> bool {
    match state.leader_started.as_deref() {
        Some(now) => recorded_start.is_none_or(|recorded| recorded == now),
        None => true,
    }
}

fn parse_slot_idx(name: &str) -> Option<usize> {
    name.strip_prefix("heavy.")?
        .strip_suffix(".lock")?
        .parse()
        .ok()
}

fn parse_slot_record(text: &str) -> SlotRecord {
    let (mut pgid, mut label, mut reserved, mut ram_mb, mut lstart) =
        (None, None, None, None, None);
    for (k, v) in text.lines().filter_map(|l| l.split_once('=')) {
        let v = v.trim();
        match k.trim() {
            "pgid" => pgid = v.parse::<u32>().ok().filter(|p| *p > 1),
            "label" if !v.is_empty() => label = Some(v.to_string()),
            "reserved" => reserved = v.parse::<u64>().ok(),
            "ram_mb" => ram_mb = v.parse::<u64>().ok(),
            "lstart" if !v.is_empty() => lstart = Some(v.to_string()),
            _ => {}
        }
    }
    SlotRecord {
        pgid,
        label,
        ram_mb: reserved.map(|b| b.div_ceil(BYTES_PER_MB)).or(ram_mb),
        lstart,
    }
}

fn read_receipt(data_dir: &Path, repo_root: &Path) -> Option<WatchReceipt> {
    let key = aaf_fp::load_key(data_dir).ok()?;
    let r = aaf_fp::read_receipt(data_dir, &key, repo_root).ok()?;
    let raw = r.tree_fp.strip_prefix("git-tree:").unwrap_or(&r.tree_fp);
    let sw = r
        .static_findings
        .iter()
        .filter(|f| f.severity == Severity::Warn)
        .count();
    let rw = r
        .reasons
        .iter()
        .filter(|s| s.starts_with("warning: "))
        .count();
    let cw = usize::try_from(r.store_compliance.warnings).unwrap_or(usize::MAX);
    Some(WatchReceipt {
        status: r.status,
        mode: r.mode,
        tree_fp_short: raw.chars().take(12).collect(),
        warnings: sw.saturating_add(rw).saturating_add(cw),
        created_at: r.created_at,
    })
}

fn is_locked(path: &Path) -> bool {
    let ok = fs::symlink_metadata(path).is_ok_and(|m| !m.file_type().is_symlink() && m.is_file());
    let Some(f) = ok.then(|| File::open(path).ok()).flatten() else {
        return false;
    };
    match f.try_lock_shared() {
        Ok(()) => {
            let _ = f.unlock();
            false
        }
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(_)) => false,
    }
}

fn read_plain(path: &Path) -> String {
    let ok = fs::symlink_metadata(path)
        .is_ok_and(|m| !m.file_type().is_symlink() && m.is_file() && m.len() <= MAX_READ_BYTES);
    if ok {
        fs::read_to_string(path).unwrap_or_default()
    } else {
        String::new()
    }
}

fn is_account_leased(leases: &Path, id: &str) -> bool {
    let ok_ch = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.');
    if id.is_empty() || id.starts_with('.') || !id.bytes().all(ok_ch) || !leases.is_dir() {
        return false;
    }
    if is_locked(&leases.join(format!("{id}.lock"))) {
        return true;
    }
    let pfx = format!("{id}.slot");
    let Ok(rd) = fs::read_dir(leases) else {
        return false;
    };
    rd.flatten().any(|e| {
        let name = e.file_name();
        let hit = name
            .to_str()
            .is_some_and(|n| n.starts_with(&pfx) && n.ends_with(".lock"));
        hit && is_locked(&e.path())
    })
}

/// Strips ANSI CSI/OSC/DCS/APC/PM/SOS escape sequences, standalone ESC
/// sequences, and C0/C1 control characters from untrusted text before
/// rendering to a terminal.
pub fn sanitize_display(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => match chars.peek().copied() {
                // Do not swallow a consecutive ESC or C1 introducer so the
                // next loop iteration strips the following sequence intact.
                Some('\x1b' | '\u{0080}'..='\u{009F}') | None => {}
                Some('[') => {
                    let _ = chars.next();
                    let _ = chars.by_ref().any(|c| ('@'..='~').contains(&c));
                }
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    let _ = chars.next();
                    skip_st(&mut chars);
                }
                Some(c) if (' '..='/').contains(&c) => {
                    let _ = chars.next();
                    let _ = chars.by_ref().any(|c| ('0'..='~').contains(&c));
                }
                Some(_) => {
                    let _ = chars.next();
                }
            },
            '\u{009B}' => {
                let _ = chars.by_ref().any(|c| ('@'..='~').contains(&c));
            }
            '\u{0090}' | '\u{0098}' | '\u{009D}' | '\u{009E}' | '\u{009F}' => skip_st(&mut chars),
            '\n' | '\r' | '\t' | '\u{2028}' | '\u{2029}' => out.push(' '),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

fn skip_st(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(ch) = chars.next() {
        if ch == '\x07' || ch == '\u{009C}' {
            break;
        }
        if ch == '\x1b' && chars.peek() == Some(&'\\') {
            let _ = chars.next();
            break;
        }
    }
}

/// Renders a compact, 4-panel plain-text terminal dashboard.
pub fn render_watch_text(snap: &WatchSnapshot) -> String {
    let mut out = String::new();
    let (c, hs, ac) = (&snap.tasks.counts, &snap.heavy_slots, &snap.accounts);
    let frz = if snap.frozen { "yes" } else { "no" };
    let sc = sanitize_display(&snap.scope);
    let _ = writeln!(
        out,
        "== aaf watch | scope: {sc} | frozen: {frz} (fails: {}) ==",
        snap.consecutive_root_fails
    );
    let _ = writeln!(
        out,
        "\n[Tasks] total={} ready={} running={} gating={} review={} blocked={} backlog={} done={} cancelled={}",
        c.total, c.ready, c.running, c.gating, c.review, c.blocked, c.backlog, c.done, c.cancelled
    );
    if snap.tasks.active.is_empty() {
        let _ = writeln!(out, "  (no active tasks)");
    }
    for t in &snap.tasks.active {
        let blk = t
            .blocked_reason
            .as_deref()
            .map_or(String::new(), |r| format!(" [{}]", sanitize_display(r)));
        let (id, prio, tt) = (
            sanitize_display(&t.id),
            sanitize_display(&t.priority),
            sanitize_display(&t.title),
        );
        let _ = writeln!(
            out,
            "  {id}  {prio}  {:<8}  cycles={}  {tt}{blk}",
            t.state.as_str(),
            t.unblock_cycles
        );
    }
    let drain = if hs.drain_active { "on" } else { "off" };
    let pressure = sanitize_display(&hs.memory_pressure);
    let _ = writeln!(
        out,
        "\n[Heavy Slots] pressure={pressure} drain={drain} measured={}",
        hs.measured_commands
    );
    if hs.slots.is_empty() {
        let _ = writeln!(out, "  (no slot files)");
    }
    for s in &hs.slots {
        let pg = s.pgid.map_or(String::new(), |p| format!("  pgid={p}"));
        let ram = s
            .reserved_ram_mb
            .map_or(String::new(), |r| format!("  ram={r}MB"));
        let lbl = s.label.as_deref().map_or(String::new(), |l| {
            format!("  label={}", sanitize_display(l))
        });
        let st = sanitize_display(&s.state);
        let _ = writeln!(out, "  slot {}  {st:<6}{pg}{ram}{lbl}", s.slot);
    }
    let pin = ac
        .pinned_account
        .as_deref()
        .map_or_else(|| "none".to_string(), sanitize_display);
    let sel = sanitize_display(&ac.selection_mode);
    let _ = writeln!(out, "\n[Accounts] selection={sel} pinned={pin}");
    if ac.entries.is_empty() {
        let _ = writeln!(out, "  (no accounts)");
    }
    for a in &ac.entries {
        let cd = a
            .cooldown_until
            .map_or(String::new(), |ts| format!(" until={ts}"));
        let id = sanitize_display(&a.id);
        let prov = sanitize_display(&a.provider);
        let st = sanitize_display(&a.state);
        let _ = writeln!(out, "  {id}  {prov}  {st}{cd}  tokens_5h={}", a.tokens_5h);
    }
    if let Some(r) = &snap.receipt {
        let fp = sanitize_display(&r.tree_fp_short);
        let _ = write!(
            out,
            "\n[Receipt] {:?} ({:?}) tree={fp} warnings={} created_at={}",
            r.status, r.mode, r.warnings, r.created_at
        );
    } else {
        let _ = write!(out, "\n[Receipt] none");
    }
    out
}
