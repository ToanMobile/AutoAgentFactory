//! Deterministic inline task creation for `aaf run "<goal>"` (Plan v3 §3.4).

use std::collections::HashSet;
use std::path::Path;

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{Clock, EventSink, PortError, TaskStore};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::factory::adapters::SystemClock;
use aaf_engine::factory::schedule_order::normalize_scope;
use serde_json::json;

use super::super::board::store::err;
use crate::output::{CmdError, CmdOutput};

const MAX_TITLE_CHARS: usize = 200;
const DEFAULT_ACCEPTANCE: &str =
    "Implementation satisfies the goal and passes the repository gate.";
const RESERVED_GOAL_TYPOS: [&str; 6] = ["auto", "once", "detach", "status", "merge", "stop"];

pub(super) enum GoalOutcome {
    Created(String),
    Refused(CmdOutput),
}

fn new_task_id(
    store: &dyn TaskStore,
    repo: &str,
    title: &str,
    now: i64,
) -> Result<String, CmdError> {
    for attempt in 0_u32..100 {
        let seed = format!("{repo}\n{title}\n{now}\n{}\n{attempt}", std::process::id());
        let id = format!("t-{}", &aaf_fp::sha256_hex(seed.as_bytes())[..8]);
        match store.get_task(&id) {
            Err(PortError::NotFound(_)) => return Ok(id),
            Ok(_) => {}
            Err(other) => return Err(err("store", other)),
        }
    }
    Err(CmdError::new("could not find a free task id"))
}

fn derive_title(goal: &str) -> String {
    let first_line = goal
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let cleaned: String = first_line
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    cleaned
        .trim()
        .chars()
        .take(MAX_TITLE_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Validates an inline `aaf run "<goal>"` argument and deterministically
/// inserts a Ready root task into `db` before any `--detach` fork.
pub(super) fn create_goal_task(
    raw_goal: &str,
    accept: &[String],
    priority_arg: Option<&str>,
    scopes: &[String],
    repo: &Path,
    db: &Db,
) -> Result<GoalOutcome, CmdError> {
    let trimmed = raw_goal.trim();
    if trimmed.is_empty() {
        let msg = "goal is empty: pass a non-empty task description";
        return Ok(GoalOutcome::Refused(
            CmdOutput::policy("refused", msg)
                .field("refused_kind", json!("usage"))
                .field("reason", json!(msg)),
        ));
    }
    if !trimmed.contains(char::is_whitespace) {
        let lower = trimmed.to_ascii_lowercase();
        if RESERVED_GOAL_TYPOS.contains(&lower.as_str()) {
            let msg = format!(
                "refused ambiguous 1-word goal {trimmed:?}: did you mean `--{lower}` or \
                 `aaf {lower}`? Pass a full task description to create a task."
            );
            return Ok(GoalOutcome::Refused(
                CmdOutput::policy("refused", msg.clone())
                    .field("refused_kind", json!("usage"))
                    .field("reason", json!(msg)),
            ));
        }
    }

    let title = derive_title(trimmed);
    if title.is_empty() {
        let msg = "goal has no usable title line";
        return Ok(GoalOutcome::Refused(
            CmdOutput::policy("refused", msg)
                .field("refused_kind", json!("usage"))
                .field("reason", json!(msg)),
        ));
    }
    let acceptance: Vec<String> = accept
        .iter()
        .map(|a| a.trim())
        .filter(|a| !a.is_empty())
        .map(str::to_string)
        .collect();
    let acceptance = if acceptance.is_empty() {
        vec![DEFAULT_ACCEPTANCE.to_string()]
    } else {
        acceptance
    };
    let priority_raw = priority_arg.unwrap_or("P2");
    let priority = Priority::parse(priority_raw)
        .ok_or_else(|| CmdError::new(format!("--priority {priority_raw}: use P0..P3")))?;
    let mut scope_paths = Vec::with_capacity(scopes.len());
    let mut seen_scopes: HashSet<String> = HashSet::with_capacity(scopes.len());
    for raw in scopes {
        let norm =
            normalize_scope(raw).map_err(|msg| CmdError::new(format!("--scope {raw}: {msg}")))?;
        if seen_scopes.insert(norm.to_ascii_lowercase()) {
            scope_paths.push(norm);
        }
    }

    let repo_str = repo.to_string_lossy().into_owned();
    let now = SystemClock.now();
    let id = new_task_id(db, &repo_str, &title, now)?;
    let spec = ImmutableSpec {
        description: trimmed.to_string(),
        acceptance,
    };
    let mut task = Task::new_root(id, repo_str, title, spec, priority, now);
    task.state = TaskState::Ready;
    task.scope_paths = scope_paths;
    db.insert_task(&task).map_err(|e| err("store", e))?;
    let payload = json!({
        "title": task.title,
        "source": "aaf_run_goal",
        "scope_paths": task.scope_paths,
    });
    let event = FactoryEvent::new(EventKind::TaskCreated, payload.to_string(), now);
    db.emit(&event.for_task(&task.id))
        .map_err(|e| err("store", e))?;
    Ok(GoalOutcome::Created(task.id))
}
