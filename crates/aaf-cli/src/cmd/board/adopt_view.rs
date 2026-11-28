//! What `aaf board list` and `show` say about an adopted pull request: the PR,
//! the head that was pinned, the phase the lane is in, how often it retried and
//! when it tries next, and, once it is over, the code and reason it was
//! cancelled with.
//!
//! Only tasks that are `Adopting` or carry an adoption fingerprint are looked
//! at, so a board without adopted PRs costs no extra query.

use std::collections::HashMap;

use aaf_domain::event::{EventKind, FactoryEvent, PrAdoptedPayload};
use aaf_domain::ports::{Clock, EventSink};
use aaf_domain::task::{Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::factory::adapters::SystemClock;
use serde_json::{Value, json};

use super::port;
use crate::output::CmdError;

/// Retries are counted from the most recent `AdoptStep` events.
const STEP_EVENTS: u32 = 1_000;
/// What a task cancelled by `aaf board cancel` shows: no `AdoptAborted` is
/// written for it, so the code is the one the checklist names.
const CANCELLED_BY_USER: &str = "cancelled_by_user";

pub(super) struct AdoptInfo {
    pub(super) json: Value,
    /// One line for `board list`.
    pub(super) short: String,
    /// Lines for `board show`.
    pub(super) detail: Vec<String>,
}

fn is_adoption(task: &Task) -> bool {
    task.state == TaskState::Adopting
        || task
            .fingerprint
            .as_deref()
            .is_some_and(|fp| fp.starts_with("gh-pr:"))
}

fn payload(event: &FactoryEvent) -> Value {
    serde_json::from_str(&event.payload_json).unwrap_or(Value::Null)
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn short_sha(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

fn latest(db: &Db, id: &str, kind: EventKind) -> Result<Option<FactoryEvent>, CmdError> {
    Ok(db
        .events_for_task_by_kinds(id, &[kind], 1)
        .map_err(port)?
        .into_iter()
        .next())
}

fn describe(db: &Db, task: &Task, now: i64) -> Result<Option<AdoptInfo>, CmdError> {
    if !is_adoption(task) {
        return Ok(None);
    }
    let id = task.id.as_str();
    let pin: Option<PrAdoptedPayload> = latest(db, id, EventKind::PrAdopted)?
        .and_then(|e| serde_json::from_str(&e.payload_json).ok());
    let steps = db
        .events_for_task_by_kinds(id, &[EventKind::AdoptStep], STEP_EVENTS)
        .map_err(port)?;
    let step = steps.first().map(payload).unwrap_or(Value::Null);
    let retries = steps
        .iter()
        .filter(|e| payload(e).get("retry").and_then(Value::as_bool) == Some(true))
        .count();
    let last_retry = steps
        .iter()
        .map(payload)
        .find(|p| p.get("retry").and_then(Value::as_bool) == Some(true))
        .unwrap_or(Value::Null);
    let aborted = match latest(db, id, EventKind::AdoptAborted)? {
        Some(event) if task.state == TaskState::Cancelled => {
            let p = payload(&event);
            Some((text(&p, "code").to_string(), text(&p, "reason").to_string()))
        }
        None if task.state == TaskState::Cancelled => {
            // `board cancel` ends an adoption without an `AdoptAborted`.
            let why = latest(db, id, EventKind::TaskStateChanged)?
                .map(|e| payload(&e))
                .map(|p| {
                    let reason = text(&p, "reason");
                    if reason.is_empty() {
                        text(&p, "why")
                    } else {
                        reason
                    }
                    .to_string()
                })
                .unwrap_or_default();
            Some((CANCELLED_BY_USER.to_string(), why))
        }
        _ => None,
    };

    let adopting = task.state == TaskState::Adopting;
    let phase = adopting.then(|| text(&step, "phase").to_string());
    let retry_in = task.retry_after.filter(|_| adopting).map(|at| at - now);
    let mut detail = Vec::new();
    let mut short = Vec::new();
    if let Some(pin) = &pin {
        detail.push(format!(
            "  adopt: PR #{} {} ({} -> {}) by {}",
            pin.pr_number, pin.url, pin.head_ref, pin.base_ref, pin.author_login
        ));
        detail.push(format!("  adopt pinned head: {}", pin.head_sha));
        short.push(format!(
            "PR #{} head {}",
            pin.pr_number,
            short_sha(&pin.head_sha)
        ));
    }
    if let Some(phase) = phase.as_deref().filter(|p| !p.is_empty()) {
        let attempt = step.get("attempt").and_then(Value::as_u64).unwrap_or(0);
        detail.push(format!("  adopt phase: {phase} (attempt {attempt})"));
        short.push(format!("phase={phase}"));
    }
    if adopting {
        short.push(format!("retry={retries}"));
        let mut line = format!("  adopt retries: {retries}");
        if let Some(wait) = retry_in {
            line.push_str(&format!(", next turn in {wait}s"));
            short.push(format!("retry_after={}", task.retry_after.unwrap_or(0)));
        }
        detail.push(line);
        if retries > 0 {
            detail.push(format!(
                "  adopt last retry: {}: {}",
                text(&last_retry, "code"),
                text(&last_retry, "reason")
            ));
        }
    }
    if let Some((code, reason)) = &aborted {
        detail.push(format!("  adopt aborted: {code}: {reason}"));
        short.push(format!("aborted={code}"));
    }

    let info = json!({
        "pr_number": pin.as_ref().map(|p| p.pr_number),
        "url": pin.as_ref().map(|p| p.url.clone()),
        "head_sha": pin.as_ref().map(|p| p.head_sha.clone()),
        "head_ref": pin.as_ref().map(|p| p.head_ref.clone()),
        "base_ref": pin.as_ref().map(|p| p.base_ref.clone()),
        "author_login": pin.as_ref().map(|p| p.author_login.clone()),
        "declared_executor": pin.as_ref().and_then(|p| p.declared_executor.clone()),
        "phase": phase.filter(|p| !p.is_empty()),
        "attempt": step.get("attempt").cloned(),
        "retries": adopting.then_some(retries),
        "retry_after": task.retry_after.filter(|_| adopting),
        "last_retry": (adopting && retries > 0).then_some(last_retry),
        "abort": aborted.as_ref().map(|(code, reason)| json!({ "code": code, "reason": reason })),
    });
    Ok(Some(AdoptInfo {
        json: info,
        short: format!("    {}", short.join(" ")),
        detail,
    }))
}

/// The adoption view of every task in `tasks` that has one, by task id.
pub(super) fn for_tasks(db: &Db, tasks: &[Task]) -> Result<HashMap<String, AdoptInfo>, CmdError> {
    let now = SystemClock.now();
    let mut found = HashMap::new();
    for task in tasks {
        if let Some(info) = describe(db, task, now)? {
            found.insert(task.id.clone(), info);
        }
    }
    Ok(found)
}
