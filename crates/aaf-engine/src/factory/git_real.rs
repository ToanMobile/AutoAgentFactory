//! [`GitOps`] on the real `crate::git` layer.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use aaf_domain::gate::Receipt;
use aaf_domain::ports::{PortError, PortResult};

use super::base_ref::{BaseRef, DEFAULT_REMOTE};
use super::commit_lock::{self, CommitLock};
use super::git_real_base::{
    NO_HOOKS, abort_rebase, git_no_hooks, merge_pr_real, refuse_nested_repositories,
};
use super::ports::{GitOps, Integration};
use crate::git::cmd::{failure, git, git_raw, git_timed};
use crate::git::pr_merge::PrMergeOutcome;
use crate::git::worktree::{self, BRANCH_PREFIX};
use crate::git::{GitError, MergeOutcome, MergeRequest, PrOutcome, PrRequest, TaskWorktree};
use crate::process::{CommandRunner, SystemRunner};

#[allow(dead_code)]
mod commit;
mod pr_note;
pub(crate) use commit::COMMIT_DEFERRED;
#[cfg(test)]
pub(super) use pr_note::open_pr_noted;
use pr_note::open_pr_noted_with_proof;

/// A merge waits this long for another `aaf` process merging the same repo.
/// `aaf merge` holds this lock across a whole scratch gate (its own
/// `--lock-timeout` defaults to 1800 s), so the factory waits as long.
const MERGE_LOCK_TIMEOUT: Duration = Duration::from_secs(1800);
const GH_TIMEOUT: Duration = Duration::from_secs(120);
const GH_LIST_TIMEOUT: Duration = Duration::from_secs(15);
const SQUASH_POLL_INTERVAL: Duration = Duration::from_secs(30);
/// Budget of one `git commit`, the repository's hooks included: a pre-commit
/// hook may first wait for a build slot (`aaf slot`, a wait with no ceiling
/// of its own; only waiting for CPU load ends after `heavy_wait_secs`)
/// before it even starts its build, so this budget bounds that wait too.
pub(super) const COMMIT_TIMEOUT: Duration = Duration::from_secs(2400);
/// A commit waits this long for another `aaf` process committing in the same
/// repository; a holder runs at most two commits, each bounded by
/// [`COMMIT_TIMEOUT`].
pub(super) const COMMIT_LOCK_TIMEOUT: Duration = Duration::from_secs(2 * COMMIT_TIMEOUT.as_secs());

pub(super) fn err(error: GitError) -> PortError {
    PortError::Io(error.to_string())
}

pub struct RealGit {
    /// Main work tree of the repository.
    repo_root: PathBuf,
    worktrees_dir: PathBuf,
    /// `<data dir>/locks`.
    locks_dir: PathBuf,
    base: BaseRef,
    /// Serialises git commands that write or enumerate worktree entries.
    admin: Mutex<()>,
    /// Queues in-process commits separately from `admin` so slow hooks never
    /// stall sibling worktree creation.
    commit: Mutex<()>,
    commit_lock_timeout: Duration,
    commit_timeout: Duration,
    merge_lock_timeout: Duration,
    last_squash_poll: Mutex<Option<Instant>>,
}

impl RealGit {
    /// `locks_dir` is `<data dir>/locks`.
    pub fn new(
        repo_root: PathBuf,
        worktrees_dir: PathBuf,
        locks_dir: PathBuf,
        base: BaseRef,
    ) -> RealGit {
        RealGit {
            repo_root,
            worktrees_dir,
            locks_dir,
            base,
            admin: Mutex::new(()),
            commit: Mutex::new(()),
            commit_lock_timeout: COMMIT_LOCK_TIMEOUT,
            commit_timeout: COMMIT_TIMEOUT,
            merge_lock_timeout: MERGE_LOCK_TIMEOUT,
            last_squash_poll: Mutex::new(None),
        }
    }

    #[cfg(test)]
    pub(super) fn with_commit_lock_timeout(mut self, timeout: Duration) -> RealGit {
        self.commit_lock_timeout = timeout;
        self
    }

    #[cfg(test)]
    pub(super) fn with_merge_lock_timeout(mut self, timeout: Duration) -> RealGit {
        self.merge_lock_timeout = timeout;
        self
    }

    #[cfg(test)]
    pub(super) fn with_commit_timeout(mut self, timeout: Duration) -> RealGit {
        self.commit_timeout = timeout;
        self
    }

    pub fn base(&self) -> &BaseRef {
        &self.base
    }

