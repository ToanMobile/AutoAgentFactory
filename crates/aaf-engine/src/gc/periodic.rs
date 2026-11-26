//! Lightweight periodic garbage collection and low-disk guard (ZERO `unsafe`).

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

use crate::git::worktree;

use super::refs::RepoRefs;
use super::scratch;
use super::task_clean::{ResolvedBase, resolve_repo_base};
use super::{
    GcError, GcOptions, GcReport, TaskCleanOpts, canonical_main_repo, clean_expired_artifacts,
    clean_task_artifacts_by_id, collect_db_state,
};

/// Result of [`clean_repo_offline_unlocked`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OfflineSweepReport {
    pub removed_orphan_batches: Vec<String>,
    pub removed_worktrees: Vec<String>,
    pub removed_branches: Vec<String>,
    pub removed_snapshot_refs: usize,
    pub removed_files: Vec<String>,
    pub removed_merge_refs: usize,
    pub removed_stale_fp: usize,
    pub warnings: Vec<String>,
}

/// Offline, lock-free per-repo cleanup safe to invoke from `aaf run` startup
/// while `.run.lock` is already held by the caller.
///
/// - Never acquires `.run.lock`.
/// - Never runs network commands (`gh` / `git fetch`).
/// - Never opens or writes `aaf.db` (uses `done_task_ids` and `active_ids`
///   supplied by the caller).
/// - Fail-closed for `Done` tasks: cleans only clean, unlocked, non-`cwd`,
///   non-in-progress worktrees whose branch/HEAD is verified merged into the
///   base ref. Use [`clean_repo_offline_unlocked_with_heads`] to supply
///   `MergeResult.head_sha` for squash-merged `Done` tasks.
/// - Uses `force = true` **only** for dead-PID batch/scratch artifacts.
pub fn clean_repo_offline_unlocked(
    repo_root: &Path,
    data_dir: &Path,
    worktrees_dir: &Path,
    done_task_ids: &[String],
    active_ids: &HashSet<String>,
    cwd: Option<&Path>,
) -> OfflineSweepReport {
    let done_tasks: Vec<(String, Option<String>)> =
        done_task_ids.iter().map(|id| (id.clone(), None)).collect();
    clean_repo_offline_unlocked_with_heads(
        repo_root,
        data_dir,
        worktrees_dir,
        &done_tasks,
        active_ids,
        cwd,
    )
}

