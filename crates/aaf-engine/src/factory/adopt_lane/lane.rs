//! One turn of the adopt lane: verify -> review -> sign -> merge, restarted
//! from the top on every attempt. Nothing is carried between attempts except
//! the attempt count (see `retry`): a stored partial verdict or receipt is
//! never read back.

use std::cell::Cell;
use std::path::PathBuf;

use aaf_domain::event::{EventKind, PrAdoptedPayload};
use aaf_domain::ports::PortResult;
use aaf_domain::task::TaskState;
use serde_json::json;

use super::env::AdoptEnv;
use super::flow::{Done, Exit, Flow};
use crate::factory::stage_router::StageRouter;
use crate::factory::{Deps, full_gate};
use crate::pr_ops::{PrMergeRequest, fetch_base};

/// Why a lane could not be built for a task.
pub(super) enum PinError {
    /// The pin is missing or malformed: cancel.
    Invalid(String),
    /// The store could not be read: leave the task alone, note it.
    Unreadable(String),
}

pub(super) struct Lane<'a> {
    pub(super) deps: &'a Deps,
    pub(super) router: &'a StageRouter,
    pub(super) env: &'a AdoptEnv,
    pub(super) task_id: String,
    pub(super) pin: PrAdoptedPayload,
    /// `pin.head_sha` in git's own spelling: the one head this lane may merge.
    pub(super) pinned: String,
    pub(super) repo_root: PathBuf,
    /// Where the turn is, for the events it writes.
    pub(super) phase: Cell<&'static str>,
}

impl<'a> Lane<'a> {
    /// Reads the pin of `task_id` from its latest `PrAdopted` event. Missing or
    /// malformed is [`PinError::Invalid`]: the lane does not guess a head. A
    /// store that cannot be read is [`PinError::Unreadable`]: nothing is known,
    /// so nothing is cancelled.
    pub(super) fn new(
        deps: &'a Deps,
        router: &'a StageRouter,
        env: &'a AdoptEnv,
        task_id: &str,
    ) -> Result<Self, PinError> {
        let events = deps
            .store
            .events_for_task_by_kinds(task_id, &[EventKind::PrAdopted], 1)
            .map_err(|e| PinError::Unreadable(format!("{task_id}: pr_adopted not read: {e}")))?;
        let invalid = |why: String| PinError::Invalid(why);
        let event = events.first().ok_or_else(|| {
            invalid("the task has no pr_adopted event, so no head is pinned".to_string())
        })?;
        let pin: PrAdoptedPayload = serde_json::from_str(&event.payload_json)
            .map_err(|e| invalid(format!("the pr_adopted payload is unreadable: {e}")))?;
        if pin.pr_number == 0 || !pin.head_is_valid() {
            return Err(invalid(format!(
                "the pr_adopted payload has no usable PR number or head ({:?})",
                pin.head_sha
            )));
        }
        Ok(Self {
            deps,
            router,
            env,
            task_id: task_id.to_string(),
            pinned: pin.pinned(),
            pin,
            repo_root: PathBuf::from(&deps.settings.repo_root),
            phase: Cell::new("verify"),
        })
    }

    /// The request of a dry run or the real merge: the template, this PR at the
    /// pinned head, and the adopt flags.
    pub(super) fn request(&self, dry_run: bool) -> PrMergeRequest {
        let mut req = self.env.request.clone();
        req.repo_root = self.repo_root.clone();
        req.data_dir = self.deps.settings.data_dir.clone();
        req.pr_number = self.pin.pr_number;
        req.expect_head = self.pinned.clone();
        req.dry_run = dry_run;
        // The lane's backoff is the wait; the call never sleeps for CI.
        req.ci_wait_secs = 0;
        req.no_receipt_reuse = true;
        req.run_commands_despite_head_lock = dry_run;
        req.strict_merged_head = true;
        req.keep_non_aaf_local = true;
        req
    }

