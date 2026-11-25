//! Garbage collection for worktrees, branches, snapshot refs, stale
//! fingerprint temp files, and expired stream/report artifacts, plus verified
//! squash-merge matching for the factory review sweep.

mod periodic;

use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use aaf_domain::ports::{PortError, TaskStore};
use aaf_domain::task::{Task, TaskState};
use serde::{Deserialize, Serialize};

use crate::db::{Db, DbError, private};
use crate::git::cmd::{git, git_raw};
use crate::git::worktree::{self, BRANCH_PREFIX};
use crate::git::{GitError, path_str};

mod archive;
mod expired;
mod oracle_scratch;
#[cfg(test)]
mod orphan_lock_tests;
mod proofs;
mod refs;
mod squash;
use expired::clean_expired_artifacts;
#[cfg(test)]
use expired::orphan_scratch_lock_removable;
use refs::RepoRefs;
pub(crate) use squash::verify_squash_merges_with_warnings;

pub use archive::{GcArchiveStats, archive_old_terminal_events};
pub use periodic::{DEFAULT_MIN_FREE_DISK_BYTES, check_free_disk_bytes, lightweight_sweep};
pub use proofs::{GcProofStats, clean_expired_proofs, collect_protected_tree_fps};

/// Default retention window for stream `.jsonl` and report `.html` artifacts.
pub const DEFAULT_OLDER_THAN_DAYS: u64 = 14;

