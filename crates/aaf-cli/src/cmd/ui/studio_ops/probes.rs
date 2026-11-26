//! Bounded, read-only runtime probes for `studio_ops`:
//! - Heavy slot / guest `.rec` orphan detection and `drain.lock` / queue probes
//! - Bounded 16 KiB stream-tail inspection for stalled, long-running, and command-looping workers
//! - Per-provider account pool availability & recent supervisor starvation log probe

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use aaf_domain::account::AccountState;
use aaf_domain::run::{RunRecord, RunStatus};
use aaf_engine::executors::reaper::validate_run_id;
use aaf_engine::telemetry::scan_throttle;
use aaf_engine::throttle::{GroupControl, SystemGroup};
use serde_json::{Value, json};

pub(super) const STALL_WARN_SECS: u64 = 300;
pub(super) const STALL_HIGH_SECS: u64 = 600;
pub(super) const STALL_NON_STREAM_WARN_SECS: u64 = 900;
pub(super) const STALL_NON_STREAM_HIGH_SECS: u64 = 1500;
pub(super) const STALL_NO_STREAM_SECS: u64 = 300;
pub(super) const LONG_RUN_WARN_SECS: u64 = 1500;
pub(super) const LONG_RUN_HIGH_SECS: u64 = 2400;
const STREAM_TAIL_BYTES: u64 = 16_384;
const STREAM_TAIL_LINES: usize = 20;
const LOOP_REPEAT_THRESHOLD: usize = 5;
const SUPERVISOR_TAIL_BYTES: u64 = 4_096;
const SUPERVISOR_RECENT_SECS: u64 = 900;

pub(super) struct SlotProbe {
    pub active_slots: usize,
    pub draining: bool,
    pub waiters: usize,
    pub oldest_wait_secs: u64,
    pub dead_orphan_slots: Vec<String>,
}

pub(super) struct WorkerProbe {
    pub stalled_workers: Vec<Value>,
    pub long_running_workers: Vec<Value>,
    pub looping_workers: Vec<Value>,
}

pub(super) struct AccountPoolSummary {
    pub by_provider: Vec<Value>,
    pub total_available: usize,
    pub total_cooling: usize,
    pub total_leased: usize,
    pub total_accounts: usize,
    pub waiting_for_account: u64,
    pub waiting_repos: Vec<String>,
}

fn parse_pgid(text: &str) -> Option<u32> {
    text.lines().find_map(|ln| {
        ln.trim()
            .strip_prefix("pgid=")
            .and_then(|v| v.trim().parse::<u32>().ok())
    })
}

fn read_bounded_tail(path: &Path, max_bytes: u64) -> Option<String> {
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len == 0 {
        return Some(String::new());
    }
    let start = len.saturating_sub(max_bytes);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    if start > 0 {
        // Drop partial first line when seeking mid-file.
        return Some(
            text.split_once('\n')
                .map_or(text.as_str(), |(_, rest)| rest)
                .to_string(),
        );
    }
    Some(text)
}

pub(super) fn inspect_slots_and_orphans(data_dir: Option<&Path>) -> SlotProbe {
    let Some(dir) = data_dir else {
        return SlotProbe {
            active_slots: 0,
            draining: false,
            waiters: 0,
            oldest_wait_secs: 0,
            dead_orphan_slots: Vec::new(),
        };
    };
    let (snap, entries) = scan_throttle(dir);
    let guest_dir = dir.join("throttle").join("guest");
    let mut dead_orphans = Vec::new();
    for entry in entries {
        if entry.locked {
            continue;
        }
        if fs::metadata(&entry.path).is_ok_and(|m| m.len() > 0) {
            let text = fs::read_to_string(&entry.path).unwrap_or_default();
            let pgid = parse_pgid(&text);
            let is_dead = match pgid {
                Some(id) => matches!(SystemGroup.read_group_state(id), Ok(None)),
                None => true,
            };
            if is_dead {
                let label = match pgid {
                    Some(id) => format!("heavy.{}.lock(pgid={id})", entry.slot),
                    None => format!("heavy.{}.lock(pgid=none)", entry.slot),
                };
                dead_orphans.push(label);
            }
            continue;
        }
        // Seatbelt guest holders leave heavy.<slot>.lock at 0 bytes and write pgid to guest/heavy.<slot>.rec.
        let guest_rec = guest_dir.join(format!("heavy.{}.rec", entry.slot));
        if fs::symlink_metadata(&guest_rec)
            .is_ok_and(|m| m.is_file() && m.len() > 0 && m.len() <= 4096)
        {
            let text = fs::read_to_string(&guest_rec).unwrap_or_default();
            if let Some(id) = parse_pgid(&text)
                && matches!(SystemGroup.read_group_state(id), Ok(None))
            {
                dead_orphans.push(format!("guest/heavy.{}.rec(pgid={id})", entry.slot));
            }
        }
    }
    SlotProbe {
        active_slots: snap.throttle_slots,
        draining: snap.drain_lock_present,
        waiters: snap.waiters,
        oldest_wait_secs: snap.oldest_wait_secs.unwrap_or(0),
        dead_orphan_slots: dead_orphans,
    }
}