    /// Runs squash-merge PR verification using `runner` (injectable in tests).
    pub fn verify_squash_merges_with(
        &self,
        candidates: &[(String, String)],
        runner: &dyn CommandRunner,
    ) -> Result<HashMap<String, String>, GitError> {
        let fetch = || {
            self.fetch_base()
                .map_err(|e| GitError::Invalid(e.to_string()))
        };
        crate::gc::verify_squash_merges(
            &self.repo_root,
            &self.base.branch,
            &self.start(),
            candidates,
            &fetch,
            runner,
        )
    }

    fn admin(&self) -> MutexGuard<'_, ()> {
        self.admin.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn start(&self) -> String {
        self.base.start_point()
    }

    fn remote(&self) -> &str {
        self.base.remote.as_deref().unwrap_or(DEFAULT_REMOTE)
    }

    /// The task's worktree when git has it registered on the task's branch.
    fn existing(&self, task_id: &str) -> PortResult<Option<TaskWorktree>> {
        let path =
            worktree::worktree_path(&self.repo_root, &self.worktrees_dir, task_id).map_err(err)?;
        let branch = format!("{BRANCH_PREFIX}{task_id}");
        // Git lists resolved paths; the configured directory may be a link.
        let Ok(resolved) = std::fs::canonicalize(&path) else {
            return Ok(None);
        };
        let registered = worktree::list(&self.repo_root)
            .map_err(err)?
            .into_iter()
            .any(|entry| {
                entry.branch.as_deref() == Some(branch.as_str())
                    && std::fs::canonicalize(&entry.path).is_ok_and(|p| p == resolved)
            });
        Ok(registered.then_some(TaskWorktree { path, branch }))
    }
}

impl GitOps for RealGit {
    fn task_worktree(&self, task_id: &str) -> PortResult<TaskWorktree> {
        let _admin = self.admin();
        if let Some(existing) = self.existing(task_id)? {
            return Ok(existing);
        }
        let wt = worktree::create(&self.repo_root, &self.worktrees_dir, task_id, &self.start())
            .map_err(err)?;
        let base_sha = self.base_sha()?;
        crate::git::hydration::hydrate_fresh_worktree(&self.repo_root, &wt, &base_sha)
            .map_err(err)?;
        Ok(wt)
    }

    fn cleanup(&self, task_id: &str) -> PortResult<()> {
        let _admin = self.admin();
        match self.existing(task_id)? {
            // Not forced: refuses a dirty tree and an unmerged branch.
            Some(tree) => worktree::remove(&self.repo_root, &tree, true, false).map_err(err),
            None => Ok(()),
        }
    }

    fn commit_all(&self, worktree: &TaskWorktree, message: &str) -> PortResult<()> {
        let tree = worktree.path.as_path();
        git(tree, &["add", "-A"]).map_err(err)?;
        if git(tree, &["status", "--porcelain"])
            .map_err(err)?
            .is_empty()
        {
            return Ok(());
        }
        refuse_nested_repositories(tree)?;
        // Serialize commits per repository; bypass untrusted commit hooks
        // (`-c core.hooksPath=/dev/null` + `--no-verify`) and interactive GPG
        // prompts (`commit.gpgsign=false`) before `head_lock` has inspected
        // the tree (Plan v3 V2-SEC-03).
        let _queue = self.commit.lock().unwrap_or_else(PoisonError::into_inner);
        let lock = commit_lock::lock_path(&self.locks_dir, &self.repo_root);
        let _lock = CommitLock::acquire(&lock, self.commit_lock_timeout).map_err(|e| match e {
            PortError::Io(why) if why.contains("still committing") => {
                PortError::Io(format!("{COMMIT_DEFERRED}: {why}"))
            }
            other => other,
        })?;
        let args = [
            "-c",
            NO_HOOKS,
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--no-verify",
            "--quiet",
            "-m",
            message,
        ];
        let out = git_timed(tree, &args, self.commit_timeout).map_err(|error| match error {
            GitError::Timeout { command } => PortError::Io(format!(
                "{COMMIT_DEFERRED}: `{command}` timed out after {} s",
                self.commit_timeout.as_secs()
            )),
            other => err(other),
        })?;
        if !out.success() {
            return Err(err(failure(&args, &out)));
        }
        Ok(())
    }

    fn has_changes(&self, worktree: &TaskWorktree) -> PortResult<bool> {
        let range = format!("{}...HEAD", self.start());
        let out = git_raw(&worktree.path, &["diff", "--quiet", &range]).map_err(err)?;
        match out.code {
            Some(0) => Ok(false),
            Some(1) => Ok(true),
            _ => Err(PortError::Io(format!(
                "git diff --quiet {range}: {}",
                out.stderr.trim()
            ))),
        }
    }

    fn diff(&self, worktree: &TaskWorktree) -> PortResult<String> {
        let range = format!("{}...HEAD", self.start());
        git(&worktree.path, &["diff", &range]).map_err(err)
    }

