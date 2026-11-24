//! PR feedback loop: turns failing CI checks and allowlisted owner review
//! threads on an open task PR into bounded automatic rework rounds (`Review -> Ready`).

use std::path::Path;

use aaf_domain::budget::BudgetCheck;
use aaf_domain::event::EventKind;
use aaf_domain::pipeline::MAX_PR_REWORK_ROUNDS_CAP;
use aaf_domain::ports::{PortError, PortResult};
use aaf_domain::task::{Task, TaskState};
use serde_json::json;

use super::Deps;
use super::pr_feedback_note::{ValidThread, build_note};
use super::scheduler::Scheduler;
use super::worker::ensure_budget;
use crate::git::pr_feedback_gh::{PrFeedbackSnapshot, PrRef, parse_pr_url};
use crate::git::worktree::BRANCH_PREFIX;

/// Minimum interval (seconds) between `gh` polls for the same Review task.
pub(crate) const PR_FEEDBACK_THROTTLE_SECS: i64 = 300;
const BOT_MARKER_PREFIX: &str = "🤖 aaf-bot:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    Rework {
        note: String,
        digest: String,
        ci_failed: Vec<String>,
        threads: usize,
        round: u32,
    },
    Exhausted {
        digest: String,
        ci_failed: Vec<String>,
        threads: usize,
        round: u32,
    },
    Skip {
        digest: String,
        why: String,
    },
    Nothing,
}

pub(crate) struct EvalContext<'a> {
    pub pr: &'a PrRef,
    pub expected_branch: &'a str,
    pub owner_allowlist: &'a [String],
    pub max_rounds: u32,
    pub prior_rework_rounds: u32,
    pub last_feedback_digest: Option<&'a str>,
    pub last_skip_why: Option<&'a str>,
    pub comment_cutoff_ts: i64,
    pub budget_ok: bool,
    pub overlaps_run: &'a dyn Fn(i64) -> PortResult<bool>,
}

fn short_digest(material: &str) -> String {
    let hex = aaf_fp::sha256_hex(material.as_bytes());
    hex[..16].to_string()
}

fn skip_decision(ctx: &EvalContext<'_>, head_oid: &str, why: &str) -> Decision {
    let material = json!({ "skip": why, "head_oid": head_oid }).to_string();
    let digest = short_digest(&material);
    if ctx.last_feedback_digest == Some(digest.as_str()) && ctx.last_skip_why == Some(why) {
        return Decision::Nothing;
    }
    Decision::Skip {
        digest,
        why: why.to_string(),
    }
}

fn login_allowlisted(allowlist: &[String], login: &str) -> bool {
    let login = login.trim();
    if login.is_empty() {
        return false;
    }
    allowlist.iter().any(|entry| {
        let e = entry.trim();
        !e.is_empty() && e.eq_ignore_ascii_case(login)
    })
}

