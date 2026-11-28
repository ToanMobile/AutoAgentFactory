//! Per-repository rollup and strict `?repo=` scope validation for `/studio`.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use aaf_domain::run::{RunRecord, RunStatus};
use aaf_domain::task::{Task, TaskState};
use aaf_engine::db::Db;
use serde_json::{Value, json};

use super::studio_tokens::{TaskTokenExtra, is_audit_executor, pct};

#[derive(Default)]
struct RepoRollup {
    repo_name: String,
    ready: u64,
    running: u64,
    gating: u64,
    review: u64,
    blocked: u64,
    done: u64,
    blocked_upstream: u64,
    runs_total: u64,
    runs_failed: u64,
    limit_hit_runs: u64,
    tokens_total: u64,
    wasted_tokens: u64,
    audit_tokens: u64,
}

/// Validates an optional `?repo=<repo_root>` query parameter against the DB's
/// known repo roots (exact string match only; never builds arbitrary paths).
pub(super) fn resolve_requested_repo<'a>(
    db: Option<&Db>,
    scoped_repo: Option<&'a str>,
    requested: Option<&'a str>,
) -> Result<Option<&'a str>, String> {
    let Some(req) = requested.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(scoped_repo);
    };
    if let Some(fixed) = scoped_repo {
        if fixed == req {
            return Ok(Some(fixed));
        }
        return Err("invalid: repo filter does not match server scope".into());
    }
    let Some(db) = db else {
        return Err("invalid: unknown repo (database not initialized)".into());
    };
    let known = db.repo_task_summary(None).unwrap_or_default();
    if known.iter().any(|(root, _, _)| root == req) {
        Ok(Some(req))
    } else {
        Err("invalid: unknown repository scope".into())
    }
}

/// Aggregates per-repo counts, upstream deadlocks, and token metrics from the
/// already-loaded window tasks/runs.
pub(super) fn collect_repo_rollup(
    tasks: &[Task],
    runs_by_task: &HashMap<String, Vec<RunRecord>>,
    extras: &HashMap<String, TaskTokenExtra>,
    blocked_upstream: &HashMap<String, String>,
) -> Vec<Value> {
    let mut by_repo: BTreeMap<&str, RepoRollup> = BTreeMap::new();
    for t in tasks {
        if t.repo_root.trim().is_empty() {
            continue;
        }
        let entry = by_repo.entry(t.repo_root.as_str()).or_insert_with(|| {
            let repo_name = Path::new(&t.repo_root)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(&t.repo_root)
                .to_string();
            RepoRollup {
                repo_name,
                ..RepoRollup::default()
            }
        });
        match t.state {
            TaskState::Ready => entry.ready += 1,
            TaskState::Running => entry.running += 1,
            TaskState::Gating => entry.gating += 1,
            TaskState::Review => entry.review += 1,
            TaskState::Blocked => entry.blocked += 1,
            TaskState::Done => entry.done += 1,
            // Adopting has no rollup column; it is left out rather than mislabeled as
            // blocked, running or done.
            TaskState::Backlog | TaskState::Cancelled | TaskState::Adopting => {}
        }
        if blocked_upstream.contains_key(&t.id) {
            entry.blocked_upstream += 1;
        }
        if let Some(runs) = runs_by_task.get(&t.id) {
            for r in runs {
                entry.runs_total += 1;
                let tot = r.tokens_in.saturating_add(r.tokens_out);
                entry.tokens_total = entry.tokens_total.saturating_add(tot);
                let audit_rej =
                    is_audit_executor(&r.executor) && r.status == RunStatus::Failed && tot > 0;
                match r.status {
                    RunStatus::Failed if !audit_rej => entry.runs_failed += 1,
                    RunStatus::TimedOut | RunStatus::Killed => entry.runs_failed += 1,
                    RunStatus::LimitHit => {
                        entry.runs_failed += 1;
                        entry.limit_hit_runs += 1;
                    }
                    _ => {}
                }
            }
        }
        if let Some(ex) = extras.get(&t.id) {
            entry.wasted_tokens = entry.wasted_tokens.saturating_add(ex.wasted_tokens);
            entry.audit_tokens = entry.audit_tokens.saturating_add(ex.audit_tokens);
        }
    }

    let mut rows: Vec<(&str, RepoRollup)> = by_repo.into_iter().collect();
    rows.sort_by(|a, b| {
        b.1.tokens_total.cmp(&a.1.tokens_total).then_with(|| {
            a.1.repo_name
                .to_lowercase()
                .cmp(&b.1.repo_name.to_lowercase())
        })
    });
    rows.into_iter()
        .map(|(root, r)| {
            json!({
                "repo_root": root,
                "repo_name": r.repo_name,
                "ready": r.ready,
                "running": r.running,
                "gating": r.gating,
                "review": r.review,
                "blocked": r.blocked,
                "done": r.done,
                "blocked_upstream": r.blocked_upstream,
                "runs_total": r.runs_total,
                "runs_failed": r.runs_failed,
                "limit_hit_runs": r.limit_hit_runs,
                "tokens_total": r.tokens_total,
                "wasted_tokens": r.wasted_tokens,
                "wasted_pct": pct(r.wasted_tokens, r.tokens_total),
                "audit_tokens": r.audit_tokens,
            })
        })
        .collect()
}