#[derive(Debug, thiserror::Error)]
pub enum GcError {
    #[error("database: {0}")]
    Db(#[from] DbError),
    #[error("store: {0}")]
    Port(#[from] PortError),
    #[error("git: {0}")]
    Git(#[from] GitError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("low disk space at {path}: {available_bytes} bytes free (minimum {min_free_bytes})")]
    LowDisk {
        path: String,
        available_bytes: u64,
        min_free_bytes: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcOptions {
    pub repo_root: Option<PathBuf>,
    pub all: bool,
    pub dry_run: bool,
    pub include_cancelled: bool,
    pub force: bool,
    pub older_than_days: u64,
}

impl Default for GcOptions {
    fn default() -> Self {
        Self {
            repo_root: None,
            all: false,
            dry_run: false,
            include_cancelled: false,
            force: false,
            older_than_days: DEFAULT_OLDER_THAN_DAYS,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcReport {
    pub dry_run: bool,
    pub repos_scanned: Vec<String>,
    pub skipped_busy: Vec<String>,
    pub skipped_dirty: Vec<String>,
    pub removed_orphan_batches: Vec<String>,
    pub removed_worktrees: Vec<String>,
    pub removed_branches: Vec<String>,
    pub removed_snapshot_refs: usize,
    pub removed_stale_fp: usize,
    pub removed_files: Vec<String>,
    pub pruned_repos: usize,
    pub warnings: Vec<String>,
    #[serde(default)]
    pub proofs_removed: usize,
    #[serde(default)]
    pub events_archived: usize,
}

struct RepoLockGuard {
    file: File,
}

impl Drop for RepoLockGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn canonical_or_owned(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn try_lock_repo(
    data_dir: &Path,
    repo_id: &str,
    dry_run: bool,
) -> Result<Option<RepoLockGuard>, GcError> {
    let locks_dir = data_dir.join("locks");
    let lock_path = locks_dir.join(format!("{repo_id}.run.lock"));
    let (file, lock_res) = if dry_run {
        if !lock_path.exists() {
            return Ok(None);
        }
        let file = File::open(&lock_path)?;
        let res = file.try_lock_shared();
        (file, res)
    } else {
        private::create_dir(&locks_dir)?;
        let file = private::open_file(&lock_path)?;
        let res = file.try_lock();
        (file, res)
    };
    match lock_res {
        Ok(()) => Ok(Some(RepoLockGuard { file })),
        Err(TryLockError::WouldBlock) => Err(GcError::Io(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "repository lock is held",
        ))),
        Err(TryLockError::Error(err)) => Err(GcError::Io(err)),
    }
}

type DbState = (Vec<Task>, HashSet<String>, HashSet<String>);

fn collect_db_state(data_dir: &Path) -> Result<DbState, GcError> {
    let db_path = data_dir.join("aaf.db");
    if !db_path.is_file() {
        return Ok((Vec::new(), HashSet::new(), HashSet::new()));
    }
    let db = Db::open_read_only(&db_path)?;
    let tasks = db.list_tasks(None, None)?;
    let running_runs = db.running_run_ids()?;
    let active_tree_fps = db.active_task_tree_fps()?;
    Ok((tasks, running_runs, active_tree_fps))
}

fn resolve_target_repos(opts: &GcOptions, tasks: &[Task]) -> Vec<PathBuf> {
    let mut set = BTreeSet::new();
    if let Some(repo) = &opts.repo_root {
        set.insert(canonical_or_owned(repo));
    }
    if opts.all {
        for task in tasks {
            if !task.repo_root.trim().is_empty() {
                set.insert(canonical_or_owned(Path::new(&task.repo_root)));
            }
        }
    }
    set.into_iter().collect()
}

/// Runs garbage collection across the selected repository (or all repositories),
/// expired stream/report/audit/proof artifacts in `data_dir`, and archives old
/// terminal-task `events` into `data_dir/archives/`.
///
/// Never modifies `receipt.key`, `receipts/`, or `approvals/`.
pub fn run_gc(
    data_dir: &Path,
    worktrees_dir: &Path,
    opts: &GcOptions,
) -> Result<GcReport, GcError> {
    let mut report = GcReport {
        dry_run: opts.dry_run,
        ..GcReport::default()
    };
    let (all_tasks, running_runs, active_db_tree_fps) = collect_db_state(data_dir)?;
    let repos = resolve_target_repos(opts, &all_tasks);

    for repo in repos {
        let repo_label = repo.display().to_string();
        if !repo.is_dir() || aaf_fp::repo_root(&repo).is_err() {
            report
                .warnings
                .push(format!("{repo_label}: skipped (not a git repository)"));
            continue;
        }
        let repo_id = aaf_fp::repo_id(&repo);
        let _lock = match try_lock_repo(data_dir, &repo_id, opts.dry_run) {
            Ok(guard) => guard,
            Err(GcError::Io(err)) if err.kind() == std::io::ErrorKind::WouldBlock => {
                report.skipped_busy.push(repo_label);
                continue;
            }
            Err(err) => return Err(err),
        };
        report.repos_scanned.push(repo_label);
        clean_repo(
            &repo,
            &repo_id,
            worktrees_dir,
            &all_tasks,
            opts,
            &mut report,
        )?;
    }

    let now = SystemTime::now();
    clean_expired_artifacts(data_dir, opts, &running_runs, now, &mut report);
    oracle_scratch::clean_oracle_scratch(data_dir, opts, now, &mut report);
    let busy_repos: HashSet<String> = report.skipped_busy.iter().cloned().collect();
    let protected_audits = crate::gc_audits::collect_protected_audit_ids_with_locks(
        &all_tasks,
        &busy_repos,
        data_dir,
        worktrees_dir,
    );
    let cutoff = now
        .checked_sub(Duration::from_secs(
            opts.older_than_days.saturating_mul(86_400),
        ))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let audit_stats = crate::gc_audits::clean_expired_audits(
        data_dir,
        cutoff,
        opts.older_than_days,
        &protected_audits,
        opts.dry_run,
    );
    report.removed_files.extend(audit_stats.removed_files);
    report.warnings.extend(audit_stats.warnings);

    let mut protected_tree_fps = proofs::collect_protected_tree_fps(data_dir, None);
    protected_tree_fps.extend(active_db_tree_fps);
    let active_task_ids: HashSet<String> = all_tasks
        .iter()
        .filter(|t| !t.state.is_terminal())
        .map(|t| t.id.clone())
        .collect();
    let proof_stats = proofs::clean_expired_proofs(
        data_dir,
        now,
        opts.older_than_days,
        &protected_tree_fps,
        &active_task_ids,
        opts.dry_run,
    );
    report.removed_files.extend(proof_stats.removed_files);
    report.proofs_removed += proof_stats.proofs_removed;
    report.warnings.extend(proof_stats.warnings);

    let now_epoch = i64::try_from(
        now.duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    )
    .unwrap_or(i64::MAX);
    let archive_stats = archive::archive_old_terminal_events(
        data_dir,
        now_epoch,
        opts.older_than_days,
        opts.dry_run,
    )?;
    report.events_archived += archive_stats.events_archived;
    Ok(report)
}

fn clean_repo(
    repo: &Path,
    repo_id: &str,
    worktrees_dir: &Path,
    all_tasks: &[Task],
    opts: &GcOptions,
    report: &mut GcReport,
) -> Result<(), GcError> {
    let canonical_repo = canonical_or_owned(repo);
    let repo_tasks: Vec<&Task> = all_tasks
        .iter()
        .filter(|t| canonical_or_owned(Path::new(&t.repo_root)) == canonical_repo)
        .collect();
    let active_ids: HashSet<String> = repo_tasks.iter().map(|t| t.id.clone()).collect();

    // 1. Orphaned `aaf/batch-*` worktrees and branches.
    match worktree::remove_orphan_batch_worktrees(
        repo,
        worktrees_dir,
        repo_id,
        &active_ids,
        opts.dry_run,
    ) {
        Ok(sweep) => {
            report.removed_orphan_batches.extend(sweep.removed);
            report.warnings.extend(sweep.warnings);
        }
        Err(err) => report
            .warnings
            .push(format!("{}: orphan batch sweep: {err}", repo.display())),
    }

    // 2. Done tasks (and Cancelled tasks when `include_cancelled`).
    let registered = worktree::list(repo)?;
    let refs = RepoRefs::list(repo)?;
    for task in repo_tasks {
        let eligible = match task.state {
            TaskState::Done => true,
            TaskState::Cancelled => opts.include_cancelled,
            _ => false,
        };
        if !eligible {
            continue;
        }
        if worktree::validate_task_id(&task.id).is_err() {
            report
                .warnings
                .push(format!("skipped invalid task id {:?}", task.id));
            continue;
        }
        clean_task_artifacts(repo, worktrees_dir, task, &registered, &refs, opts, report)?;
    }

    // 3. `worktree::prune` + `aaf_fp::sweep_stale`.
    if !opts.dry_run {
        match worktree::prune(repo) {
            Ok(()) => report.pruned_repos += 1,
            Err(err) => report
                .warnings
                .push(format!("{}: worktree prune: {err}", repo.display())),
        }
        match aaf_fp::sweep_stale(repo) {
            Ok(n) => report.removed_stale_fp += n,
            Err(err) => report
                .warnings
                .push(format!("{}: sweep_stale: {err}", repo.display())),
        }
    }
    Ok(())
}

fn clean_task_artifacts(
    repo: &Path,
    worktrees_dir: &Path,
    task: &Task,
    registered: &[worktree::WorktreeEntry],
    refs: &RepoRefs,
    opts: &GcOptions,
    report: &mut GcReport,
) -> Result<(), GcError> {
    let branch = format!("{BRANCH_PREFIX}{}", task.id);
    let expected_path = worktree::worktree_path(repo, worktrees_dir, &task.id)?;
    let reg_entry = registered
        .iter()
        .find(|e| e.branch.as_deref() == Some(branch.as_str()));
    // Only the task's own aaf worktree is removed: the branch checked out in
    // the main worktree or a user's linked worktree is kept, and so is it.
    if let Some(warning) =
        reg_entry.and_then(|e| worktree::foreign_checkout(&task.id, e, registered, &expected_path))
    {
        report.warnings.push(warning);
        return Ok(());
    }
    let existing_wt_path = reg_entry
        .filter(|e| e.path.exists())
        .map(|e| e.path.clone())
        .or_else(|| expected_path.exists().then_some(expected_path.clone()));

    if let Some(wt_path) = &existing_wt_path {
        let dirty = git_raw(wt_path, &["status", "--porcelain"])
            .map(|o| !o.success() || !o.stdout.trim().is_empty())
            .unwrap_or(true);
        if dirty && !opts.force {
            report.skipped_dirty.push(task.id.clone());
            return Ok(());
        }
        // A probe git cannot answer (e.g. a broken `.git` link) means the
        // real run's removal fails too: a per-task warning, not an error.
        if opts.dry_run
            && reg_entry.is_some()
            && let Some(reason) = removal_refusal(wt_path, opts.force)
                .map_or_else(|err| Some(err.to_string()), |r| r.map(str::to_string))
        {
            report.warnings.push(format!(
                "{}: worktree remove: would be refused: {reason}",
                task.id
            ));
            return Ok(());
        }
        if !opts.dry_run {
            if reg_entry.is_some() {
                let p = path_str(wt_path)?;
                // Git refuses some worktrees: without --force initialised
                // submodules or a lock, with one --force still a lock (never
                // `-f -f`). Keep this task's worktree, branch and snapshot
                // refs; the rest of the run goes on.
                let out = if opts.force {
                    git_raw(repo, &["worktree", "remove", "--force", p])?
                } else {
                    git_raw(repo, &["worktree", "remove", p])?
                };
                if !out.success() {
                    report.warnings.push(format!(
                        "{}: worktree remove: {}",
                        task.id,
                        out.stderr.trim()
                    ));
                    return Ok(());
                }
            } else if let Ok(meta) = fs::symlink_metadata(wt_path)
                && !meta.file_type().is_symlink()
                && meta.is_dir()
            {
                fs::remove_dir_all(wt_path)?;
            }
        }
        report.removed_worktrees.push(wt_path.display().to_string());
    } else if reg_entry.is_some_and(|e| e.locked) {
        // A locked tree whose directory is gone (a volume not mounted):
        // prune keeps its record, so git keeps its branch too.
        report.warnings.push(format!(
            "{}: worktree is locked and its directory is missing: kept",
            task.id
        ));
        return Ok(());
    } else if reg_entry.is_some() && !opts.dry_run {
        worktree::prune(repo)?;
    }

    let branch_ref = format!("refs/heads/{branch}");
    if refs.has_branch(&branch_ref) {
        if !opts.dry_run {
            let del = git_raw(repo, &["branch", "-D", &branch])?;
            if !del.success() && !del.stderr.to_ascii_lowercase().contains("not found") {
                // A branch git keeps (e.g. still checked out somewhere) is
                // this task's warning; the rest of the run goes on.
                report
                    .warnings
                    .push(format!("{}: branch delete: {}", task.id, del.stderr.trim()));
                return Ok(());
            }
        }
        report.removed_branches.push(branch);
    }

    // Only a task git listed snapshot refs for is asked to delete them.
    let snapshots = refs.snapshot_count(&task.id);
    if opts.dry_run {
        report.removed_snapshot_refs += snapshots;
    } else if snapshots > 0 {
        match worktree::delete_snapshot_refs(repo, &task.id) {
            Ok(deleted) => report.removed_snapshot_refs += deleted,
            // e.g. a stale ref lock: this task's warning, the run goes on.
            Err(err) => report
                .warnings
                .push(format!("{}: snapshot refs: {err}", task.id)),
        }
    }
    Ok(())
}

/// Why the real run's `git worktree remove` of `wt` would be refused. With
/// `--force` git still refuses a locked worktree (gc never passes `-f -f`:
/// a lock is explicit intent to keep it); initialised submodules are fine.
fn removal_refusal(wt: &Path, force: bool) -> Result<Option<&'static str>, GcError> {
    if !force {
        return Ok(worktree::removal_refusal(wt)?);
    }
    let git_dir = git(wt, &["rev-parse", "--absolute-git-dir"])?;
    Ok(Path::new(git_dir.trim())
        .join("locked")
        .exists()
        .then_some("cannot remove a locked working tree"))
}