/// Pure decision engine for PR feedback (B4 + D2 + D4 + D10).
pub(crate) fn evaluate(
    snapshot: &PrFeedbackSnapshot,
    ctx: &EvalContext<'_>,
) -> PortResult<Decision> {
    if ctx.max_rounds == 0 {
        return Ok(Decision::Nothing);
    }
    let snap_url = snapshot.url.trim().trim_end_matches('/');
    let pr_url = ctx.pr.url.trim().trim_end_matches('/');
    if snap_url != pr_url {
        return Ok(skip_decision(ctx, &snapshot.head_oid, "pr url mismatch"));
    }
    if snapshot.is_cross_repository {
        return Ok(skip_decision(ctx, &snapshot.head_oid, "fork PR"));
    }
    if snapshot.head_branch != ctx.expected_branch {
        return Ok(skip_decision(
            ctx,
            &snapshot.head_oid,
            "head branch mismatch",
        ));
    }
    if !snapshot.state.eq_ignore_ascii_case("OPEN") {
        return Ok(Decision::Nothing);
    }

    let mut ci_failed: Vec<String> = snapshot
        .checks
        .iter()
        .filter(|c| {
            c.bucket.eq_ignore_ascii_case("fail") || c.bucket.eq_ignore_ascii_case("cancel")
        })
        .map(|c| c.name.clone())
        .collect();
    ci_failed.sort();
    ci_failed.dedup();

    let mut valid_threads: Vec<ValidThread<'_>> = Vec::new();
    let mut valid_comment_ids: Vec<&str> = Vec::new();
    if !ctx.owner_allowlist.is_empty() {
        for thread in &snapshot.threads {
            if thread.is_resolved || thread.is_outdated {
                continue;
            }
            let mut comments = Vec::new();
            for comment in &thread.comments {
                if !login_allowlisted(ctx.owner_allowlist, &comment.author_login) {
                    continue;
                }
                if comment.body.trim_start().starts_with(BOT_MARKER_PREFIX) {
                    continue;
                }
                let Some(ts) = comment.created_at_unix else {
                    continue;
                };
                if ts <= ctx.comment_cutoff_ts {
                    continue;
                }
                if (ctx.overlaps_run)(ts)? {
                    continue;
                }
                valid_comment_ids.push(comment.id.as_str());
                comments.push(comment);
            }
            if !comments.is_empty() {
                valid_threads.push(ValidThread {
                    path: thread.path.as_deref(),
                    comments,
                });
            }
        }
    }

    if ci_failed.is_empty() && valid_threads.is_empty() {
        return Ok(Decision::Nothing);
    }

    valid_comment_ids.sort_unstable();
    valid_comment_ids.dedup();
    let digest_material = json!({
        "ci": ci_failed,
        "comments": valid_comment_ids,
        "head_oid": snapshot.head_oid,
    })
    .to_string();
    let digest = short_digest(&digest_material);

    if ctx.last_skip_why.is_none() && ctx.last_feedback_digest == Some(digest.as_str()) {
        return Ok(Decision::Nothing);
    }

    if ctx.prior_rework_rounds >= ctx.max_rounds {
        return Ok(Decision::Exhausted {
            digest,
            ci_failed,
            threads: valid_threads.len(),
            round: ctx.prior_rework_rounds,
        });
    }

    if !ctx.budget_ok {
        if ctx.last_feedback_digest == Some(digest.as_str()) && ctx.last_skip_why == Some("budget")
        {
            return Ok(Decision::Nothing);
        }
        return Ok(Decision::Skip {
            digest,
            why: "budget".to_string(),
        });
    }

    let note = build_note(&ci_failed, &snapshot.failed_logs, &valid_threads);
    Ok(Decision::Rework {
        note,
        digest,
        ci_failed,
        threads: valid_threads.len(),
        round: ctx.prior_rework_rounds.saturating_add(1),
    })
}

struct TaskPrHistory {
    pr: PrRef,
    prior_rework_rounds: u32,
    last_feedback_digest: Option<String>,
    last_skip_why: Option<String>,
    comment_cutoff_ts: i64,
}

fn load_pr_history(deps: &Deps, task_id: &str) -> PortResult<Option<TaskPrHistory>> {
    let events = deps.store.events_for_task_by_kinds(
        task_id,
        &[EventKind::PrOpened, EventKind::PrFeedback],
        u32::MAX,
    )?;
    let mut latest_pr: Option<(PrRef, i64)> = None;
    let mut prior_rework_rounds: u32 = 0;
    let mut last_feedback_digest: Option<String> = None;
    let mut last_skip_why: Option<String> = None;
    let mut last_rework_ts: Option<i64> = None;

    // Events are returned newest-first.
    for ev in events {
        let payload = serde_json::from_str::<serde_json::Value>(&ev.payload_json).ok();
        match ev.kind {
            EventKind::PrOpened => {
                if latest_pr.is_none()
                    && let Some(url) = payload.as_ref().and_then(|p| p["url"].as_str())
                    && let Some(pr) = parse_pr_url(url)
                {
                    latest_pr = Some((pr, ev.created_at));
                }
            }
            EventKind::PrFeedback => {
                let action = payload
                    .as_ref()
                    .and_then(|p| p["action"].as_str())
                    .unwrap_or("");
                if last_feedback_digest.is_none() {
                    last_feedback_digest = payload
                        .as_ref()
                        .and_then(|p| p["digest"].as_str())
                        .map(str::to_string);
                    if action == "skipped" {
                        last_skip_why = payload
                            .as_ref()
                            .and_then(|p| p["why"].as_str())
                            .map(str::to_string);
                    }
                }
                if action == "rework" {
                    prior_rework_rounds = prior_rework_rounds.saturating_add(1);
                    if last_rework_ts.is_none() {
                        last_rework_ts = Some(ev.created_at);
                    }
                }
            }
            _ => {}
        }
    }

    let Some((pr, pr_opened_ts)) = latest_pr else {
        return Ok(None);
    };
    Ok(Some(TaskPrHistory {
        pr,
        prior_rework_rounds,
        last_feedback_digest,
        last_skip_why,
        comment_cutoff_ts: last_rework_ts.unwrap_or(pr_opened_ts),
    }))
}

