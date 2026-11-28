//! The adopt lane (`aaf board adopt`): an `Adopting` task is a pull request
//! opened outside the factory, pinned at one head. Each turn walks verify ->
//! review -> sign -> merge on that head and ends in exactly one of: the task
//! `Done`, the task `Cancelled` with a reason (and a comment on the PR), or
//! `tasks.retry_after` set for a later turn. It never blocks the task, never
//! records a root result and never counts toward `consecutive_root_fails`: the
//! factory has no stake in a PR somebody else wrote.
//!
//! Only `aaf run --auto` runs it, and only when its `StageRouter` carries an
//! [`AdoptEnv`] (the CLI wires `gh` there; the engine never picks one).

mod abort;
mod env;
mod flow;
mod lane;
mod lock_view;
mod merge_step;
mod pin;
mod retry;
mod review;
mod sign;
mod verify;

pub use env::AdoptEnv;

use aaf_domain::ports::PortResult;

use super::Deps;
use super::stage_router::StageRouter;
use flow::Exit;
use lane::{Lane, PinError};

/// What one lane job reports back to the scheduler.
pub(crate) struct AdoptDone {
    pub task_id: String,
    /// Something the turn could not record or post; the task state is as the
    /// turn left it.
    pub error: Option<String>,
}

/// One turn for `task_id`. Every outcome is applied here; nothing is returned
/// for the generic worker path to act on.
pub(crate) fn run_job(deps: &Deps, router: &StageRouter, task_id: &str) -> AdoptDone {
    let error = match router.adopt.as_deref() {
        None => None,
        Some(env) => match Lane::new(deps, router, env, task_id) {
            Err(pin) => unpinned(deps, task_id, pin),
            Ok(lane) => {
                let result = lane.steps();
                lane.finish(result)
            }
        },
    };
    AdoptDone {
        task_id: task_id.to_string(),
        error,
    }
}

/// A task whose pin cannot be had: invalid is cancelled (there is no PR to
/// comment on), unreadable is left for the next turn.
fn unpinned(deps: &Deps, task_id: &str, pin: PinError) -> Option<String> {
    match pin {
        PinError::Invalid(reason) => abort::cancel_unpinned(deps, task_id, "head_changed", &reason),
        PinError::Unreadable(note) => Some(note),
    }
}

/// What a panic inside a turn becomes: the task's runs the panic left
/// `running` are reaped the way a restart would (`sweep::recover`), so `board
/// cancel` is not refused and nothing is left to a daemon restart; then a retry,
/// never a blocked task.
pub(crate) fn on_panic(
    deps: &Deps,
    router: &StageRouter,
    task_id: &str,
    reap: &dyn Fn(&str) -> PortResult<()>,
) -> AdoptDone {
    let reaped = super::sweep::reap_running_runs(deps, task_id, reap)
        .err()
        .map(|err| format!("{task_id}: runs left by the panic not reaped: {err}"));
    let error = match router.adopt.as_deref() {
        None => None,
        Some(env) => match Lane::new(deps, router, env, task_id) {
            Err(pin) => unpinned(deps, task_id, pin),
            Ok(lane) => lane.finish(Err(Exit::retry(
                "lane_panic",
                "the adopt lane panicked on this turn",
            ))),
        },
    };
    let note = [reaped, error]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("; ");
    AdoptDone {
        task_id: task_id.to_string(),
        error: Some(if note.is_empty() {
            format!("{task_id}: the adopt lane panicked; it will retry")
        } else {
            note
        }),
    }
}
