//! The pin (I2): before verify, review and merge the adopt worktree must still
//! be at the pinned head, on `aaf/<id>`, with `refs/heads/aaf/<id>` at the same
//! head and nothing uncommitted. Any difference cancels with `head_changed`.

use aaf_domain::task::TaskState;

use super::flow::{Exit, Flow};
use super::lane::Lane;
use crate::git::cmd::{git, git_raw};
use crate::git::{GitError, TaskWorktree};

fn head_changed(why: impl Into<String>) -> Exit {
    Exit::abort("head_changed", why)
}

impl Lane<'_> {
    fn branch(&self) -> String {
        format!("aaf/{}", self.task_id)
    }

    /// The value of `refs/heads/aaf/<id>`, `None` when it does not exist.
    fn branch_head(&self) -> Result<Option<String>, GitError> {
        let spec = format!("refs/heads/{}", self.branch());
        let out = git_raw(
            &self.repo_root,
            &["rev-parse", "--verify", "--quiet", &spec],
        )?;
        let head = out.stdout.trim().to_ascii_lowercase();
        Ok((out.success() && !head.is_empty()).then_some(head))
    }

    /// Checks the pin and returns the adopt worktree. Never creates one: a
    /// missing worktree is a change, not something to rebuild from the base.
    pub(super) fn check_pin(&self) -> Flow<TaskWorktree> {
        let wt = match self.deps.git.existing_worktree_head(&self.task_id)? {
            Some((wt, _)) if wt.path.is_dir() => wt,
            _ => return Err(head_changed("the adopt worktree no longer exists")),
        };
        let head = git(&wt.path, &["rev-parse", "HEAD"])?.to_ascii_lowercase();
        if head != self.pinned {
            return Err(head_changed(format!(
                "the adopt worktree is at {head}, not the adopted {}",
                self.pinned
            )));
        }
        let on = git_raw(&wt.path, &["symbolic-ref", "-q", "HEAD"])?;
        if on.stdout.trim() != format!("refs/heads/{}", self.branch()) {
            return Err(head_changed(format!(
                "the adopt worktree is not on {}",
                self.branch()
            )));
        }
        match self.branch_head()? {
            Some(branch) if branch == self.pinned => {}
            Some(branch) => {
                return Err(head_changed(format!(
                    "{} moved to {branch}, not the adopted {}",
                    self.branch(),
                    self.pinned
                )));
            }
            None => return Err(head_changed(format!("{} no longer exists", self.branch()))),
        }
        match crate::git::worktree::ensure_clean(&wt.path) {
            Ok(()) => {}
            Err(GitError::Dirty(_)) => {
                return Err(head_changed("the adopt worktree has uncommitted changes"));
            }
            Err(other) => return Err(other.into()),
        }
        Ok(wt)
    }

    /// What `pre_merge_check` asks right before `gh pr merge`: still adopting
    /// and the branch still at the pinned head. Only reads; it runs while the
    /// repository merge lock is held.
    pub(super) fn still_pinned(&self) -> bool {
        let adopting = self
            .deps
            .store
            .get_task(&self.task_id)
            .is_ok_and(|task| task.state == TaskState::Adopting);
        adopting && self.branch_head().ok().flatten().as_deref() == Some(self.pinned.as_str())
    }
}
