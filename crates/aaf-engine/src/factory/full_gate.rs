//! The full gate `aaf run --auto` runs on the base branch by itself
//! (`[full_gate]`): scoped gates only test what each task touched, so the
//! whole suite runs once the factory has nothing to do, at most once per
//! `cooldown_hours`, and after `max_days` even while it is busy.
//!
//! A red run opens one P0 task, `Sửa main`, with the failing commands and the
//! commits since the last green run, and holds auto-merge (other tasks land
//! as pull requests) until a later full run is green. A failure that passes
//! when its test command is run again is a flaky test: a P2 task, no hold.

use std::path::Path;

use aaf_domain::config::LandMode;
use aaf_domain::event::EventKind;
use aaf_domain::gate::{GateStatus, Receipt};
use aaf_domain::ports::{PortError, PortResult};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use serde_json::json;

use super::Deps;
use crate::git::cmd::git;

/// Worktree id of the checkout the full gate runs in. It is kept between
/// runs so the build cache (`target/`, `node_modules/`, ...) survives.
pub const WORKTREE_ID: &str = "full-main";
const META_LAST_RUN: &str = "full_gate.last_run_at";
const META_SINCE: &str = "full_gate.since";
const META_GREEN_SHA: &str = "full_gate.green_sha";
const META_RED_SHA: &str = "full_gate.red_sha";
const META_FIX_TASK: &str = "full_gate.fix_task";
const FLAKY_FINGERPRINT: &str = "full-gate:flaky:";
const LOG_TAIL_BYTES: usize = 6000;
const MAX_COMMITS: &str = "50";

/// What one [`tick`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing ran, and why.
    Skipped(&'static str),
    /// Only safe documentation changed since the last green run.
    DocsOnly {
        sha: String,
    },
    Green {
        sha: String,
    },
    /// A test failed once and passed when run again.
    Flaky {
        sha: String,
        task_id: String,
    },
    /// The base branch is red; auto-merge is held until it is green.
    Red {
        sha: String,
        task_id: String,
    },
}

/// The task that has to land while the base is red, if it is.
pub fn fix_task(deps: &Deps) -> PortResult<Option<String>> {
    Ok(deps
        .store
        .get_meta(META_FIX_TASK)?
        .filter(|id| !id.is_empty()))
}

/// [`super::Settings::effective_land`], except that while the base is red
/// only the fix task (and its subtasks) may land by itself.
pub fn land_for(deps: &Deps, task: &Task) -> LandMode {
    let land = deps.settings.effective_land();
    if !land.auto_lands() {
        return land;
    }
    match fix_task(deps) {
        Ok(Some(fix)) if fix != task.root_id => LandMode::Pr,
        _ => land,
    }
}