/// Like [`clean_repo_offline_unlocked`], taking `(task_id, expected_head_sha)`
/// pairs for `Done` tasks (from `MergeResult` events).
///
/// A `Done` task's worktree and branch are removed only when either:
/// - `expected_head == Some(oid)` and both worktree `HEAD` and branch tip equal
///   `oid`, or
/// - `expected_head == None` and the branch/HEAD is an ancestor of the repo's
///   resolved base ref.
pub fn clean_repo_offline_unlocked_with_heads(
    repo_root: &Path,
    data_dir: &Path,
    worktrees_dir: &Path,
    done_tasks: &[(String, Option<String>)],
    active_ids: &HashSet<String>,
    cwd: Option<&Path>,
) -> OfflineSweepReport {
    let mut gc_report = GcReport::default();
    let repo = canonical_main_repo(repo_root);
    if !repo.is_dir() || aaf_fp::repo_root(&repo).is_err() {
        return OfflineSweepReport::default();
    }
    let repo_id = aaf_fp::repo_id(&repo);

    let registered = match worktree::list(&repo) {
        Ok(list) => Some(list),
        Err(err) => {
            gc_report
                .warnings
                .push(format!("{}: worktree list: {err}", repo.display()));
            None
        }
    };
    let refs = match RepoRefs::list(&repo) {
        Ok(refs) => Some(refs),
        Err(err) => {
            gc_report
                .warnings
                .push(format!("{}: list refs: {err}", repo.display()));
            None
        }
    };
    let active_ops = registered
        .as_deref()
        .map(worktree::active_in_progress_branches_for)
        .unwrap_or_default();

    let batch_sweep = match (registered.as_deref(), refs.as_ref()) {
        (Some(reg), Some(r)) => {
            let batch_branches = r.batch_short_branches();
            worktree::remove_orphan_batch_worktrees_prepared(
                &repo,
                worktrees_dir,
                &repo_id,
                reg,
                &active_ops,
                Some(&batch_branches),
                active_ids,
                false,
                false,
                true,
                cwd,
            )
        }
        _ => worktree::remove_orphan_batch_worktrees_dead_pid_force(
            &repo,
            worktrees_dir,
            &repo_id,
            active_ids,
            false,
            cwd,
        ),
    };
    match batch_sweep {
        Ok(sweep) => {
            gc_report.removed_orphan_batches.extend(sweep.removed);
            gc_report.warnings.extend(sweep.warnings);
        }
        Err(err) => gc_report
            .warnings
            .push(format!("{}: orphan batch sweep: {err}", repo.display())),
    }

    if let (Some(registered), Some(refs)) = (registered.as_deref(), refs.as_ref()) {
        let mut cached_base: Option<Result<ResolvedBase, String>> = None;
        for (task_id, expected_head) in done_tasks {
            if worktree::validate_task_id(task_id).is_err() {
                gc_report
                    .warnings
                    .push(format!("skipped invalid task id {task_id:?}"));
                continue;
            }
            let branch = format!("{}{task_id}", worktree::BRANCH_PREFIX);
            let branch_ref = format!("refs/heads/{branch}");
            let expected_wt = worktree::worktree_path(&repo, worktrees_dir, task_id).ok();
            let needs_base = expected_head.is_none()
                && (refs.has_branch(&branch_ref)
                    || expected_wt.as_deref().is_some_and(|exp| {
                        exp.exists()
                            || worktree::find_registered_for_task(registered, exp, &branch)
                                .is_some()
                    }));
            let base = if needs_base {
                let res = cached_base.get_or_insert_with(|| resolve_repo_base(&repo));
                res.as_ref().ok()
            } else {
                None
            };
            let clean_opts = TaskCleanOpts {
                dry_run: false,
                force: false,
                require_verified_merge: true,
                base,
                cwd,
                expected_head: expected_head.as_deref(),
            };
            if let Err(err) = clean_task_artifacts_by_id(
                &repo,
                worktrees_dir,
                task_id,
                registered,
                refs,
                &active_ops,
                &clean_opts,
                &mut gc_report,
            ) {
                gc_report.warnings.push(format!("{task_id}: {err}"));
            }
        }

        scratch::clean_repo_scratch(
            &repo,
            data_dir,
            registered,
            refs.merge_refs(),
            cwd,
            false,
            SystemTime::now(),
            &mut gc_report,
        );
    }

    if let Err(err) = worktree::prune_with_expire(&repo, "5.minutes.ago") {
        gc_report
            .warnings
            .push(format!("{}: worktree prune: {err}", repo.display()));
    }
    match aaf_fp::sweep_stale(&repo) {
        Ok(n) => gc_report.removed_stale_fp += n,
        Err(err) => gc_report
            .warnings
            .push(format!("{}: sweep_stale: {err}", repo.display())),
    }

    OfflineSweepReport {
        removed_orphan_batches: gc_report.removed_orphan_batches,
        removed_worktrees: gc_report.removed_worktrees,
        removed_branches: gc_report.removed_branches,
        removed_snapshot_refs: gc_report.removed_snapshot_refs,
        removed_files: gc_report.removed_files,
        removed_merge_refs: gc_report.removed_merge_refs,
        removed_stale_fp: gc_report.removed_stale_fp,
        warnings: gc_report.warnings,
    }
}

/// Default minimum free disk space checked during lightweight sweeps (1 GiB).
pub const DEFAULT_MIN_FREE_DISK_BYTES: u64 = 1024 * 1024 * 1024;

/// Parses available bytes from POSIX `df -k -P` stdout.
pub(super) fn parse_df_available_bytes(stdout: &str) -> Option<u64> {
    let data_line = stdout
        .lines()
        .skip(1)
        .find(|line| !line.trim().is_empty())?;
    let tokens: Vec<&str> = data_line.split_whitespace().collect();
    let pct_idx = tokens.iter().enumerate().find_map(|(idx, tok)| {
        let num = tok.strip_suffix('%')?;
        (idx >= 1 && !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit())).then_some(idx)
    })?;
    let avail_kib: u64 = tokens.get(pct_idx - 1)?.parse().ok()?;
    Some(avail_kib.saturating_mul(1024))
}

