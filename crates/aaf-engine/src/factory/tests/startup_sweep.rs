//! The scheduler's startup sweep surfaces the orphan batches git refused to
//! remove (`GitOps::sweep_orphan_batches` warnings) in `RunSummary::errors`.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use aaf_domain::gate::Receipt;
use aaf_domain::ports::PortResult;

use super::fakes_git::FakeGit;
use super::world::World;
use crate::factory::ports::{GitOps, Integration};
use crate::factory::{Hooks, Mode, run};
use crate::git::{GitError, MergeOutcome, PrOutcome, TaskWorktree};

/// [`FakeGit`] whose orphan sweep reports one refused batch.
#[derive(Default)]
struct RefusingSweepGit(FakeGit);

impl GitOps for RefusingSweepGit {
    fn task_worktree(&self, task_id: &str) -> PortResult<TaskWorktree> {
        self.0.task_worktree(task_id)
    }
    fn commit_all(&self, worktree: &TaskWorktree, message: &str) -> PortResult<()> {
        self.0.commit_all(worktree, message)
    }
    fn has_changes(&self, worktree: &TaskWorktree) -> PortResult<bool> {
        self.0.has_changes(worktree)
    }
    fn diff(&self, worktree: &TaskWorktree) -> PortResult<String> {
        self.0.diff(worktree)
    }
    fn integrate(&self, task_ids: &[String]) -> PortResult<Integration> {
        self.0.integrate(task_ids)
    }
    fn discard(&self, integration: &Integration) -> PortResult<()> {
        self.0.discard(integration)
    }
    fn rebase(&self, worktree: &TaskWorktree) -> PortResult<Vec<String>> {
        self.0.rebase(worktree)
    }
    fn base_sha(&self) -> PortResult<String> {
        self.0.base_sha()
    }
    fn merge(
        &self,
        worktree: &TaskWorktree,
        regate: &dyn Fn(&Path) -> Result<bool, String>,
    ) -> PortResult<MergeOutcome> {
        self.0.merge(worktree, regate)
    }
    fn open_pr(
        &self,
        worktree: &TaskWorktree,
        title: &str,
        receipt: &Receipt,
        note: Option<&str>,
    ) -> PortResult<PrOutcome> {
        self.0.open_pr(worktree, title, receipt, note)
    }
    fn fetch_base(&self) -> PortResult<()> {
        self.0.fetch_base()
    }
    fn is_merged(&self, task_id: &str) -> PortResult<bool> {
        self.0.is_merged(task_id)
    }
    fn cleanup(&self, task_id: &str) -> PortResult<()> {
        self.0.cleanup(task_id)
    }
    fn sweep_orphan_batches(
        &self,
        _active_task_ids: &HashSet<String>,
    ) -> Result<Vec<String>, GitError> {
        Ok(vec!["batch-x: worktree remove: x".to_string()])
    }
}

#[test]
fn startup_sweep_reports_refused_orphan_batches_in_run_errors() {
    let world = World::new();
    let mut deps = world.deps();
    deps.git = Arc::new(RefusingSweepGit::default());
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };

    let summary = run(&deps, Mode::Once, &hooks).unwrap();

    assert_eq!(
        summary.errors,
        vec!["startup_sweep orphan batches: batch-x: worktree remove: x".to_string()]
    );
}
