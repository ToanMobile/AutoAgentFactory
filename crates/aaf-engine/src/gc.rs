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

use aaf_domain::event::EventKind;
use aaf_domain::ports::{EventSink, PortError, TaskStore};
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
pub(crate) use task_clean::verified_merged_head_from_events;
use task_clean::{
    ResolvedBase, TaskCleanOpts, canonical_main_repo, canonical_or_owned, check_cwd_blocks,
    clean_task_artifacts_by_id, record_removed_file, resolve_repo_base, resolved_path,
};

pub use archive::{GcArchiveStats, archive_old_terminal_events};
pub use periodic::{
    DEFAULT_MIN_FREE_DISK_BYTES, OfflineSweepReport, check_free_disk_bytes,
    clean_repo_offline_unlocked, clean_repo_offline_unlocked_with_heads, lightweight_sweep,
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

fn collect_repo_lock_ids(repo: &Path, all_tasks: &[Task]) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    ids.insert(aaf_fp::repo_id(repo));
    if let Ok(entries) = worktree::list(repo) {
        for entry in entries {
            let canon = canonical_or_owned(&entry.path);
            ids.insert(aaf_fp::repo_id(&canon));
        }
    }
    for task in all_tasks {
        if task.repo_root.trim().is_empty() {
            continue;
        }
        let task_root = Path::new(&task.repo_root);
        if canonical_main_repo(task_root) == repo {
            let canon = canonical_or_owned(task_root);
            ids.insert(aaf_fp::repo_id(&canon));
        }
    }
    ids
}

fn try_lock_repo_set(
    data_dir: &Path,
    lock_ids: &BTreeSet<String>,
    dry_run: bool,
) -> Result<Option<Vec<RepoLockGuard>>, GcError> {
    let mut guards = Vec::with_capacity(lock_ids.len());
    for id in lock_ids {
        match try_lock_repo(data_dir, id, dry_run) {
            Ok(Some(guard)) => guards.push(guard),
            Ok(None) => {}
            Err(GcError::Io(err)) if err.kind() == std::io::ErrorKind::WouldBlock => {
                drop(guards);
                return Ok(None);
            }
            Err(err) => return Err(err),
        }
    }
    Ok(Some(guards))
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
        let lock_ids = collect_repo_lock_ids(&repo, &all_tasks);
        let Some(_locks) = try_lock_repo_set(data_dir, &lock_ids, opts.dry_run)? else {
            report.skipped_busy.push(repo_label);
            continue;
        };
        // Re-read tasks after acquiring `.run.lock` so state transitions that
        // happened before lock acquisition are observed accurately. Fail closed
        // if the re-read fails rather than acting on a stale pre-lock snapshot.
        let fresh_tasks = match collect_db_state(data_dir) {
            Ok((tasks, _, _)) => tasks,
            Err(err) => {
                report
                    .warnings
                    .push(format!("{repo_label}: db re-read failed (skipped): {err}"));
                continue;
            }
        };
        report.repos_scanned.push(repo_label);
        clean_repo(
            &repo,
            &repo_id,
            &lock_ids,
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
    lock_ids: &BTreeSet<String>,
    data_dir: &Path,
    worktrees_dir: &Path,
    all_tasks: &[Task],
    opts: &GcOptions,
    now: SystemTime,
    report: &mut GcReport,
) {
    let repo_tasks: Vec<&Task> = all_tasks
        .iter()
        .filter(|t| canonical_main_repo(Path::new(&t.repo_root)) == repo)
        .collect();
    let active_ids: HashSet<String> = repo_tasks.iter().map(|t| t.id.clone()).collect();
    let all_task_ids: HashSet<String> = all_tasks.iter().map(|t| t.id.clone()).collect();

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
    let active_ops = registered
        .as_deref()
        .map(worktree::active_in_progress_branches_for)
        .unwrap_or_default();

    let mut batch_repo_ids = vec![repo_id];
    for id in lock_ids {
        if id != repo_id && worktrees_dir.join(id).is_dir() {
            batch_repo_ids.push(id.as_str());
        }
    }
    let pre_batch_branches = refs.as_ref().map(RepoRefs::batch_short_branches);
    for sweep_repo_id in batch_repo_ids {
        let res = match registered.as_deref() {
            Some(reg) => worktree::remove_orphan_batch_worktrees_prepared(
                repo,
                worktrees_dir,
                sweep_repo_id,
                reg,
                &active_ops,
                pre_batch_branches.as_deref(),
                &active_ids,
                opts.dry_run,
                opts.force,
                opts.force,
                opts.cwd.as_deref(),
            ),
            None => worktree::remove_orphan_batch_worktrees_with(
                repo,
                worktrees_dir,
                sweep_repo_id,
                &active_ids,
                opts.dry_run,
                opts.force,
                opts.cwd.as_deref(),
            ),
        };
        match res {
            Ok(sweep) => {
                for name in sweep.removed {
                    if !report.removed_orphan_batches.contains(&name) {
                        report.removed_orphan_batches.push(name);
                    }
                }
                for w in sweep.warnings {
                    if !report.warnings.contains(&w) {
                        report.warnings.push(w);
                    }
                }
            }
            Err(err) => report
                .warnings
                .push(format!("{}: orphan batch sweep: {err}", repo.display())),
        }
    }

    if let (Some(registered), Some(refs)) = (registered.as_deref(), refs.as_ref()) {
        let mut ro_db: Option<Option<Db>> = None;
        let mut cached_base: Option<Result<ResolvedBase, String>> = None;
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
            let require_verified_merge = task.state == TaskState::Done && !opts.force;
            let branch = format!("{}{}", worktree::BRANCH_PREFIX, task.id);
            let branch_ref = format!("refs/heads/{branch}");
            let expected_wt = worktree::worktree_path(repo, worktrees_dir, &task.id).ok();
            let has_artifacts = refs.has_branch(&branch_ref)
                || expected_wt.as_deref().is_some_and(|exp| {
                    exp.exists()
                        || worktree::find_registered_for_task(registered, exp, &branch).is_some()
                });
            let expected_head = if require_verified_merge && has_artifacts {
                let db_opt = ro_db.get_or_insert_with(|| {
                    let db_path = data_dir.join("aaf.db");
                    db_path
                        .is_file()
                        .then(|| Db::open_read_only(&db_path).ok())
                        .flatten()
                });
                db_opt.as_ref().and_then(|db| {
                    db.events_for_task_by_kinds(&task.id, &[EventKind::MergeResult], 10)
                        .ok()
                        .and_then(|events| verified_merged_head_from_events(&events))
                })
            } else {
                None
            };
            let base = if require_verified_merge && has_artifacts && expected_head.is_none() {
                cached_base
                    .get_or_insert_with(|| resolve_repo_base(repo))
                    .as_ref()
                    .ok()
            } else {
                None
            };
            let task_opts = TaskCleanOpts {
                dry_run: opts.dry_run,
                force: opts.force,
                require_verified_merge,
                base,
                cwd: opts.cwd.as_deref(),
                expected_head: expected_head.as_deref(),
            };
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
            cached_base.as_ref(),
            opts,
            now,
            report,
        );

        scratch::clean_repo_scratch(
            repo,
            data_dir,
            registered,
            refs.merge_refs(),
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
