//! [`GitOps`] on the real `crate::git` layer.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use aaf_domain::gate::Receipt;
use aaf_domain::ports::{PortError, PortResult};
use aaf_domain::proof::ProofRecord;

use super::commit_lock::{self, CommitLock};
use super::ports::{GitOps, Integration};
use crate::gate::base::{AUTO_FALLBACKS, origin_head_branch};
use crate::git::cmd::{git, git_raw};
use crate::git::worktree::{self, BRANCH_PREFIX};
use crate::git::{
    GitError, MergeOutcome, MergeRequest, PrOutcome, PrRequest, TaskWorktree, has_remote,
};
use crate::process::{CommandOutput, CommandRunner, SystemRunner};

const DEFAULT_REMOTE: &str = "origin";
/// A merge waits this long for another `aaf` process merging the same repo.
const MERGE_LOCK_TIMEOUT: Duration = Duration::from_secs(600);
const GH_TIMEOUT: Duration = Duration::from_secs(120);
const GH_LIST_TIMEOUT: Duration = Duration::from_secs(15);
const SQUASH_POLL_INTERVAL: Duration = Duration::from_secs(30);
/// A commit waits this long for another `aaf` process committing in the same
/// repository; each holder is bounded by the timeout of its own git call.
const COMMIT_LOCK_TIMEOUT: Duration = Duration::from_secs(600);

/// The branch tasks start from and land on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseRef {
    /// `Some` when the base is a remote-tracking branch.
    pub remote: Option<String>,
    /// Short branch name, e.g. `main`.
    pub branch: String,
}

impl BaseRef {
    /// What a new task branch is cut from: `origin/main` or `main`.
    pub fn start_point(&self) -> String {
        match &self.remote {
            Some(remote) => format!("{remote}/{}", self.branch),
            None => self.branch.clone(),
        }
    }
}

fn err(error: GitError) -> PortError {
    PortError::Io(error.to_string())
}

fn is_commit(repo: &Path, name: &str) -> PortResult<bool> {
    let spec = format!("{name}^{{commit}}");
    Ok(git_raw(repo, &["rev-parse", "--verify", "--quiet", &spec])
        .map_err(err)?
        .success())
}

fn split(repo: &Path, name: &str) -> PortResult<BaseRef> {
    if let Some((remote, branch)) = name.split_once('/')
        && !remote.starts_with('-')
        && has_remote(repo, remote).map_err(err)?
    {
        return Ok(BaseRef {
            remote: Some(remote.to_string()),
            branch: branch.to_string(),
        });
    }
    Ok(BaseRef {
        remote: None,
        branch: name.to_string(),
    })
}

/// Resolves `gate.base_ref` to the base branch. `auto` follows the same
/// order as the gate: `origin/HEAD`, `origin/main`, `origin/master`, `main`,
/// `master`; a repository with none of them uses its current branch.
pub fn resolve_base(repo: &Path, base_ref: &str) -> PortResult<BaseRef> {
    let wanted = base_ref.trim();
    if !wanted.is_empty() && wanted != "auto" {
        if wanted.starts_with('-') || !is_commit(repo, wanted)? {
            return Err(PortError::Invalid(format!(
                "gate.base_ref {wanted:?} does not resolve to a commit"
            )));
        }
        return split(repo, wanted);
    }
    let head = git_raw(
        repo,
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
    )
    .map_err(err)?;
    let origin_head = head.stdout.trim().to_string();
    let mut candidates = Vec::new();
    // As in the gate: `origin/HEAD` only as a symref to a non-task branch
    // of `origin`, and full ref names a tag or branch cannot shadow.
    if head.success() && origin_head_branch(&origin_head).is_some() {
        candidates.push(origin_head);
    }
    candidates.extend(AUTO_FALLBACKS.map(String::from));
    for name in candidates {
        if is_commit(repo, &name)? {
            let remote = name.strip_prefix("refs/remotes/origin/");
            let branch = remote.or(name.strip_prefix("refs/heads/"));
            return Ok(BaseRef {
                remote: remote.map(|_| DEFAULT_REMOTE.to_string()),
                branch: branch.unwrap_or(&name).to_string(),
            });
        }
    }
    let current = git(repo, &["symbolic-ref", "--short", "HEAD"]).map_err(err)?;
    Ok(BaseRef {
        remote: None,
        branch: current,
    })
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
            last_squash_poll: Mutex::new(None),
        }
    }

    #[cfg(test)]
    pub(super) fn with_commit_lock_timeout(mut self, timeout: Duration) -> RealGit {
        self.commit_lock_timeout = timeout;
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

/// Aborts the rebase in progress in `tree` and returns the files it stopped
/// on. Empty when it stopped for another reason.
fn abort_rebase(tree: &Path) -> Result<Vec<String>, GitError> {
    let unmerged = git_raw(tree, &["diff", "--name-only", "--diff-filter=U", "-z"])?;
    let files = unmerged
        .stdout
        .split('\0')
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect();
    git(tree, &["rebase", "--abort"])?;
    Ok(files)
}

/// Puts a note above the body of `gh pr create`. The body is written inside
/// [`crate::git::open_pr`], so it is amended on its way to `gh`.
struct NotedRunner<'a> {
    inner: &'a dyn CommandRunner,
    note: &'a str,
}

