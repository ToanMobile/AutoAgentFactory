//! Safe per-task worktree, branch, and snapshot cleanup shared by `aaf gc`,
//! `gc/review.rs`, and offline `startup_sweep`.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use super::refs::RepoRefs;
use super::{GcError, GcReport, PRUNE_EXPIRE};
use crate::git::cmd::{git, git_raw};
use crate::git::path_str;
use crate::git::pr_cleanup::main_work_tree;
use crate::git::worktree::{self, BRANCH_PREFIX, WorktreeEntry};

pub(super) struct TaskCleanOpts<'a> {
    pub(super) dry_run: bool,
    pub(super) force: bool,
    pub(super) cwd: Option<&'a Path>,
    pub(super) expected_head: Option<&'a str>,
}

pub(super) fn canonical_or_owned(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub(super) fn canonical_main_repo(path: &Path) -> PathBuf {
    canonical_or_owned(&main_work_tree(path))
}

pub(super) fn record_removed_file(report: &mut GcReport, path: &Path) {
    let display = path.display().to_string();
    if !report.removed_files.contains(&display) {
        report.removed_files.push(display);
    }
}

pub(super) fn resolved_path(path: &Path) -> PathBuf {
    if let Ok(real) = fs::canonicalize(path) {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => {
            fs::canonicalize(parent).map_or_else(|_| path.to_path_buf(), |dir| dir.join(name))
        }
        _ => path.to_path_buf(),
    }
}

pub(super) fn check_cwd_blocks(cwd: Option<&Path>, wt: &Path) -> Option<String> {
    let cwd = cwd?;
    let Ok(canon_cwd) = fs::canonicalize(cwd) else {
        return Some(format!("cannot canonicalize cwd {}", cwd.display()));
    };
    let Ok(canon_wt) = fs::canonicalize(wt) else {
        return Some(format!("cannot canonicalize worktree {}", wt.display()));
    };
    canon_cwd.starts_with(&canon_wt).then(|| {
        format!(
            "cwd {} is inside worktree {}",
            canon_cwd.display(),
            wt.display()
        )
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn clean_task_artifacts_by_id(
    repo: &Path,
    worktrees_dir: &Path,
    task_id: &str,
    registered: &[WorktreeEntry],
    refs: &RepoRefs,
    active_ops: &HashSet<String>,
    clean_opts: &TaskCleanOpts<'_>,
    report: &mut GcReport,
) -> Result<(), GcError> {
    let branch = format!("{BRANCH_PREFIX}{task_id}");
    let expected_path = worktree::worktree_path(repo, worktrees_dir, task_id)?;
    let expected_res = resolved_path(&expected_path);
    let branch_entry = registered
        .iter()
        .find(|e| e.branch.as_deref() == Some(branch.as_str()));
    let path_entry = registered
        .iter()
        .find(|e| resolved_path(&e.path) == expected_res);
    let reg_entry = branch_entry.or(path_entry);

    if let Some(warning) =
        reg_entry.and_then(|e| worktree::foreign_checkout(task_id, e, registered, &expected_path))
    {
        report.warnings.push(warning);
        return Ok(());
    }
    let existing_wt_path = reg_entry
        .filter(|e| e.path.exists())
        .map(|e| e.path.clone())
        .or_else(|| expected_path.exists().then_some(expected_path.clone()));

    if reg_entry.is_some_and(|e| e.locked) {
        let msg = if existing_wt_path.is_none() {
            format!("{task_id}: worktree is locked and its directory is missing: kept")
        } else if clean_opts.dry_run {
            format!(
                "{task_id}: worktree remove: would be refused: cannot remove a locked working tree"
            )
        } else {
            format!("{task_id}: worktree remove: cannot remove a locked working tree")
        };
        report.warnings.push(msg);
        return Ok(());
    }

    if let Some(wt_path) = &existing_wt_path {
        if let Some(reason) = check_cwd_blocks(clean_opts.cwd, wt_path) {
            report.warnings.push(format!("{task_id}: {reason}: kept"));
            return Ok(());
        }
        if reg_entry.is_some() || wt_path.join(".git").exists() {
            match worktree::is_operation_in_progress(wt_path) {
                Ok(true) => {
                    report
                        .warnings
                        .push(format!("{task_id}: git operation in progress: kept"));
                    return Ok(());
                }
                Ok(false) => {}
                Err(err) => {
                    let prefix = if clean_opts.dry_run && reg_entry.is_some() {
                        "worktree remove: would be refused:"
                    } else {
                        "worktree remove:"
                    };
                    report.warnings.push(format!("{task_id}: {prefix} {err}"));
                    return Ok(());
                }
            }
        }
        if active_ops.contains(&branch) {
            report.warnings.push(format!(
                "{task_id}: git operation in progress on {branch}: kept"
            ));
            return Ok(());
        }
        let dirty = git_raw(wt_path, &["status", "--porcelain"])
            .map(|o| !o.success() || !o.stdout.trim().is_empty())
            .unwrap_or(true);
        if dirty && !clean_opts.force {
            report.skipped_dirty.push(task_id.to_string());
            return Ok(());
        }
        if let Some(expected_head) = clean_opts.expected_head {
            let actual = match git(wt_path, &["rev-parse", "HEAD"]) {
                Ok(oid) => oid,
                Err(err) => {
                    report
                        .warnings
                        .push(format!("{task_id}: read HEAD failed: {err}"));
                    return Ok(());
                }
            };
            if !actual.eq_ignore_ascii_case(expected_head) {
                report.warnings.push(format!(
                    "{task_id}: worktree HEAD {actual} != verified {expected_head}: kept"
                ));
                return Ok(());
            }
        }
        if clean_opts.dry_run
            && reg_entry.is_some()
            && let Some(reason) = removal_refusal(wt_path, clean_opts.force)
                .map_or_else(|err| Some(err.to_string()), |r| r.map(str::to_string))
        {
            report.warnings.push(format!(
                "{task_id}: worktree remove: would be refused: {reason}"
            ));
            return Ok(());
        }
        if !clean_opts.dry_run {
            if reg_entry.is_some() {
                let p = path_str(wt_path)?;
                let out = if clean_opts.force {
                    git_raw(repo, &["worktree", "remove", "--force", p])?
                } else {
                    git_raw(repo, &["worktree", "remove", p])?
                };
                if !out.success() {
                    report
                        .warnings
                        .push(format!("{task_id}: worktree remove: {}", out.stderr.trim()));
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
    } else {
        if active_ops.contains(&branch) {
            report.warnings.push(format!(
                "{task_id}: git operation in progress on {branch}: kept"
            ));
            return Ok(());
        }
        if reg_entry.is_some() && !clean_opts.dry_run {
            worktree::prune_with_expire(repo, PRUNE_EXPIRE)?;
        }
    }

    let branch_ref = format!("refs/heads/{branch}");
    if refs.has_branch(&branch_ref) {
        if let Some(expected_head) = clean_opts.expected_head {
            let rev = git_raw(repo, &["rev-parse", "--verify", "--quiet", &branch_ref])?;
            if rev.success() && !rev.stdout.trim().eq_ignore_ascii_case(expected_head) {
                report.warnings.push(format!(
                    "{task_id}: branch tip {} != verified {expected_head}: kept",
                    rev.stdout.trim()
                ));
                return Ok(());
            }
        }
        if !clean_opts.dry_run {
            let del = git_raw(repo, &["branch", "-D", &branch])?;
            if !del.success() && !del.stderr.to_ascii_lowercase().contains("not found") {
                report
                    .warnings
                    .push(format!("{task_id}: branch delete: {}", del.stderr.trim()));
                return Ok(());
            }
        }
        report.removed_branches.push(branch);
    }

    let snapshots = refs.snapshot_count(task_id);
    if clean_opts.dry_run {
        report.removed_snapshot_refs += snapshots;
    } else if snapshots > 0 {
        match worktree::delete_snapshot_refs(repo, task_id) {
            Ok(deleted) => report.removed_snapshot_refs += deleted,
            Err(err) => report
                .warnings
                .push(format!("{task_id}: snapshot refs: {err}")),
        }
    }
    Ok(())
}

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