/// Runs the full gate when it is due. Called by `aaf run --auto` whenever
/// it finds nothing to start; blocks for as long as the gate runs.
pub fn tick(deps: &Deps) -> PortResult<Outcome> {
    let cfg = deps.settings.repo.full_gate;
    if cfg.cooldown_hours == 0 {
        return Ok(Outcome::Skipped("off"));
    }
    let now = deps.now();
    let meta_i64 = |key| -> PortResult<Option<i64>> {
        Ok(deps.store.get_meta(key)?.and_then(|v| v.parse().ok()))
    };
    let since = match meta_i64(META_SINCE)? {
        Some(at) => at,
        None => {
            deps.store.set_meta(META_SINCE, &now.to_string())?;
            now
        }
    };
    let last_run = meta_i64(META_LAST_RUN)?;
    let red = fix_task(deps)?.is_some();
    let hours = |secs: u64| i64::try_from(secs.saturating_mul(3600)).unwrap_or(i64::MAX);
    // While the base is red every change to it is checked again at once:
    // that is how the hold on auto-merge ends.
    if !red && last_run.is_some_and(|at| now - at < hours(cfg.cooldown_hours)) {
        return Ok(Outcome::Skipped("cooldown"));
    }
    if busy(deps)? {
        let overdue = cfg.max_days > 0
            && now - last_run.unwrap_or(since) >= hours(cfg.max_days.saturating_mul(24));
        if !overdue {
            return Ok(Outcome::Skipped("busy"));
        }
    }
    let sha = deps.git.base_sha()?;
    let green = deps
        .store
        .get_meta(META_GREEN_SHA)?
        .filter(|s| !s.is_empty());
    if green.as_deref() == Some(sha.as_str()) {
        return Ok(Outcome::Skipped("unchanged"));
    }
    if red && deps.store.get_meta(META_RED_SHA)?.as_deref() == Some(sha.as_str()) {
        return Ok(Outcome::Skipped("still red"));
    }

    let worktree = deps.git.task_worktree(WORKTREE_ID)?;
    deps.git.reset_to(&worktree, &sha)?;
    let tree = worktree.path;
    if let Some(green) = &green
        && docs_only(deps, &tree, green)
    {
        deps.store.set_meta(META_GREEN_SHA, &sha)?;
        return Ok(Outcome::DocsOnly { sha });
    }
    let receipt = deps.gate.run_full(&tree)?;
    deps.store
        .set_meta(META_LAST_RUN, &deps.now().to_string())?;
    if receipt.status == GateStatus::Clear {
        return green_at(deps, sha);
    }
    let commits = commits_since(&tree, green.as_deref(), &sha);
    if let Some(rerun) = deps.gate.rerun_failed_test(&tree, &receipt)?
        && rerun.passed > 0
    {
        let task_id = flaky_task(deps, &receipt, &sha)?;
        green_at(deps, sha.clone())?;
        return Ok(Outcome::Flaky { sha, task_id });
    }
    let task_id = red_task(deps, &receipt, &sha, &commits)?;
    deps.store.set_meta(META_RED_SHA, &sha)?;
    deps.store.set_meta(META_FIX_TASK, &task_id)?;
    Ok(Outcome::Red { sha, task_id })
}

