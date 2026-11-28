//! Starting and finishing the adopt lane's job. It is its own kind of message
//! and thread: it never becomes a `worker::Outcome`, so a failure or a panic
//! of an adopted PR cannot reach `block_infra`, `block_untried` or
//! `record_root_result`. Only `Mode::Auto` starts it, only when the router
//! carries an adopt environment, and at most one turn runs per repository.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::thread::Scope;

use aaf_domain::ports::PortResult;
use aaf_domain::task::TaskState;

use super::{Msg, Scheduler};
use crate::factory::Mode;
use crate::factory::adopt_lane::{self, AdoptDone};

/// After a turn that could not even record its outcome, wait before the next.
const ERROR_COOLDOWN_SECS: i64 = 60;

impl<'a> Scheduler<'a> {
    /// Starts one lane turn for the first `Adopting` task that is due
    /// (`retry_after` reached or never set), if none is running.
    pub(super) fn start_adopt_job<'scope>(
        &mut self,
        scope: &'scope Scope<'scope, '_>,
        reap: &'scope (dyn Fn(&str) -> PortResult<()> + Sync),
    ) -> PortResult<()>
    where
        'a: 'scope,
    {
        if self.mode != Mode::Auto || self.state.adopt_active {
            return Ok(());
        }
        let Some(router) = self.router.filter(|router| router.adopt.is_some()) else {
            return Ok(());
        };
        let now = self.deps.now();
        let repo = Some(self.deps.settings.repo_root.as_str());
        let due = self
            .deps
            .store
            .list_tasks(repo, Some(TaskState::Adopting))?
            .into_iter()
            .find(|task| {
                task.retry_after.is_none_or(|at| at <= now)
                    && !self
                        .state
                        .task_wait_until
                        .get(&task.id)
                        .is_some_and(|&until| until > now)
                    && !self.state.in_flight_tasks.contains_key(&task.id)
            });
        let Some(task) = due else {
            return Ok(());
        };
        self.state.adopt_active = true;
        self.state.aux_active += 1;
        *self
            .state
            .in_flight_tasks
            .entry(task.id.clone())
            .or_insert(0) += 1;
        let (deps, tx) = (self.deps, self.msg_tx.clone());
        scope.spawn(move || {
            let id = task.id;
            let done = catch_unwind(AssertUnwindSafe(|| adopt_lane::run_job(deps, router, &id)))
                .unwrap_or_else(|_| {
                    catch_unwind(AssertUnwindSafe(|| {
                        adopt_lane::on_panic(deps, router, &id, reap)
                    }))
                    .unwrap_or_else(|_| AdoptDone {
                        task_id: id.clone(),
                        error: Some(format!("{id}: the adopt lane panicked twice")),
                    })
                });
            let _ = tx.send(Msg::Adopt(done));
        });
        Ok(())
    }

    /// Takes the finished turn off the books. A turn that left a note holds
    /// the next one off for a minute, so a broken store is not hammered.
    pub(super) fn finish_adopt_job(&mut self, done: AdoptDone) {
        self.state.adopt_active = false;
        self.state.aux_active = self.state.aux_active.saturating_sub(1);
        if let Some(count) = self.state.in_flight_tasks.get_mut(&done.task_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.state.in_flight_tasks.remove(&done.task_id);
            }
        }
        if let Some(error) = done.error {
            let until = self.deps.now() + ERROR_COOLDOWN_SECS;
            self.state.task_wait_until.insert(done.task_id, until);
            self.summary.errors.push(error);
        }
    }
}