struct StreamTailInfo {
    last_type: Option<String>,
    last_tool: Option<String>,
    last_target: Option<String>,
    waiting_on_slot: bool,
    repeat_count: usize,
    repeated_sig: Option<String>,
}

fn truncate_str(s: &str, max_chars: usize) -> String {
    let clean: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut chars = clean.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

fn is_paginated_file_tool(tool: Option<&str>) -> bool {
    matches!(
        tool,
        Some(
            "Read"
                | "Edit"
                | "MultiEdit"
                | "Write"
                | "NotebookEdit"
                | "TodoWrite"
                | "read_file"
                | "edit_file"
        )
    )
}

fn inspect_stream_tail(path: &Path) -> StreamTailInfo {
    let Some(text) = read_bounded_tail(path, STREAM_TAIL_BYTES) else {
        return StreamTailInfo {
            last_type: None,
            last_tool: None,
            last_target: None,
            waiting_on_slot: false,
            repeat_count: 0,
            repeated_sig: None,
        };
    };
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .rev()
        .take(STREAM_TAIL_LINES)
        .collect();
    let mut last_type = None;
    let mut last_tool = None;
    let mut last_target = None;
    let mut waiting_on_slot = false;
    let mut sig_counts: HashMap<String, usize> = HashMap::new();

    for (idx, ln) in lines.iter().enumerate() {
        let Ok(v) = serde_json::from_str::<Value>(ln) else {
            continue;
        };
        let ev_type = v.get("type").and_then(Value::as_str);
        let tool = v.get("tool").and_then(Value::as_str);
        let target = v.get("target").and_then(Value::as_str);
        if idx == 0 {
            last_type = ev_type.map(str::to_string);
            last_tool = tool.map(str::to_string);
            last_target = target.map(|t| truncate_str(t, 120));
        }
        if idx < 4
            && let Some(t) = target
        {
            let lower = t.to_ascii_lowercase();
            if lower.contains("aaf slot")
                || lower.contains("slot nặng")
                || lower.contains("heavy slot")
            {
                waiting_on_slot = true;
            }
        }
        if matches!(ev_type, Some("tool_use" | "error" | "rate_limit"))
            && !(ev_type == Some("tool_use") && is_paginated_file_tool(tool))
            && let Some(t) = target
        {
            // Key on the full target (up to 512 chars) so long shared `cd /worktree/...;`
            // prefixes never collapse distinct commands into a false-positive loop.
            let sig = format!(
                "{}:{}",
                tool.unwrap_or(ev_type.unwrap_or("")),
                truncate_str(t, 512)
            );
            *sig_counts.entry(sig).or_insert(0) += 1;
        }
    }
    let (repeated_sig, repeat_count) = sig_counts
        .into_iter()
        .max_by_key(|(_, c)| *c)
        .map_or((None, 0), |(s, c)| (Some(truncate_str(&s, 100)), c));

    StreamTailInfo {
        last_type,
        last_tool,
        last_target,
        waiting_on_slot,
        repeat_count,
        repeated_sig,
    }
}

pub(super) fn detect_worker_issues(
    data_dir: Option<&Path>,
    runs_by_task: &HashMap<String, Vec<RunRecord>>,
    now: i64,
) -> WorkerProbe {
    let streams_dir = data_dir.map(|d| d.join("streams"));
    let now_time = UNIX_EPOCH + Duration::from_secs(u64::try_from(now.max(0)).unwrap_or(0));
    let mut stalled = Vec::new();
    let mut long_running = Vec::new();
    let mut looping = Vec::new();

    for runs in runs_by_task.values() {
        for r in runs.iter().filter(|r| r.status == RunStatus::Running) {
            let run_age = u64::try_from(now.saturating_sub(r.started_at).max(0)).unwrap_or(0);
            let stream_path = streams_dir
                .as_ref()
                .filter(|_| validate_run_id(&r.id).is_ok())
                .map(|d| d.join(format!("{}.jsonl", r.id)));
            let meta = stream_path.as_ref().and_then(|p| fs::metadata(p).ok());
            let tail = stream_path
                .as_ref()
                .filter(|_| meta.is_some())
                .map(|p| inspect_stream_tail(p));

            if run_age >= LONG_RUN_WARN_SECS {
                let sev = if run_age >= LONG_RUN_HIGH_SECS {
                    "high"
                } else {
                    "medium"
                };
                long_running.push(json!({
                    "task_id": r.task_id,
                    "run_id": r.id,
                    "age_secs": run_age,
                    "severity": sev,
                    "last_type": tail.as_ref().and_then(|t| t.last_type.clone()),
                    "last_tool": tail.as_ref().and_then(|t| t.last_tool.clone()),
                    "last_target": tail.as_ref().and_then(|t| t.last_target.clone()),
                    "waiting_on_slot": tail.as_ref().is_some_and(|t| t.waiting_on_slot),
                }));
            }
            if let Some(ref info) = tail
                && info.repeat_count >= LOOP_REPEAT_THRESHOLD
            {
                looping.push(json!({
                    "task_id": r.task_id,
                    "run_id": r.id,
                    "repeat_count": info.repeat_count,
                    "signature": info.repeated_sig,
                }));
            }
            if streams_dir.is_none() || validate_run_id(&r.id).is_err() {
                continue;
            }
            let non_streaming = r.executor.eq_ignore_ascii_case("antigravity")
                || tail
                    .as_ref()
                    .and_then(|t| t.last_tool.as_deref())
                    .is_some_and(|t| t.eq_ignore_ascii_case("antigravity"));
            let (warn_s, high_s) = if non_streaming {
                (STALL_NON_STREAM_WARN_SECS, STALL_NON_STREAM_HIGH_SECS)
            } else {
                (STALL_WARN_SECS, STALL_HIGH_SECS)
            };
            if let Some(mtime) = meta.and_then(|m| m.modified().ok()) {
                let idle = now_time
                    .duration_since(mtime)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                if idle >= warn_s {
                    let sev = if idle >= high_s { "high" } else { "medium" };
                    stalled.push(json!({
                        "task_id": r.task_id,
                        "run_id": r.id,
                        "idle_secs": idle,
                        "severity": sev,
                        "last_type": tail.as_ref().and_then(|t| t.last_type.clone()),
                        "last_tool": tail.as_ref().and_then(|t| t.last_tool.clone()),
                        "last_target": tail.as_ref().and_then(|t| t.last_target.clone()),
                        "waiting_on_slot": tail.as_ref().is_some_and(|t| t.waiting_on_slot),
                    }));
                }
            } else if run_age >= STALL_NO_STREAM_SECS {
                let sev = if run_age >= high_s { "high" } else { "medium" };
                stalled.push(json!({
                    "task_id": r.task_id,
                    "run_id": r.id,
                    "idle_secs": run_age,
                    "severity": sev,
                    "last_type": null,
                    "last_tool": null,
                    "last_target": null,
                    "waiting_on_slot": false,
                }));
            }
        }
    }
    stalled.sort_by(|a, b| {
        b["idle_secs"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["idle_secs"].as_u64().unwrap_or(0))
    });
    long_running.sort_by(|a, b| {
        b["age_secs"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["age_secs"].as_u64().unwrap_or(0))
    });
    looping.sort_by(|a, b| {
        b["repeat_count"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["repeat_count"].as_u64().unwrap_or(0))
    });
    WorkerProbe {
        stalled_workers: stalled,
        long_running_workers: long_running,
        looping_workers: looping,
    }
}

fn parse_waiting_for_account_line(line: &str) -> Option<u64> {
    let idx = line.find(" waiting for an account")?;
    let prefix = line[..idx].trim_end();
    let num_str = prefix.rsplit(|c: char| !c.is_ascii_digit()).next()?;
    if num_str.is_empty() {
        return None;
    }
    num_str.parse::<u64>().ok()
}

fn scan_supervisor_account_waits(data_dir: Option<&Path>, now: i64) -> (u64, Vec<String>) {
    let Some(dir) = data_dir else {
        return (0, Vec::new());
    };
    let logs_dir = dir.join("supervisor-logs");
    let Ok(entries) = fs::read_dir(&logs_dir) else {
        return (0, Vec::new());
    };
    let now_time = UNIX_EPOCH + Duration::from_secs(u64::try_from(now.max(0)).unwrap_or(0));
    let mut total_wait = 0u64;
    let mut repos = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("log") {
            continue;
        }
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        if now_time
            .duration_since(mtime)
            .is_ok_and(|d| d.as_secs() > SUPERVISOR_RECENT_SECS)
        {
            continue;
        }
        let Some(tail) = read_bounded_tail(&path, SUPERVISOR_TAIL_BYTES) else {
            continue;
        };
        if let Some(cnt) = tail.lines().rev().find_map(parse_waiting_for_account_line)
            && cnt > 0
        {
            total_wait = total_wait.saturating_add(cnt);
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("repo");
            let repo = stem.rsplit_once('-').map_or(stem, |(name, _)| name);
            repos.push(format!("{repo}({cnt})"));
        }
    }
    repos.sort();
    (total_wait, repos)
}