/// Checks that the filesystem hosting `path` has at least `min_free_bytes`
/// available, using POSIX `df -k -P` with zero `unsafe`.
pub fn check_free_disk_bytes(path: &Path, min_free_bytes: u64) -> Result<(), GcError> {
    if min_free_bytes == 0 {
        return Ok(());
    }
    let probe = path.ancestors().find(|p| p.exists()).unwrap_or(path);
    let Ok(output) = Command::new("df").args(["-k", "-P"]).arg(probe).output() else {
        return Ok(());
    };
    if !output.status.success() {
        return Ok(());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(available_bytes) = parse_df_available_bytes(&stdout) else {
        return Ok(());
    };
    if available_bytes < min_free_bytes {
        return Err(GcError::LowDisk {
            path: path.display().to_string(),
            available_bytes,
            min_free_bytes,
        });
    }
    Ok(())
}

/// Non-locking periodic GC safe to call from an active factory loop on
/// `repo_root`. Cleans expired `data_dir` artifacts and audits (only when DB
/// state is readable), stale fingerprint temps, orphan `aaf/batch-*`
/// worktrees/branches, prunes gone worktree entries, and checks free disk space
/// on `worktrees_dir`. Never writes or deletes rows in `aaf.db`.
pub fn lightweight_sweep(
    repo_root: &Path,
    data_dir: &Path,
    worktrees_dir: &Path,
    now: i64,
) -> GcReport {
    let mut report = GcReport::default();
    let opts = GcOptions::default();
    let now_time = SystemTime::UNIX_EPOCH + Duration::from_secs(u64::try_from(now).unwrap_or(0));

    let db_state = match collect_db_state(data_dir) {
        Ok(state) => Some(state),
        Err(err) => {
            report.warnings.push(format!("db state: {err}"));
            None
        }
    };

    if let Some((ref all_tasks, ref running_runs, _)) = db_state {
        clean_expired_artifacts(data_dir, &opts, running_runs, now_time, &mut report);

        let mut busy_repos = HashSet::new();
        busy_repos.insert(repo_root.display().to_string());
        let canonical_repo = canonical_main_repo(repo_root);
        busy_repos.insert(canonical_repo.display().to_string());
        let protected_audits = crate::gc_audits::collect_protected_audit_ids_with_locks(
            all_tasks,
            &busy_repos,
            data_dir,
            worktrees_dir,
        );
        let cutoff = now_time
            .checked_sub(Duration::from_secs(
                opts.older_than_days.saturating_mul(86_400),
            ))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let audit_stats = crate::gc_audits::clean_expired_audits(
            data_dir,
            cutoff,
            opts.older_than_days,
            &protected_audits,
            false,
        );
        report.removed_files.extend(audit_stats.removed_files);
        report.warnings.extend(audit_stats.warnings);
    }

    if repo_root.is_dir() && aaf_fp::repo_root(repo_root).is_ok() {
        let canonical_repo = canonical_main_repo(repo_root);
        report
            .repos_scanned
            .push(canonical_repo.display().to_string());
        let repo_id = aaf_fp::repo_id(&canonical_repo);

        if let Some((ref all_tasks, _, _)) = db_state {
            let active_ids: HashSet<String> = all_tasks
                .iter()
                .filter(|t| canonical_main_repo(Path::new(&t.repo_root)) == canonical_repo)
                .map(|t| t.id.clone())
                .collect();

            match worktree::remove_orphan_batch_worktrees(
                &canonical_repo,
                worktrees_dir,
                &repo_id,
                &active_ids,
                false,
            ) {
                Ok(sweep) => {
                    report.removed_orphan_batches.extend(sweep.removed);
                    report.warnings.extend(sweep.warnings);
                }
                Err(err) => report.warnings.push(format!(
                    "{}: orphan batch sweep: {err}",
                    canonical_repo.display()
                )),
            }
        }

        match worktree::prune_with_expire(&canonical_repo, "5.minutes.ago") {
            Ok(()) => report.pruned_repos += 1,
            Err(err) => report.warnings.push(format!(
                "{}: worktree prune: {err}",
                canonical_repo.display()
            )),
        }

        match aaf_fp::sweep_stale(&canonical_repo) {
            Ok(n) => report.removed_stale_fp += n,
            Err(err) => report
                .warnings
                .push(format!("{}: sweep_stale: {err}", canonical_repo.display())),
        }
    }

    if let Err(err) = check_free_disk_bytes(worktrees_dir, DEFAULT_MIN_FREE_DISK_BYTES) {
        report.warnings.push(err.to_string());
    }

    report
}
