//! Garbage collection for worktrees, branches, snapshot refs, stale
//! fingerprint temp files, merge/oracle scratch artifacts, verified-merged
//! `Review` / orphan `aaf/*` branches, and expired stream/report/audit/proof
//! artifacts, plus verified squash-merge matching for the factory review sweep.
//!
//! DB inspection is strictly read-only except when `aaf gc` (not `--dry-run`)
//! atomically transitions verified-merged `Review` tasks to `Done` before
//! cleaning their worktrees/branches or archives old terminal-task events.

mod archive;
mod expired;
mod merged_branches;
#[cfg(test)]
mod orphan_lock_tests;
mod periodic;
mod proofs;
mod refs;
mod review;
mod scratch;
mod squash;
mod task_clean;

use std::collections::{BTreeSet, HashSet};
use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use aaf_domain::ports::{PortError, TaskStore};
use aaf_domain::task::{Task, TaskState};
use serde::{Deserialize, Serialize};

use crate::db::{Db, DbError, private};
use crate::git::GitError;
use crate::git::worktree;

use expired::clean_expired_artifacts;
#[cfg(test)]
use expired::orphan_scratch_lock_removable;
use refs::RepoRefs;
pub(crate) use squash::verify_squash_merges_with_warnings;
use task_clean::{
    TaskCleanOpts, canonical_main_repo, check_cwd_blocks, clean_task_artifacts_by_id,
    record_removed_file, resolved_path,
};

pub use archive::{GcArchiveStats, archive_old_terminal_events};
pub use periodic::{
    DEFAULT_MIN_FREE_DISK_BYTES, OfflineSweepReport, check_free_disk_bytes,
    clean_repo_offline_unlocked, lightweight_sweep,
};
pub use proofs::{GcProofStats, clean_expired_proofs, collect_protected_tree_fps};

/// Default retention window for stream `.jsonl` and report `.html` artifacts.
pub const DEFAULT_OLDER_THAN_DAYS: u64 = 14;
/// Prune grace window preventing races with concurrent `git worktree add`.
pub(super) const PRUNE_EXPIRE: &str = "5.minutes.ago";

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
    pub cwd: Option<PathBuf>,
    pub merged_branches: bool,
    pub base: Option<String>,
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
            cwd: None,
            merged_branches: false,
            base: None,
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
    #[serde(default)]
    pub merged_branches_base: Option<String>,
    #[serde(default)]
    pub merged_branches_deleted: Vec<String>,
    #[serde(default)]
    pub merged_worktrees_removed: Vec<PathBuf>,
    #[serde(default)]
    pub completed_review_tasks: Vec<String>,
    #[serde(default)]
    pub removed_merge_refs: usize,
}

struct RepoLockGuard {
    file: File,
}

impl Drop for RepoLockGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
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
        set.insert(canonical_main_repo(repo));
    }
    if opts.all {
        for task in tasks {
            if !task.repo_root.trim().is_empty() {
                set.insert(canonical_main_repo(Path::new(&task.repo_root)));
            }
        }
    }
    set.into_iter().collect()
}

/// Runs garbage collection across the selected repository (or all repositories),
/// verified-merged `Review` / orphan `aaf/*` branches, merge/oracle scratch
/// leftovers, expired stream/report/audit/proof artifacts in `data_dir`, and
/// archives old terminal-task `events` into `data_dir/archives/`.
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
    let now = SystemTime::now();

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
        report.repos_scanned.push(repo_label.clone());
        // Re-read tasks after acquiring `.run.lock` so state transitions that
        // happened before lock acquisition are observed accurately.
        let fresh_tasks = match collect_db_state(data_dir) {
            Ok((tasks, _, _)) => tasks,
            Err(err) => {
                report
                    .warnings
                    .push(format!("{repo_label}: db re-read: {err}"));
                all_tasks.clone()
            }
        };
        clean_repo(
            &repo,
            &repo_id,
            data_dir,
            worktrees_dir,
            &fresh_tasks,
            opts,
            now,
            &mut report,
        );
    }

    clean_expired_artifacts(data_dir, opts, &running_runs, now, &mut report);
    scratch::clean_legacy_oracle_scratch(data_dir, opts, now, &mut report);
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

#[allow(clippy::too_many_arguments)]
fn clean_repo(
    repo: &Path,
    repo_id: &str,
    data_dir: &Path,
    worktrees_dir: &Path,
    all_tasks: &[Task],
    opts: &GcOptions,
    now: SystemTime,
    report: &mut GcReport,
) {
    let canonical_repo = canonical_main_repo(repo);
    let repo_tasks: Vec<&Task> = all_tasks
        .iter()
        .filter(|t| canonical_main_repo(Path::new(&t.repo_root)) == canonical_repo)
        .collect();
    let active_ids: HashSet<String> = repo_tasks.iter().map(|t| t.id.clone()).collect();
    let all_task_ids: HashSet<String> = all_tasks.iter().map(|t| t.id.clone()).collect();

    match worktree::remove_orphan_batch_worktrees_with(
        repo,
        worktrees_dir,
        repo_id,
        &active_ids,
        opts.dry_run,
        opts.force,
        opts.cwd.as_deref(),
    ) {
        Ok(sweep) => {
            report.removed_orphan_batches.extend(sweep.removed);
            report.warnings.extend(sweep.warnings);
        }
        Err(err) => report
            .warnings
            .push(format!("{}: orphan batch sweep: {err}", repo.display())),
    }

    let registered = worktree::list(repo)
        .map_err(|err| {
            report
                .warnings
                .push(format!("{}: worktree list: {err}", repo.display()));
        })
        .ok();
    let refs = RepoRefs::list(repo)
        .map_err(|err| {
            report
                .warnings
                .push(format!("{}: list refs: {err}", repo.display()));
        })
        .ok();
    let active_ops = worktree::active_in_progress_branches(repo).unwrap_or_else(|err| {
        report
            .warnings
            .push(format!("{}: active ops: {err}", repo.display()));
        HashSet::new()
    });

    if let (Some(registered), Some(refs)) = (registered.as_deref(), refs.as_ref()) {
        let task_opts = TaskCleanOpts {
            dry_run: opts.dry_run,
            force: opts.force,
            cwd: opts.cwd.as_deref(),
            expected_head: None,
        };
        for task in &repo_tasks {
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
            if let Err(err) = clean_task_artifacts_by_id(
                repo,
                worktrees_dir,
                &task.id,
                registered,
                refs,
                &active_ops,
                &task_opts,
                report,
            ) {
                report.warnings.push(format!("{}: {err}", task.id));
            }
        }

        review::clean_merged_review_and_orphan_tasks(
            repo,
            data_dir,
            worktrees_dir,
            &repo_tasks,
            &all_task_ids,
            registered,
            refs,
            &active_ops,
            opts,
            now,
            report,
        );

        scratch::clean_repo_scratch(
            repo,
            data_dir,
            registered,
            opts.cwd.as_deref(),
            opts.dry_run,
            now,
            report,
        );
    }

    if opts.merged_branches {
        merged_branches::sweep_merged_branches(repo, opts, report);
    }

    if !opts.dry_run {
        match worktree::prune_with_expire(repo, PRUNE_EXPIRE) {
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
}
