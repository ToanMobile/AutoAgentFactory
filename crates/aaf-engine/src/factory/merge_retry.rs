//! Retries an unattended PR merge that was blocked only by time
//! (`ci_pending`, `merge_in_progress`): the task stays in Review and the
//! review sweep tries again with exponential backoff, at most
//! [`MERGE_RETRY_MAX`] attempts in all (the land attempt is the first).
//! Every other block (`ci_failed`, approvals, head mismatch...) is never
//! retried here: red CI goes to the PR feedback loop, the rest to a person.

use aaf_domain::event::EventKind;
use aaf_domain::ports::PortResult;
use aaf_domain::task::{Task, TaskState};
use serde_json::json;

use super::Deps;
use super::scheduler::Scheduler;
use crate::git::TaskWorktree;
use crate::pr_ops::PrMergeOutcome;

/// Block codes that only need time to clear.
pub(crate) const RETRYABLE_MERGE_CODES: &[&str] =
    &["ci_pending", "merge_in_progress", "merge_error"];
/// Attempts in all, counting the one made when the task landed.
pub(crate) const MERGE_RETRY_MAX: u32 = 8;
const BACKOFF_BASE_SECS: i64 = 60;
const BACKOFF_CAP_SECS: i64 = 1_800;
const EXHAUSTED_CODE: &str = "merge_retry_exhausted";

/// Wait before attempt `attempts + 1`, after `attempts` blocked ones.
pub(crate) fn backoff_secs(attempts: u32) -> i64 {
    let shift = attempts.saturating_sub(1).min(10);
    (BACKOFF_BASE_SECS << shift).min(BACKOFF_CAP_SECS)
}

struct Pending {
    url: String,
    head_sha: String,
    attempts: u32,
    last_ts: i64,
}

/// The retryable block of the task's latest pull request, if its newest
/// merge result is one. A newer `PrOpened` starts the count again.
fn pending(deps: &Deps, task_id: &str) -> PortResult<Option<Pending>> {
    let events = deps.store.events_for_task_by_kinds(
        task_id,
        &[EventKind::PrOpened, EventKind::MergeResult],
        u32::MAX,
    )?;
    let mut newest: Option<Pending> = None;
    let mut attempts = 0_u32;
    // Newest first.
    for ev in events {
        if ev.kind == EventKind::PrOpened {
            break;
        }
        let p: serde_json::Value = serde_json::from_str(&ev.payload_json).unwrap_or_default();
        if p["kind"] != "pr_merge" || p["merged"] != false {
            if newest.is_none() {
                return Ok(None);
            }
            break;
        }
        let code = p["code"].as_str().unwrap_or("");
        if !RETRYABLE_MERGE_CODES.contains(&code) {
            if newest.is_none() {
                return Ok(None);
            }
            break;
        }
        attempts = attempts.saturating_add(1);
        if newest.is_none() {
            newest = Some(Pending {
                url: p["url"].as_str().unwrap_or("").to_string(),
                head_sha: p["head_sha"].as_str().unwrap_or("").to_string(),
                attempts: 0,
                last_ts: ev.created_at,
            });
        }
    }
    Ok(newest.map(|n| Pending { attempts, ..n }))
}

/// Records a blocked `pr_merge` attempt; retryable codes are picked up by
/// [`retry_task`].
pub(crate) fn record_block(
    deps: &Deps,
    task_id: &str,
    url: &str,
    head_sha: &str,
    code: &str,
) -> PortResult<()> {
    deps.emit(
        EventKind::MergeResult,
        task_id,
        json!({
            "merged": false,
            "kind": "pr_merge",
            "code": code,
            "url": url,
            "head_sha": head_sha,
            "retryable": RETRYABLE_MERGE_CODES.contains(&code),
        }),
    )
}

/// True when `task_id` is already `Done` with a recent `MergeResult` recorded
/// by `pr_cleanup_db` (`aaf_merge`) for `pr_number`.
pub(super) fn completed_by_pr_cleanup(
    deps: &Deps,
    task_id: &str,
    pr_number: u64,
) -> PortResult<bool> {
    if deps.store.get_task(task_id)?.state != TaskState::Done {
        return Ok(false);
    }
    let events = deps
        .store
        .events_for_task_by_kinds(task_id, &[EventKind::MergeResult], 5)?;
    Ok(events.iter().any(|ev| {
        serde_json::from_str::<serde_json::Value>(&ev.payload_json)
            .ok()
            .is_some_and(|p| {
                p["merged"] == true
                    && p["pr_number"].as_u64() == Some(pr_number)
                    && (p["by"].as_str() == Some("aaf_merge")
                        || p["kind"].as_str() == Some("pr_merge"))
            })
    }))
}