pub(super) fn summarize_account_pool(
    data_dir: Option<&Path>,
    accounts: &[AccountState],
    runs_by_task: &HashMap<String, Vec<RunRecord>>,
    now: i64,
) -> AccountPoolSummary {
    let mut leased_counts: HashMap<&str, usize> = HashMap::new();
    for runs in runs_by_task.values() {
        for r in runs.iter().filter(|r| r.status == RunStatus::Running) {
            if let Some(aid) = r.account_id.as_deref() {
                *leased_counts.entry(aid).or_insert(0) += 1;
            }
        }
    }
    let mut prov_map: BTreeMap<&str, (usize, usize, usize, usize, Option<u64>)> = BTreeMap::new();
    let (mut total_available, mut total_cooling, mut total_leased) = (0usize, 0usize, 0usize);
    for a in accounts {
        let prov = a.profile.provider.as_str();
        let entry = prov_map.entry(prov).or_insert((0, 0, 0, 0, None));
        entry.0 += 1;
        let cooling_rem = a
            .cooldown_until
            .filter(|&u| u > now)
            .map(|u| u64::try_from(u.saturating_sub(now)).unwrap_or(0));
        let is_leased = leased_counts
            .get(a.profile.id.as_str())
            .copied()
            .unwrap_or(0)
            > 0;
        if let Some(rem) = cooling_rem {
            entry.3 += 1;
            total_cooling += 1;
            entry.4 = Some(entry.4.map_or(rem, |cur| cur.min(rem)));
        } else if is_leased {
            entry.2 += 1;
            total_leased += 1;
        } else {
            entry.1 += 1;
            total_available += 1;
        }
    }
    let by_provider: Vec<Value> = prov_map
        .into_iter()
        .map(|(prov, (total, available, leased, cooling, min_cd))| {
            json!({
                "provider": prov,
                "total": total,
                "available": available,
                "leased": leased,
                "cooling": cooling,
                "min_cooldown_remaining_secs": min_cd,
            })
        })
        .collect();
    let (waiting_for_account, waiting_repos) = scan_supervisor_account_waits(data_dir, now);
    AccountPoolSummary {
        by_provider,
        total_available,
        total_cooling,
        total_leased,
        total_accounts: accounts.len(),
        waiting_for_account,
        waiting_repos,
    }
}