    fn integrate(&self, task_ids: &[String]) -> PortResult<Integration> {
        // A task-shaped name, so the worktree layer creates and removes the
        // integration tree like any other and it shows up in its listings.
        let name = format!("batch-{}", &super::unblock::unique_id()[..12]);
        let tree = {
            let _admin = self.admin();
            worktree::create(&self.repo_root, &self.worktrees_dir, &name, &self.start())
                .map_err(err)?
        };
        let mut integration = Integration {
            tree,
            merged: Vec::new(),
            conflicted: Vec::new(),
        };
        for task_id in task_ids {
            worktree::validate_task_id(task_id).map_err(err)?;
            let branch = format!("{BRANCH_PREFIX}{task_id}");
            let message = format!("aaf batch: {branch}");
            let merge = git_no_hooks(
                &integration.tree.path,
                &[
                    "-c",
                    "commit.gpgsign=false",
                    "merge",
                    "--no-ff",
                    "--no-edit",
                    "--no-verify",
                    "-m",
                    &message,
                    &branch,
                ],
            )
            .map_err(err)?;
            if !merge.success() {
                // Fails when the merge never started (nothing to abort); the
                // status check below is what guards the tree.
                let _ = git_no_hooks(&integration.tree.path, &["merge", "--abort"]);
            }
            let dirty = git(&integration.tree.path, &["status", "--porcelain"]).map_err(err)?;
            if !dirty.is_empty() {
                let cleanup = self.discard(&integration);
                return Err(PortError::Io(format!(
                    "integration tree is dirty after the merge of {branch} \
                     (cleanup: {cleanup:?})"
                )));
            }
            if merge.success() {
                integration.merged.push(task_id.clone());
            } else {
                integration.conflicted.push(task_id.clone());
            }
        }
        Ok(integration)
    }

    fn discard(&self, integration: &Integration) -> PortResult<()> {
        // Forced: the tree holds only merge commits of branches that all
        // still exist, and nothing in it is ever worked on.
        let _admin = self.admin();
        worktree::remove(&self.repo_root, &integration.tree, true, true).map_err(err)
    }

    fn rebase(&self, worktree: &TaskWorktree) -> PortResult<Vec<String>> {
        let _admin = self.admin();
        let tree = worktree.path.as_path();
        let start = self.start();
        let args = [
            "-c",
            "commit.gpgsign=false",
            "rebase",
            "--rebase-merges",
            start.as_str(),
        ];
        let out = git_no_hooks(tree, &args).map_err(err)?;
        if out.success() {
            // The gate reads the working tree; what lands is the commit.
            worktree::ensure_clean(tree).map_err(err)?;
            return Ok(Vec::new());
        }
        // Aborting a rebase that never started hides the original rebase error.
        match abort_rebase(tree) {
            Ok(files) if !files.is_empty() => Ok(files),
            _ => Err(PortError::Io(format!(
                "git rebase {start}: {}",
                out.stderr.trim()
            ))),
        }
    }

    fn base_sha(&self) -> PortResult<String> {
        git(&self.repo_root, &["rev-parse", "--verify", &self.start()]).map_err(err)
    }

    fn merge(
        &self,
        worktree: &TaskWorktree,
        regate: &dyn Fn(&Path) -> Result<bool, String>,
    ) -> PortResult<MergeOutcome> {
        self.merge_with_onto(worktree, &|tree, _| regate(tree))
    }

    fn merge_with_onto(
        &self,
        worktree: &TaskWorktree,
        regate: &dyn Fn(&Path, &str) -> Result<bool, String>,
    ) -> PortResult<MergeOutcome> {
        let request = MergeRequest {
            repo_root: self.repo_root.clone(),
            worktree: worktree.clone(),
            base: self.base.branch.clone(),
            remote: self.base.remote.clone(),
            locks_dir: self.locks_dir.clone(),
            lock_timeout: self.merge_lock_timeout,
        };
        // Acquire cross-process merge lock before `admin` so waiting on
        // another process never stalls sibling worker threads. A holder that
        // outlasts the wait (`aaf merge` gating a scratch tree) is not an
        // error: the merge is tried again like a base that moved.
        let lock = match crate::git::merge_queue::acquire_lock(&request) {
            Ok(lock) => lock,
            Err(GitError::LockTimeout(_)) => return Ok(MergeOutcome::NotFastForward),
            Err(error) => return Err(err(error)),
        };
        // Release `admin` while the gate judges the rebased tree.
        let admin = RefCell::new(Some(self.admin()));
        let regate_unlocked = |tree: &Path, onto_sha: &str| {
            drop(admin.borrow_mut().take());
            let verdict = regate(tree, onto_sha);
            *admin.borrow_mut() = Some(self.admin());
            verdict
        };
        crate::git::merge_queue::merge_with_lock_onto(&request, &lock, &regate_unlocked)
            .map_err(err)
    }