/// Retries the merge of one Review task when its backoff has elapsed.
/// Returns `Ok(true)` when the task was merged (moved to Done).
pub(crate) fn retry_task(deps: &Deps, task: &Task) -> PortResult<bool> {
    if task.state != TaskState::Review {
        return Ok(false);
    }
    let Some(p) = pending(deps, &task.id)? else {
        return Ok(false);
    };
    // Without the gated head there is nothing to pin the merge to.
    if p.url.is_empty() || p.head_sha.trim().is_empty() {
        return Ok(false);
    }
    if p.attempts >= MERGE_RETRY_MAX {
        // Emitted once: the newest result is then no longer retryable.
        record_block(deps, &task.id, &p.url, &p.head_sha, EXHAUSTED_CODE)?;
        return Ok(false);
    }
    if deps.now().saturating_sub(p.last_ts) < backoff_secs(p.attempts) {
        return Ok(false);
    }
    // `head_sha` is pinned above; `merge_pr` still checks reflogs on `wt.path` (`repo_root`).
    let wt = TaskWorktree {
        path: std::path::PathBuf::from(&deps.settings.repo_root),
        branch: format!("aaf/{}", task.id),
    };
    let outcome = match deps.git.merge_pr(&wt, &p.url, &p.head_sha) {
        Ok(outcome) => outcome,
        Err(err) => {
            eprintln!("aaf: merge retry {} ({}): {err}", task.id, p.url);
            // Counted as an attempt, so a broken `gh` cannot retry forever.
            record_block(deps, &task.id, &p.url, &p.head_sha, "merge_error")?;
            return Ok(false);
        }
    };
    match outcome {
        PrMergeOutcome::Merged {
            pr_number,
            head_sha,
            ..
        } => {
            let why = format!("merged pull request #{pr_number} ({}) on retry", p.url);
            let merge_payload = json!({
                "merged": true,
                "kind": "pr_merge",
                "pr_number": pr_number,
                "head_sha": head_sha,
                "retry": true,
            });
            let completed = deps.store.complete_review_task(
                &task.id,
                &why,
                Some(&merge_payload),
                deps.now(),
            )?;
            if !completed && !completed_by_pr_cleanup(deps, &task.id, pr_number)? {
                return Ok(false);
            }
            if let Err(err) = deps.git.fetch_base() {
                eprintln!("aaf: post-merge fetch {}: {err}", task.id);
            }
            if let Err(err) = deps.git.cleanup_verified_merged(&task.id, &head_sha) {
                eprintln!("aaf: post-merge cleanup {}: {err}", task.id);
            }
            if !completed {
                super::finish::close(deps, task)?;
                super::unblock::root_succeeded(deps, task)?;
            }
            Ok(true)
        }
        PrMergeOutcome::AlreadyMerged { pr_number, .. } => {
            let why = format!("merged pull request #{pr_number} ({}) on retry", p.url);
            let merge_payload = json!({
                "merged": true,
                "kind": "pr_merge",
                "pr_number": pr_number,
                "already_merged": true,
                "retry": true,
            });
            let completed = deps.store.complete_review_task(
                &task.id,
                &why,
                Some(&merge_payload),
                deps.now(),
            )?;
            if !completed && !completed_by_pr_cleanup(deps, &task.id, pr_number)? {
                return Ok(false);
            }
            if let Err(err) = deps.git.fetch_base() {
                eprintln!("aaf: post-merge fetch {}: {err}", task.id);
            }
            if let Err(err) = deps.git.cleanup(&task.id) {
                eprintln!("aaf: post-merge cleanup {}: {err}", task.id);
            }
            if !completed {
                super::finish::close(deps, task)?;
                super::unblock::root_succeeded(deps, task)?;
            }
            Ok(true)
        }
        PrMergeOutcome::Blocked { code, .. } => {
            record_block(deps, &task.id, &p.url, &p.head_sha, &code)?;
            Ok(false)
        }
        // A dry run never comes from the factory; leave the task alone.
        PrMergeOutcome::DryRunClear { .. } | PrMergeOutcome::DryRunPendingHeadLock { .. } => {
            Ok(false)
        }
    }
}

impl Scheduler<'_> {
    /// Retries merges of Review tasks blocked only by time; see [`retry_task`].
    pub(super) fn sweep_merge_retry(&mut self) -> PortResult<()> {
        let repo = Some(self.deps.settings.repo_root.as_str());
        let review = self.deps.store.list_tasks(repo, Some(TaskState::Review))?;
        for task in review {
            match retry_task(self.deps, &task) {
                Ok(true) => {
                    self.clear_restarts_for_task(&task.id);
                    self.summary.done.push(task.id);
                }
                Ok(false) => {}
                Err(err) => self
                    .summary
                    .errors
                    .push(format!("{}: merge retry: {err}", task.id)),
            }
        }
        Ok(())
    }
}