impl CommandRunner for NotedRunner<'_> {
    fn run(&self, program: &str, args: &[&str], cwd: &Path) -> io::Result<CommandOutput> {
        if program != "gh" {
            return self.inner.run(program, args, cwd);
        }
        if args.starts_with(&["pr", "create"]) {
            // By position, never by searching: the title is untrusted text and
            // may itself read `--body`.
            let ["pr", "create", "--title", _, "--body", body, ..] = args else {
                // A pull request without the note would present a batch receipt
                // as the branch's own; none is opened instead.
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "gh pr create: no body to put the batch note in",
                ));
            };
            let noted = format!("{}\n\n{body}", self.note);
            let mut args = args.to_vec();
            args[5] = &noted;
            return self.inner.run(program, &args, cwd);
        }
        if args.starts_with(&["pr", "edit"]) {
            let ["pr", "edit", _, "--body", body, ..] = args else {
                return self.inner.run(program, args, cwd);
            };
            let noted = format!("{}\n\n{body}", self.note);
            let mut args = args.to_vec();
            args[4] = &noted;
            return self.inner.run(program, &args, cwd);
        }
        self.inner.run(program, args, cwd)
    }
}

/// [`crate::git::open_pr`] with `note` above the receipt summary of the body.
#[cfg(test)]
pub(super) fn open_pr_noted(
    request: &PrRequest,
    runner: &dyn CommandRunner,
    note: Option<&str>,
) -> Result<PrOutcome, GitError> {
    open_pr_noted_with_proof(request, None, runner, note)
}

fn open_pr_noted_with_proof(
    request: &PrRequest,
    proof: Option<&ProofRecord>,
    runner: &dyn CommandRunner,
    note: Option<&str>,
) -> Result<PrOutcome, GitError> {
    match note {
        Some(note) => crate::git::open_pr_with_proof(
            request,
            proof,
            &NotedRunner {
                inner: runner,
                note,
            },
        ),
        None => crate::git::open_pr_with_proof(request, proof, runner),
    }
}

impl GitOps for RealGit {
    fn task_worktree(&self, task_id: &str) -> PortResult<TaskWorktree> {
        let _admin = self.admin();
        if let Some(existing) = self.existing(task_id)? {
            return Ok(existing);
        }
        worktree::create(&self.repo_root, &self.worktrees_dir, task_id, &self.start()).map_err(err)
    }

    fn cleanup(&self, task_id: &str) -> PortResult<()> {
        let _admin = self.admin();
        match self.existing(task_id)? {
            // Not forced: the worktree layer refuses a dirty tree and a
            // branch no other ref contains.
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
        // One `git commit` per repository at a time so pre-commit hooks serialize.
        let _queue = self.commit.lock().unwrap_or_else(PoisonError::into_inner);
        let lock = commit_lock::lock_path(&self.locks_dir, &self.repo_root);
        let _lock = CommitLock::acquire(&lock, self.commit_lock_timeout)?;
        git(tree, &["commit", "--quiet", "-m", message]).map_err(err)?;
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
            let merge = git_raw(
                &integration.tree.path,
                &["merge", "--no-ff", "--no-edit", "-m", &message, &branch],
            )
            .map_err(err)?;
            if merge.success() {
                integration.merged.push(task_id.clone());
                continue;
            }
            // Fails when the merge never started (nothing to abort); the
            // status check below is what guards the tree.
            let _ = git_raw(&integration.tree.path, &["merge", "--abort"]);
            let dirty = git(&integration.tree.path, &["status", "--porcelain"]).map_err(err)?;
            if !dirty.is_empty() {
                let cleanup = self.discard(&integration);
                return Err(PortError::Io(format!(
                    "integration tree is dirty after a failed merge of {branch} \
                     (cleanup: {cleanup:?})"
                )));
            }
            integration.conflicted.push(task_id.clone());
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
        let args = ["rebase", start.as_str()];
        let out = git_raw(tree, &args).map_err(err)?;
        if out.success() {
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
            remote: self.remote().to_string(),
            locks_dir: self.locks_dir.clone(),
            lock_timeout: MERGE_LOCK_TIMEOUT,
        };
        // Acquire cross-process merge lock before `admin` so waiting on
        // another process never stalls sibling worker threads.
        let lock = crate::git::merge_queue::acquire_lock(&request).map_err(err)?;
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
        // `open_pr_with_proof` pushes the validated `aaf/*` task branch, falling
        // back to `--force-with-lease` only after a non-fast-forward rework.
        let runner = SystemRunner {
            timeout: GH_TIMEOUT,
        };
        let proof = self.locks_dir.parent().and_then(|data_dir| {
            crate::proof::show_proof(&worktree.path, data_dir, Some(&receipt.tree_fp)).ok()
        });
        open_pr_noted_with_proof(&request, proof.as_ref(), &runner, note).map_err(err)
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
        // Exit 1 is "not an ancestor"; a missing branch (128) is not merged
        // as far as this repository can tell.
        Ok(git_raw(&self.repo_root, &args).map_err(err)?.code == Some(0))
    }

    fn existing_worktree_head(
        &self,
        task_id: &str,
    ) -> Result<Option<(TaskWorktree, String)>, GitError> {
        let _admin = self.admin();
        worktree::existing_head(&self.repo_root, &self.worktrees_dir, task_id)
    }

    fn sweep_orphan_batches(&self, active_task_ids: &HashSet<String>) -> Result<usize, GitError> {
        let _admin = self.admin();
        let repo_id = aaf_fp::repo_id(&self.repo_root);
        worktree::remove_orphan_batch_worktrees(
            &self.repo_root,
            &self.worktrees_dir,
            &repo_id,
            active_task_ids,
            false,
        )
        .map(|removed| removed.len())
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
