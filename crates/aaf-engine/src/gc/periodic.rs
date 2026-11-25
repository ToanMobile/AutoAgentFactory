//! Lightweight periodic garbage collection and low-disk guard (ZERO `unsafe`).

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

use crate::git::worktree;

use super::{
    GcError, GcOptions, GcReport, canonical_or_owned, clean_expired_artifacts, collect_db_state,
};

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
        let canonical_repo = canonical_or_owned(repo_root);
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
        let canonical_repo = canonical_or_owned(repo_root);
        report
            .repos_scanned
            .push(canonical_repo.display().to_string());
        let repo_id = aaf_fp::repo_id(&canonical_repo);

        if let Some((ref all_tasks, _, _)) = db_state {
            let active_ids: HashSet<String> = all_tasks
                .iter()
                .filter(|t| canonical_or_owned(Path::new(&t.repo_root)) == canonical_repo)
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

        match worktree::prune(&canonical_repo) {
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