/// A task of this repository that is Ready, Running or Gating.
fn busy(deps: &Deps) -> PortResult<bool> {
    for state in [TaskState::Ready, TaskState::Running, TaskState::Gating] {
        if !deps
            .store
            .list_tasks(Some(&deps.settings.repo_root), Some(state))?
            .is_empty()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn docs_only(deps: &Deps, tree: &Path, green: &str) -> bool {
    let gate = &deps.settings.repo.gate;
    crate::gate::changes::collect(tree, green, crate::gate::GIT_TIMEOUT)
        .and_then(|set| {
            crate::gate::docs_fast::is_docs_only_in_repo(
                tree,
                &set.changes,
                &gate.head_lock_globs,
                &gate.docs_fast_path_exclude_globs,
                crate::gate::GIT_TIMEOUT,
            )
        })
        .unwrap_or(false)
}

fn green_at(deps: &Deps, sha: String) -> PortResult<Outcome> {
    deps.store.set_meta(META_GREEN_SHA, &sha)?;
    deps.store.set_meta(META_RED_SHA, "")?;
    if let Some(fix) = fix_task(deps)? {
        deps.store.set_meta(META_FIX_TASK, "")?;
        let note = format!("full gate green again at {sha}: auto-merge resumed");
        let _ = deps.store.append_context_note(&fix, &note, deps.now());
    }
    Ok(Outcome::Green { sha })
}

fn commits_since(tree: &Path, green: Option<&str>, sha: &str) -> String {
    let range = green.map_or_else(|| sha.to_string(), |g| format!("{g}..{sha}"));
    git(
        tree,
        &[
            "log",
            "--oneline",
            "--no-decorate",
            "-n",
            MAX_COMMITS,
            &range,
        ],
    )
    .unwrap_or_else(|e| format!("(git log failed: {e})"))
}

fn failures(receipt: &Receipt) -> String {
    let mut out = String::new();
    for reason in &receipt.reasons {
        out.push_str(&format!("- {reason}\n"));
    }
    for cmd in receipt.commands.iter().filter(|c| !c.passed()) {
        let tail = tail(&cmd.log_tail, LOG_TAIL_BYTES);
        out.push_str(&format!(
            "\n`{}` ({}) exit {:?}{}:\n```\n{tail}\n```\n",
            cmd.name,
            cmd.command,
            cmd.exit_code,
            if cmd.timed_out { ", timed out" } else { "" },
        ));
    }
    out
}

fn tail(text: &str, max: usize) -> &str {
    let mut start = text.len().saturating_sub(max);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

fn red_task(deps: &Deps, receipt: &Receipt, sha: &str, commits: &str) -> PortResult<String> {
    let body = format!(
        "The full gate on the base branch failed at {sha}.\n\n{}\nCommits since the last \
         green full gate:\n```\n{commits}\n```\nAuto-merge of other tasks is held until \
         the full gate is green again.",
        failures(receipt)
    );
    if let Some(open) = open_fix_task(deps)? {
        deps.store.append_context_note(&open, &body, deps.now())?;
        return Ok(open);
    }
    let spec = ImmutableSpec {
        description: body,
        acceptance: vec![format!(
            "`aaf gate --full` is Clear on the base branch (failing now at {sha})"
        )],
    };
    create(deps, "Sửa main: gate full đỏ", spec, Priority::P0, None)
}

/// The fix task recorded for the current red run, if it is still open.
fn open_fix_task(deps: &Deps) -> PortResult<Option<String>> {
    let Some(id) = fix_task(deps)? else {
        return Ok(None);
    };
    match deps.store.get_task(&id) {
        Ok(task) if !task.state.is_terminal() => Ok(Some(id)),
        Ok(_) | Err(PortError::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

fn flaky_task(deps: &Deps, receipt: &Receipt, sha: &str) -> PortResult<String> {
    let name = receipt
        .commands
        .iter()
        .find(|c| !c.passed())
        .map_or("test", |c| c.name.as_str());
    let fingerprint = format!("{FLAKY_FINGERPRINT}{name}");
    let open = deps
        .store
        .list_tasks(Some(&deps.settings.repo_root), None)?
        .into_iter()
        .find(|t| !t.state.is_terminal() && t.fingerprint.as_deref() == Some(&fingerprint));
    let body = format!(
        "The full gate on the base branch failed at {sha} and passed when the failing \
         command was run again: a flaky test.\n\n{}",
        failures(receipt)
    );
    if let Some(task) = open {
        deps.store
            .append_context_note(&task.id, &body, deps.now())?;
        return Ok(task.id);
    }
    let spec = ImmutableSpec {
        description: body,
        acceptance: vec!["the test passes on every run".to_string()],
    };
    create(
        deps,
        "Sửa test flaky",
        spec,
        Priority::P2,
        Some(fingerprint),
    )
}

fn create(
    deps: &Deps,
    title: &str,
    spec: ImmutableSpec,
    priority: Priority,
    fingerprint: Option<String>,
) -> PortResult<String> {
    let now = deps.now();
    let repo = &deps.settings.repo_root;
    for attempt in 0_u32..100 {
        let seed = format!("{repo}\nfull-gate\n{title}\n{now}\n{attempt}");
        let id = format!("t-{}", &aaf_fp::sha256_hex(seed.as_bytes())[..8]);
        match deps.store.get_task(&id) {
            Err(PortError::NotFound(_)) => {}
            Ok(_) => continue,
            Err(e) => return Err(e),
        }
        let mut task = Task::new_root(id.clone(), repo.clone(), title, spec, priority, now);
        task.state = TaskState::Ready;
        task.fingerprint = fingerprint;
        deps.store.insert_task(&task)?;
        deps.emit(
            EventKind::TaskCreated,
            &id,
            json!({"title": title, "source": "full_gate"}),
        )?;
        return Ok(id);
    }
    Err(PortError::Conflict(
        "could not allocate a task id for the full gate".to_string(),
    ))
}