/// Evaluates and applies PR feedback for one `Review` task.
/// Returns `Ok(true)` when the task was moved back to `Ready` for rework.
pub(crate) fn check_task(deps: &Deps, task: &Task) -> PortResult<bool> {
    if task.state != TaskState::Review {
        return Ok(false);
    }
    let max_rounds = deps
        .settings
        .repo
        .pipeline
        .max_pr_rework_rounds
        .min(MAX_PR_REWORK_ROUNDS_CAP);
    if max_rounds == 0 {
        return Ok(false);
    }
    let Some(history) = load_pr_history(deps, &task.id)? else {
        return Ok(false);
    };
    let expected_branch = format!("{BRANCH_PREFIX}{}", task.id);
    let repo_root = Path::new(&deps.settings.repo_root);
    let snapshot = match deps
        .git
        .pr_feedback(repo_root, &history.pr, &expected_branch)
    {
        Ok(Some(snap)) => snap,
        Ok(None) => return Ok(false),
        Err(err) => {
            eprintln!("aaf: pr_feedback {} ({}): {err}", task.id, history.pr.url);
            return Ok(false);
        }
    };

    let budget = ensure_budget(deps, &task.root_id)?;
    let budget_ok = budget.check() == BudgetCheck::Ok;
    let now = deps.now();
    let overlaps = |ts: i64| {
        deps.store
            .has_overlapping_run(&deps.settings.repo_root, ts, now)
    };
    let ctx = EvalContext {
        pr: &history.pr,
        expected_branch: &expected_branch,
        owner_allowlist: &deps.settings.owner_github_allowlist,
        max_rounds,
        prior_rework_rounds: history.prior_rework_rounds,
        last_feedback_digest: history.last_feedback_digest.as_deref(),
        last_skip_why: history.last_skip_why.as_deref(),
        comment_cutoff_ts: history.comment_cutoff_ts,
        budget_ok,
        overlaps_run: &overlaps,
    };

    match evaluate(&snapshot, &ctx)? {
        Decision::Nothing => Ok(false),
        Decision::Rework {
            note,
            digest,
            ci_failed,
            threads,
            round,
        } => {
            if deps.store.get_task(&task.id)?.state != TaskState::Review {
                return Ok(false);
            }
            deps.store.append_context_note(&task.id, &note, now)?;
            deps.emit(
                EventKind::PrFeedback,
                &task.id,
                json!({
                    "action": "rework",
                    "round": round,
                    "pr": history.pr.number,
                    "head_oid": snapshot.head_oid,
                    "ci_failed": ci_failed,
                    "threads": threads,
                    "digest": digest,
                    "why": "",
                }),
            )?;
            match deps.move_to(&task.id, TaskState::Ready, None, "pr_feedback") {
                Ok(()) => Ok(true),
                Err(PortError::Invalid(_)) => Ok(false),
                Err(err) => Err(err),
            }
        }
        Decision::Exhausted {
            digest,
            ci_failed,
            threads,
            round,
        } => {
            deps.emit(
                EventKind::PrFeedback,
                &task.id,
                json!({
                    "action": "exhausted",
                    "round": round,
                    "pr": history.pr.number,
                    "head_oid": snapshot.head_oid,
                    "ci_failed": ci_failed,
                    "threads": threads,
                    "digest": digest,
                    "why": "max_pr_rework_rounds reached",
                }),
            )?;
            Ok(false)
        }
        Decision::Skip { digest, why } => {
            deps.emit(
                EventKind::PrFeedback,
                &task.id,
                json!({
                    "action": "skipped",
                    "round": history.prior_rework_rounds,
                    "pr": history.pr.number,
                    "head_oid": snapshot.head_oid,
                    "ci_failed": Vec::<String>::new(),
                    "threads": 0,
                    "digest": digest,
                    "why": why,
                }),
            )?;
            Ok(false)
        }
    }
}

impl Scheduler<'_> {
    /// Checks open `Review` tasks for CI failures or owner review threads,
    /// throttled to at most once per [`PR_FEEDBACK_THROTTLE_SECS`] per task.
    pub(super) fn sweep_pr_feedback(&mut self) -> PortResult<()> {
        let cap = self
            .deps
            .settings
            .repo
            .pipeline
            .max_pr_rework_rounds
            .min(MAX_PR_REWORK_ROUNDS_CAP);
        if cap == 0 {
            return Ok(());
        }
        let repo = Some(self.deps.settings.repo_root.as_str());
        let review = self.deps.store.list_tasks(repo, Some(TaskState::Review))?;
        let now = self.deps.now();
        for task in review {
            if self
                .state
                .last_pr_feedback
                .get(&task.id)
                .is_some_and(|&t| now.saturating_sub(t) < PR_FEEDBACK_THROTTLE_SECS)
            {
                continue;
            }
            self.state.last_pr_feedback.insert(task.id.clone(), now);
            match check_task(self.deps, &task) {
                Ok(true) => self.requeue_if_ready(task.id, TaskState::Ready),
                Ok(false) => {}
                Err(err) => eprintln!("aaf: pr_feedback {}: {err}", task.id),
            }
        }
        Ok(())
    }
}
