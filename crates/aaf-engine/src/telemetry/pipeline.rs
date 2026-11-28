//! Read-only task pipeline DAG projection and SOP checkpoint reader.

use std::cmp::Reverse;
use std::fs::{self, File};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use aaf_domain::blocked::BlockedReason;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::run::{RunRecord, RunStatus};
use aaf_domain::sop::SopCheckpoint;
use aaf_domain::task::{Task, TaskState};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_SOP_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SOP_SCAN_ENTRIES: usize = 2000;
const REWORK_DEDUP_WINDOW_SECS: i64 = 5;
const STAGES: [(&str, &str, &str); 8] = [
    ("spec", "Spec", "spec"),
    ("plan", "Plan", "agent"),
    ("implement", "Implement", "agent"),
    ("gate", "Gate", "gate"),
    ("proof", "Proof", "proof"),
    ("audit", "Audit", "audit"),
    ("review", "Review", "human"),
    ("done", "Done", "delivery"),
];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineNode {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub status: String,
    pub attempts: u32,
    pub duration_ms: Option<u64>,
    pub executor: Option<String>,
    pub account_id: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineEdge {
    pub from: String,
    pub to: String,
    pub kind: String,
    pub count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskPipeline {
    pub task_id: String,
    pub state: String,
    pub nodes: Vec<PipelineNode>,
    pub edges: Vec<PipelineEdge>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SopStageView {
    pub id: String,
    pub status: String,
    pub summary: Option<String>,
    pub artifacts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SopRunSummary {
    pub run_id: String,
    pub sop_name: String,
    pub status: String,
    pub updated_at: i64,
    pub stages: Vec<SopStageView>,
}

#[derive(Default)]
struct Track {
    gate_ts: Option<i64>,
    gating_ts: Option<i64>,
    pending_fail: Option<usize>,
    reworks: [u32; 3], // [gate, audit, review]
    recent_rework_ts: Option<i64>,
}

/// Builds a deterministic stage DAG for `task` from its runs and append-only events.
pub fn build_task_pipeline(
    task: &Task,
    runs: &[RunRecord],
    events: &[FactoryEvent],
) -> TaskPipeline {
    let mut n: Vec<_> = STAGES
        .iter()
        .map(|&(id, lb, kd)| PipelineNode {
            id: id.into(),
            label: lb.into(),
            kind: kd.into(),
            status: "pending".into(),
            ..Default::default()
        })
        .collect();
    let mut s_runs: Vec<_> = runs.iter().collect();
    s_runs.sort_by_key(|r| r.started_at);
    let mut s_evs: Vec<_> = events.iter().collect();
    s_evs.sort_by_key(|e| (e.created_at, e.id.is_none(), e.id.unwrap_or(0)));

    let (mut tr, mut ri) = (Track::default(), 0usize);
    for ev in s_evs {
        while ri < s_runs.len() && s_runs[ri].started_at <= ev.created_at {
            if let Some(i) = tr.pending_fail.take() {
                tr.reworks[i] = tr.reworks[i].saturating_add(1);
            }
            tr.recent_rework_ts = None;
            ri += 1;
        }
        apply_event(ev, &mut n, &mut tr);
    }
    if ri < s_runs.len()
        && let Some(i) = tr.pending_fail.take()
    {
        tr.reworks[i] = tr.reworks[i].saturating_add(1);
    }
    finalize(task, &s_runs, &mut n, tr.gate_ts, tr.gating_ts);

    let mut edges: Vec<_> = STAGES
        .windows(2)
        .map(|w| edge(w[0].0, w[1].0, "next", 1))
        .collect();
    for (&src, &c) in ["gate", "audit", "review"].iter().zip(&tr.reworks) {
        if c > 0 {
            edges.push(edge(src, "implement", "rework", c));
        }
    }
    TaskPipeline {
        task_id: task.id.clone(),
        state: task.state.as_str().into(),
        nodes: n,
        edges,
    }
}

fn edge(f: &str, t: &str, k: &str, count: u32) -> PipelineEdge {
    PipelineEdge {
        from: f.into(),
        to: t.into(),
        kind: k.into(),
        count,
    }
}

fn jstr(v: Option<&Value>, keys: &[&str]) -> Option<String> {
    let o = v?;
    keys.iter()
        .find_map(|k| o.get(*k)?.as_str().map(str::trim).filter(|s| !s.is_empty()))
        .map(str::to_string)
}

fn hit(n: &mut PipelineNode, v: Option<&Value>, keys: &[&str], exec: bool) {
    n.attempts = n.attempts.saturating_add(1);
    n.detail = jstr(v, keys).or_else(|| n.detail.take());
    if exec {
        n.executor = jstr(v, &["executor"]).or_else(|| n.executor.take());
    }
}

fn apply_event(ev: &FactoryEvent, n: &mut [PipelineNode], tr: &mut Track) {
    use EventKind as E;
    let val = serde_json::from_str::<Value>(&ev.payload_json).ok();
    let v = val.as_ref();
    let jb = |k: &str| v.and_then(|x| x.get(k)).and_then(Value::as_bool);
    let pf = |ok: bool| if ok { "passed" } else { "failed" }.to_string();
    let rw = |t: &mut Track, i: usize| {
        t.reworks[i] = t.reworks[i].saturating_add(1);
        t.pending_fail = None;
    };
    let bad = jstr(v, &["status"]).is_some_and(|s| s.eq_ignore_ascii_case("failed"));
    match ev.kind {
        E::PlanGenerated | E::PlanDebateTurn | E::PlanConverged => {
            n[1].attempts = n[1]
                .attempts
                .saturating_sub((ev.kind == E::PlanConverged) as u32);
            hit(&mut n[1], v, &["summary", "verdict", "forced_by"], true);
            if n[1].status != "passed" {
                let s = if ev.kind != E::PlanDebateTurn {
                    "passed"
                } else if jb("ok") == Some(false) {
                    "failed"
                } else {
                    "running"
                };
                n[1].status = s.into();
            }
        }
        E::RunStarted => {
            tr.recent_rework_ts = None;
            n[2].attempts = n[2].attempts.saturating_add(1);
            if let Some(i) = tr.pending_fail {
                rw(tr, i);
            }
        }
        E::GateResult => {
            tr.recent_rework_ts = None;
            if tr.pending_fail == Some(0) {
                rw(tr, 0);
            }
            hit(&mut n[3], v, &["summary", "status"], false);
            tr.gate_ts = Some(ev.created_at);
            if let Some(ms) = v.and_then(|x| x.get("duration_ms")).and_then(Value::as_u64) {
                n[3].duration_ms = Some(n[3].duration_ms.unwrap_or(0).saturating_add(ms));
            }
            let st = jstr(v, &["status"]).map(|s| s.to_ascii_lowercase());
            let ok = st.as_deref().map_or(jb("passed").unwrap_or(false), |s| {
                matches!(s, "clear" | "pass" | "passed")
            });
            n[3].status = pf(ok);
            tr.pending_fail = (!ok).then_some(0).or(tr.pending_fail);
        }
        E::ProofRecorded => {
            hit(&mut n[4], v, &["tree_fp", "summary", "status"], false);
            n[4].status = pf(jb("ok").unwrap_or(!bad));
        }
        E::CrossAudit | E::DualReviewTurn => {
            tr.recent_rework_ts = None;
            let ver = jstr(v, &["verdict"]).map(|s| s.to_ascii_lowercase());
            let pass = match ver.as_deref() {
                Some("auth_fallback") => None,
                Some(s) => Some(
                    !s.starts_with("fail") && !s.starts_with("reject") && s != "changes_requested",
                ),
                None => jb("passed"),
            };
            if let Some(p) = pass {
                hit(&mut n[5], v, &["verdict", "summary"], true);
                n[5].status = pf(p);
                tr.pending_fail = (!p).then_some(1).or(tr.pending_fail);
            }
        }
        E::PrOpened | E::PrFeedback => {
            tr.recent_rework_ts = None;
            hit(&mut n[6], v, &["pr_url", "url", "summary", "reason"], false);
            n[6].status = "passed".into();
            tr.pending_fail = (ev.kind == E::PrFeedback).then_some(2).or(tr.pending_fail);
        }
        E::ReworkRequested => {
            let i = match jstr(v, &["from", "stage"]).as_deref() {
                Some("gate") => 0,
                Some("audit") => 1,
                Some("review") => 2,
                _ => tr.pending_fail.unwrap_or(2),
            };
            rw(tr, i);
            tr.recent_rework_ts = Some(ev.created_at);
            if i == 2 {
                hit(&mut n[6], v, &["reason", "summary"], false);
                n[6].status = "passed".into();
            }
        }
        E::TaskStateChanged => {
            let (from, to, why) = (jstr(v, &["from"]), jstr(v, &["to"]), jstr(v, &["why"]));
            let (fr, tr_to) = (from.as_deref(), to.as_deref());
            if tr_to == Some("gating") {
                tr.gating_ts = Some(ev.created_at);
            }
            if fr == Some("review") || tr_to == Some("review") {
                (n[6].status, n[6].attempts) = ("passed".into(), n[6].attempts.max(1));
            }
            let dedup = tr.recent_rework_ts.take().is_some_and(|rw_ts| {
                ev.created_at.saturating_sub(rw_ts) <= REWORK_DEDUP_WINDOW_SECS
            });
            if matches!(tr_to, Some("ready" | "running")) && !dedup {
                let aud = why.as_deref().is_some_and(|w| w.contains("audit"));
                if let Some(i) = aud
                    .then_some(1)
                    .or((fr == Some("review")).then_some(2))
                    .or(tr.pending_fail)
                {
                    rw(tr, i);
                }
            }
        }
        E::MergeResult => {
            hit(&mut n[7], v, &["merged_head", "sha", "error"], false);
            let fb = !bad && jstr(v, &["error"]).is_none();
            n[7].status = pf(jb("ok").or_else(|| jb("merged")).unwrap_or(fb));
        }
        _ => {}
    }
}

fn finalize(
    task: &Task,
    runs: &[&RunRecord],
    n: &mut [PipelineNode],
    gate_ts: Option<i64>,
    gating_ts: Option<i64>,
) {
    let (last, st) = (runs.last().copied(), task.state);
    (n[0].status, n[0].attempts) = ("passed".into(), 1);
    n[0].detail = (!task.title.is_empty()).then(|| task.title.clone());
    let rc = u32::try_from(runs.len()).unwrap_or(u32::MAX);
    n[2].attempts = n[2].attempts.max(rc);
    n[2].duration_ms = runs
        .iter()
        .filter_map(|r| {
            Some(
                u64::try_from(r.ended_at?.saturating_sub(r.started_at))
                    .unwrap_or(0)
                    .saturating_mul(1000),
            )
        })
        .reduce(u64::saturating_add);
    if let Some(r) = last {
        (n[2].executor, n[2].account_id) = (Some(r.executor.clone()), r.account_id.clone());
        n[2].detail = Some(r.status.as_str().into());
    }
    n[2].status = match last {
        // Adopting: no factory coding stage (a chat session wrote the PR), and the lane's
        // reviewer runs must not read as a coding run that is running or failed.
        _ if st == TaskState::Adopting => "skipped",
        Some(r) if r.status == RunStatus::Running => "running",
        Some(r) if r.status == RunStatus::Succeeded || st == TaskState::Done => "passed",
        Some(_) => "failed",
        None if st == TaskState::Running => "running",
        None => "pending",
    }
    .into();
    let stale_run = last.is_some_and(|r| gate_ts.is_none_or(|t| t < r.started_at));
    let stale_gating = gating_ts.is_some_and(|gt| gate_ts.is_none_or(|t| t < gt));
    if st == TaskState::Gating && (stale_run || stale_gating || n[3].status == "pending") {
        n[3].status = "running".into();
    }
    match &task.blocked_reason {
        Some(BlockedReason::GateFailed { summary }) if n[3].status == "pending" => {
            n[3].status = "failed".into();
            n[3].detail.get_or_insert_with(|| summary.clone());
        }
        Some(BlockedReason::NeedsHuman { why }) if st == TaskState::Blocked => {
            (n[6].status, n[6].attempts) = ("waiting_human".into(), n[6].attempts.max(1));
            n[6].detail = Some(why.clone());
        }
        Some(BlockedReason::MergeConflict { detail }) => {
            n[7].status = "failed".into();
            n[7].detail.get_or_insert_with(|| detail.clone());
        }
        _ => {}
    }
    match st {
        TaskState::Review => {
            (n[6].status, n[6].attempts) = ("waiting_human".into(), n[6].attempts.max(1))
        }
        TaskState::Done => (n[7].status, n[7].attempts) = ("passed".into(), n[7].attempts.max(1)),
        TaskState::Cancelled => n[7].status = "skipped".into(),
        // Adopting: keep the review/merge nodes as the lane's events recorded them; the
        // reset below is for factory tasks that fell back out of review.
        TaskState::Adopting => {}
        _ if n[6].status == "passed" => n[6].status = "pending".into(),
        _ => {}
    }
    if let Some(hi) = n.iter().rposition(|x| x.status != "pending") {
        for x in &mut n[..hi] {
            if x.status == "pending" {
                x.status = "skipped".into();
            } else if x.status == "running"
                && !matches!(
                    st,
                    // Adopting: the lane's live phase is still running, not finished.
                    TaskState::Running | TaskState::Gating | TaskState::Adopting
                )
            {
                x.status = "passed".into();
            }
        }
    }
}

/// Reads SOP checkpoint summaries under `<data_dir>/sops/*.json` read-only.
///
/// Symlinks, non-regular files, files over 2 MiB, and malformed JSON are skipped safely.
pub fn list_sop_runs(data_dir: &Path, limit: usize) -> Vec<SopRunSummary> {
    let dir = data_dir.join("sops");
    let Ok(dm) = fs::symlink_metadata(&dir) else {
        return Vec::new();
    };
    if limit == 0 || dm.file_type().is_symlink() || !dm.is_dir() {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut candidates: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in entries.take(MAX_SOP_SCAN_ENTRIES).flatten() {
        let name = entry.file_name();
        let Some(s) = name.to_str() else { continue };
        if s.starts_with('.') || !s.ends_with(".json") {
            continue;
        }
        let path = entry.path();
        let Ok(m) = fs::symlink_metadata(&path) else {
            continue;
        };
        if m.file_type().is_symlink() || !m.is_file() || m.len() > MAX_SOP_BYTES {
            continue;
        }
        candidates.push((m.modified().unwrap_or(UNIX_EPOCH), path));
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let parse_cap = limit.saturating_mul(2).max(16);
    let mut out: Vec<_> = candidates
        .into_iter()
        .take(parse_cap)
        .filter_map(|(_, path)| read_sop(&path))
        .collect();
    out.sort_by_key(|r| (Reverse(r.updated_at), r.run_id.clone()));
    out.truncate(limit);
    out
}

fn read_sop(path: &Path) -> Option<SopRunSummary> {
    let m = fs::symlink_metadata(path).ok()?;
    if m.file_type().is_symlink() || !m.is_file() || m.len() > MAX_SOP_BYTES {
        return None;
    }
    let mut buf = Vec::new();
    File::open(path)
        .ok()?
        .take(MAX_SOP_BYTES + 1)
        .read_to_end(&mut buf)
        .ok()?;
    let len = u64::try_from(buf.len()).unwrap_or(u64::MAX);
    let cp: SopCheckpoint = (len <= MAX_SOP_BYTES).then(|| serde_json::from_slice(&buf).ok())??;
    let tag = |v: Value| v.as_str().unwrap_or_default().to_string();
    let stages = cp.stage_states.into_iter().map(|(id, r)| SopStageView {
        id,
        status: serde_json::to_value(r.status).map(tag).unwrap_or_default(),
        summary: r.summary,
        artifacts: r.artifacts,
    });
    Some(SopRunSummary {
        run_id: cp.run_id,
        sop_name: cp.sop_name,
        status: serde_json::to_value(cp.status).map(tag).unwrap_or_default(),
        updated_at: cp.updated_at,
        stages: stages.collect(),
    })
}

#[cfg(test)]
mod tests;
