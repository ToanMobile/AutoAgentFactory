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
const META_LAST_RUN: &str = "last_run_at";
const META_SINCE: &str = "since";
const META_FAILED_AT: &str = "failed_at";
const META_GREEN_SHA: &str = "green_sha";
const META_RED_SHA: &str = "red_sha";
const META_FIX_TASK: &str = "fix_task";
/// After a run that could not finish (git, the worktree, the receipt), the
/// next try waits this long instead of every idle poll.
const RETRY_AFTER_ERROR_SECS: i64 = 3600;
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
    /// The gate could not verify the base (nothing failed); no hold.
    Unverified {
        sha: String,
        reasons: Vec<String>,
    },
    /// The base branch is red; auto-merge is held until it is green.
    Red {
        sha: String,
        task_id: String,
    },
}

/// The meta key of `name` for this repository: every repository on the
/// machine shares one database.
fn key(deps: &Deps, name: &str) -> String {
    let repo = aaf_fp::repo_id(Path::new(&deps.settings.repo_root));
    format!("full_gate.{repo}.{name}")
}

fn meta(deps: &Deps, name: &str) -> PortResult<Option<String>> {
    Ok(deps
        .store
        .get_meta(&key(deps, name))?
        .filter(|v| !v.is_empty()))
}

fn set(deps: &Deps, name: &str, value: &str) -> PortResult<()> {
    deps.store.set_meta(&key(deps, name), value)
}

/// The task that has to land while the base is red, if it is. A fix task
/// someone cancelled, or one that is gone, holds nothing any more: the next
/// change to the base is checked again and opens a new one if still red.
pub fn fix_task(deps: &Deps) -> PortResult<Option<String>> {
    let Some(id) = meta(deps, META_FIX_TASK)? else {
        return Ok(None);
    };
    match deps.store.get_task(&id) {
        Ok(task) if task.state == TaskState::Cancelled => Ok(None),
        Ok(_) => Ok(Some(id)),
        Err(PortError::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// [`super::Settings::effective_land`], except that while the base is red
/// only the fix task (and its subtasks) may land by itself. Turning the
/// full gate off (`cooldown_hours = 0`) also ends the hold.
pub fn land_for(deps: &Deps, task: &Task) -> LandMode {
    let land = deps.settings.effective_land();
    if !land.auto_lands() || deps.settings.repo.full_gate.cooldown_hours == 0 {
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
    if deps.settings.repo.commands.test_full.trim().is_empty() {
        return Ok(Outcome::Skipped("no test_full"));
    }
    let now = deps.now();
    let at =
        |name| -> PortResult<Option<i64>> { Ok(meta(deps, name)?.and_then(|v| v.parse().ok())) };
    let since = match at(META_SINCE)? {
        Some(at) => at,
        None => {
            set(deps, META_SINCE, &now.to_string())?;
            now
        }
    };
    if at(META_FAILED_AT)?.is_some_and(|t| now - t < RETRY_AFTER_ERROR_SECS) {
        return Ok(Outcome::Skipped("error backoff"));
    }
    let last_run = at(META_LAST_RUN)?;
    let red = fix_task(deps)?.is_some();
    let hours = |h: u64| i64::try_from(h.saturating_mul(3600)).unwrap_or(i64::MAX);
    // While the base is red every change to it is checked again at once:
    // that is how the hold on auto-merge ends.
    if !red && last_run.is_some_and(|t| now - t < hours(cfg.cooldown_hours)) {
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
    let green = meta(deps, META_GREEN_SHA)?;
    if green.as_deref() == Some(sha.as_str()) {
        return Ok(Outcome::Skipped("unchanged"));
    }
    if red && meta(deps, META_RED_SHA)?.as_deref() == Some(sha.as_str()) {
        return Ok(Outcome::Skipped("still red"));
    }
    let outcome = run_at(deps, &sha, green.as_deref());
    if outcome.is_err() {
        set(deps, META_FAILED_AT, &deps.now().to_string())?;
    }
    outcome
}

fn run_at(deps: &Deps, sha: &str, green: Option<&str>) -> PortResult<Outcome> {
    let worktree = deps.git.task_worktree(WORKTREE_ID)?;
    deps.git.reset_to(&worktree, sha)?;
    let tree = worktree.path;
    if let Some(green) = green
        && docs_only(deps, &tree, green)
    {
        green_at(deps, sha)?;
        return Ok(Outcome::DocsOnly {
            sha: sha.to_string(),
        });
    }
    let receipt = deps.gate.run_full(&tree)?;
    set(deps, META_LAST_RUN, &deps.now().to_string())?;
    let sha = sha.to_string();
    match receipt.status {
        GateStatus::Clear => {
            green_at(deps, &sha)?;
            return Ok(Outcome::Green { sha });
        }
        // Nothing failed: the gate could not say (the tree moved under it,
        // a check it needs is missing). Not a reason to hold anything.
        GateStatus::Unverified => {
            set(deps, META_FAILED_AT, &deps.now().to_string())?;
            return Ok(Outcome::Unverified {
                sha,
                reasons: receipt.reasons,
            });
        }
        GateStatus::Blocked => {}
    }
    let commits = commits_since(&tree, green, &sha);
    if let Some(rerun) = deps.gate.rerun_failed_test(&tree, &receipt)?
        && rerun.passed > 0
    {
        let task_id = flaky_task(deps, &receipt, &sha)?;
        green_at(deps, &sha)?;
        return Ok(Outcome::Flaky { sha, task_id });
    }
    let task_id = red_task(deps, &receipt, &sha, &commits)?;
    set(deps, META_RED_SHA, &sha)?;
    set(deps, META_FIX_TASK, &task_id)?;
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

fn green_at(deps: &Deps, sha: &str) -> PortResult<()> {
    set(deps, META_GREEN_SHA, sha)?;
    set(deps, META_RED_SHA, "")?;
    set(deps, META_FAILED_AT, "")?;
    if let Some(fix) = meta(deps, META_FIX_TASK)? {
        set(deps, META_FIX_TASK, "")?;
        let note = format!("full gate green again at {sha}: auto-merge resumed");
        let _ = deps.store.append_context_note(&fix, &note, deps.now());
    }
    Ok(())
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
    let task = deps.store.get_task(&id)?;
    Ok((!task.state.is_terminal()).then_some(id))
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