    pub(super) fn begin_phase(&self, phase: &'static str) -> PortResult<()> {
        self.phase.set(phase);
        let attempt = self.retries_so_far()?;
        self.deps.emit(
            EventKind::AdoptStep,
            &self.task_id,
            json!({ "phase": phase, "attempt": attempt, "retry": false }),
        )
    }

    /// While the base branch is red only its fix task may land: wait.
    pub(super) fn base_red(&self) -> Flow<()> {
        if self.deps.settings.repo.full_gate.cooldown_hours == 0 {
            return Ok(());
        }
        if let Some(fix) = full_gate::fix_task(self.deps)? {
            return Err(Exit::retry(
                "base_red",
                format!("the base branch is red; its fix task {fix} has to land first"),
            ));
        }
        Ok(())
    }

    /// The whole turn. `Ok` means the PR is merged at the pinned head.
    pub(super) fn steps(&self) -> Flow<Done> {
        if self.deps.store.get_task(&self.task_id)?.state != TaskState::Adopting {
            return Err(Exit::Stop);
        }
        self.begin_phase("verify")?;
        let wt = self.check_pin()?;
        self.base_red()?;
        let Some(pr) = self.verify_pr()? else {
            return Ok(Done);
        };
        let tip = fetch_base(&self.repo_root, &self.env.request.remote, &pr.base_ref_name)?;
        let mut view = self.lock_view(&tip, &wt)?;
        let Some(dry) = self.dry_run()? else {
            return Ok(Done);
        };
        if dry.base_sha != view.tip {
            // The base moved between the fetch and the dry run: judge the lock
            // on the tip the dry run used, never across two tips.
            view = self.lock_view(&dry.base_sha, &wt)?;
        }
        let sign_fp = self.reconcile(&view, &dry)?;

        self.begin_phase("review")?;
        let wt = self.check_pin()?;
        let reviewers = self.review(&view, &dry, &wt)?;
        if let Some(fp) = sign_fp {
            self.sign(&view, &reviewers, &fp, &wt)?;
        }

        self.begin_phase("merge")?;
        self.check_pin()?;
        self.base_red()?;
        self.merge()
    }

    /// Applies how the turn ended. Returns a note for the run summary when
    /// something could not be recorded or posted.
    pub(super) fn finish(&self, result: Flow<Done>) -> Option<String> {
        match result {
            Ok(Done) => self.complete(),
            Err(Exit::Abort { code, reason }) => self.cancel(code, &reason),
            Err(Exit::Retry { code, reason }) => self.schedule_retry(&code, &reason),
            Err(Exit::Stop) => None,
        }
    }

    /// `Adopting -> Done`, the merge result for `aaf gc`, and the cleanup of
    /// `aaf/<id>` and its worktree. The chat's own branch and worktree stay.
    fn complete(&self) -> Option<String> {
        let now = self.deps.now();
        match self.deps.store.compare_and_set_state(
            &self.task_id,
            TaskState::Adopting,
            TaskState::Done,
            None,
            now,
        ) {
            Ok(true) => {}
            Ok(false) => return None,
            Err(err) => return Some(format!("{}: adopt not completed: {err}", self.task_id)),
        }
        let mut notes = Vec::new();
        let recorded = self
            .deps
            .emit(
                EventKind::TaskStateChanged,
                &self.task_id,
                json!({ "to": TaskState::Done.as_str(), "why": "adopted PR merged" }),
            )
            .and_then(|()| {
                self.deps.emit(
                    EventKind::MergeResult,
                    &self.task_id,
                    json!({
                        "merged": true, "head_sha": self.pinned, "kind": "adopt",
                        "pr_number": self.pin.pr_number, "url": self.pin.url,
                    }),
                )
            });
        if let Err(err) = recorded {
            notes.push(format!("merge result not recorded: {err}"));
        }
        if let Err(err) = self
            .deps
            .git
            .cleanup_verified_merged(&self.task_id, &self.pinned)
        {
            notes.push(format!(
                "cleanup of aaf/{} left work behind: {err}",
                self.task_id
            ));
        }
        (!notes.is_empty()).then(|| format!("{}: {}", self.task_id, notes.join("; ")))
    }
}