    fn open_pr(
        &self,
        worktree: &TaskWorktree,
        title: &str,
        receipt: &Receipt,
        note: Option<&str>,
    ) -> PortResult<PrOutcome> {
        let request = PrRequest {
            repo_root: self.repo_root.clone(),
            worktree: worktree.clone(),
            base: self.base.branch.clone(),
            remote: self.remote().to_string(),
            title: title.to_string(),
            receipt: receipt.clone(),
        };
        let runner = SystemRunner {
            timeout: GH_TIMEOUT,
        };
        let proof = self.locks_dir.parent().and_then(|data_dir| {
            crate::proof::show_proof(&worktree.path, data_dir, Some(&receipt.tree_fp)).ok()
        });
        open_pr_noted_with_proof(&request, proof.as_ref(), &runner, note).map_err(err)
    }

    fn merge_pr(
        &self,
        worktree: &TaskWorktree,
        pr_url: &str,
        head_sha: &str,
    ) -> PortResult<PrMergeOutcome> {
        merge_pr_real(
            &self.repo_root,
            &self.locks_dir,
            self.remote(),
            MERGE_LOCK_TIMEOUT,
            worktree,
            pr_url,
            head_sha,
        )
    }

    fn sync_local_base(&self) -> PortResult<Option<String>> {
        let Some(remote) = self.base.remote.as_deref() else {
            return Ok(None);
        };
        let note =
            crate::git::pr_merge::sync_local_base(&self.repo_root, remote, &self.base.branch);
        Ok(Some(note))
    }

    fn fetch_base(&self) -> PortResult<()> {
        let Some(remote) = &self.base.remote else {
            return Ok(());
        };
        let branch = &self.base.branch;
        let refspec = format!("+refs/heads/{branch}:refs/remotes/{remote}/{branch}");
        git(&self.repo_root, &["fetch", remote, &refspec]).map_err(err)?;
        Ok(())
    }

    fn is_merged(&self, task_id: &str) -> PortResult<bool> {
        worktree::validate_task_id(task_id).map_err(err)?;
        let branch = format!("refs/heads/{BRANCH_PREFIX}{task_id}");
        let args = ["merge-base", "--is-ancestor", &branch, &self.start()];
        Ok(git_raw(&self.repo_root, &args).map_err(err)?.code == Some(0))
    }

    fn existing_worktree_head(
        &self,
        task_id: &str,
    ) -> Result<Option<(TaskWorktree, String)>, GitError> {
        let _admin = self.admin();
        worktree::existing_head(&self.repo_root, &self.worktrees_dir, task_id)
    }

    fn sweep_orphan_batches(
        &self,
        active_task_ids: &HashSet<String>,
    ) -> Result<Vec<String>, GitError> {
        let _admin = self.admin();
        let repo_id = aaf_fp::repo_id(&self.repo_root);
        worktree::remove_orphan_batch_worktrees(
            &self.repo_root,
            &self.worktrees_dir,
            &repo_id,
            active_task_ids,
            false,
        )
        .map(|sweep| sweep.warnings)
    }

    fn prune_worktrees(&self) -> Result<(), GitError> {
        let _admin = self.admin();
        worktree::prune(&self.repo_root)
    }

    fn verified_squash_merges(
        &self,
        candidates: &[(String, String)],
    ) -> Result<HashMap<String, String>, GitError> {
        if candidates.is_empty() {
            return Ok(HashMap::new());
        }
        {
            let mut last = self
                .last_squash_poll
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if last.is_some_and(|t| t.elapsed() < SQUASH_POLL_INTERVAL) {
                return Ok(HashMap::new());
            }
            *last = Some(Instant::now());
        }
        let runner = SystemRunner {
            timeout: GH_LIST_TIMEOUT,
        };
        self.verify_squash_merges_with(candidates, &runner)
    }

    fn remove_verified_merged(
        &self,
        worktree: &TaskWorktree,
        expected_head_oid: &str,
    ) -> Result<(), GitError> {
        let _admin = self.admin();
        worktree::remove_verified_merged(&self.repo_root, worktree, expected_head_oid)
    }

    fn create_turn_snapshot(
        &self,
        worktree: &TaskWorktree,
        run_id: &str,
        now: i64,
    ) -> Result<Option<(String, String)>, GitError> {
        let _admin = self.admin();
        let task_id = worktree.task_id()?;
        super::snapshots::create_turn_snapshot_ref(
            &self.repo_root,
            &worktree.path,
            task_id,
            run_id,
            now,
        )
        .map(Some)
    }
}
