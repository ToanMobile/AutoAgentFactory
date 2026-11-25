//! Fail-closed post-merge cleanup for `aaf merge`:
//! - safely deletes the remote head branch (replacing `gh pr merge --delete-branch`),
//! - atomically transitions matching `aaf/<task_id>` tasks `Review -> Done` in `aaf.db`,
//! - removes clean linked worktrees, local branches, and `refs/aaf/snapshots/<id>/*`.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use aaf_domain::ports::Clock;
use serde::Serialize;

use super::cmd::{git, git_raw};
use super::gh_client::PrInfo;
use super::pr_cleanup_db::{DbDecision, transition_review_task_done};
use super::pr_merge::{PrMergeOutcome, PrMergeRequest, sync_local_base};
use super::pr_merge_support::{MergeLockGuard, valid_sha40};
use super::worktree::{self, WorktreeEntry};
use super::{GitError, check_branch_name, path_str};
use crate::gate;

const OP_MARKERS: [&str; 5] = [
    "rebase-merge",
    "rebase-apply",
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
];

/// Summary of post-merge local/remote/DB cleanup actions and warnings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PrCleanupReport {
    pub worktree_removed: bool,
    pub branch_deleted: bool,
    pub remote_branch_deleted: bool,
    pub snapshots_deleted: usize,
    pub task_marked_done: bool,
    pub warnings: Vec<String>,
}

impl PrCleanupReport {
    pub fn summary_note(&self) -> Option<String> {
        let mut parts = Vec::new();
        for (flag, label) in [
            (self.remote_branch_deleted, "remote branch deleted"),
            (self.worktree_removed, "worktree removed"),
            (self.branch_deleted, "local branch deleted"),
            (self.task_marked_done, "task marked done"),
        ] {
            if flag {
                parts.push(label.to_string());
            }
        }
        if self.snapshots_deleted > 0 {
            parts.push(format!(
                "{} snapshot ref(s) deleted",
                self.snapshots_deleted
            ));
        }
        parts.extend(self.warnings.iter().map(|w| format!("warning: {w}")));
        (!parts.is_empty()).then(|| parts.join("; "))
    }
}

/// Resolves the main worktree root for `path`, even when invoked inside a linked worktree.
pub fn main_work_tree(path: &Path) -> PathBuf {
    if let Some(main) = aaf_fp::repository_dirs(path).into_iter().last() {
        return main;
    }
    if let Ok(common) = git(
        path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ) {
        let common_path = PathBuf::from(common.trim());
        if common_path.file_name().is_some_and(|n| n == ".git")
            && let Some(parent) = common_path.parent()
        {
            return fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
        }
    }
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Case-insensitive ASCII protection for base/default/release/public/non-ASCII branches.
pub(crate) fn is_protected_branch(branch: &str, base_ref_name: &str) -> bool {
    let trimmed = branch.trim();
    if trimmed.is_empty()
        || !trimmed.is_ascii()
        || trimmed.eq_ignore_ascii_case(base_ref_name.trim())
    {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "main" | "master" | "develop" | "development" | "trunk" | "head"
    ) || lower.starts_with("release/")
        || lower.starts_with("public-")
}

/// Runs post-merge remote deletion, local cleanup, and base fast-forward after
/// `gh pr merge` succeeds. Never fails the merge itself.
pub(super) fn finish_merged(
    req: &PrMergeRequest,
    pr: &PrInfo,
    caller_cwd: Option<&Path>,
    clock: &dyn Clock,
) -> (String, PrCleanupReport) {
    let main_repo = main_work_tree(&req.repo_root);
    let mut report = PrCleanupReport::default();
    delete_remote_branch(&main_repo, req, pr, &mut report);
    cleanup_merged_pr_local_into(&main_repo, req, pr, caller_cwd, clock, &mut report);
    let mut note = sync_local_base(&main_repo, &req.remote, &pr.base_ref_name);
    if let Some(summary) = report.summary_note() {
        if !note.is_empty() {
            note.push_str("; ");
        }
        note.push_str(&summary);
    }
    (note, report)
}

/// Pairs an early `PrMergeOutcome` with optional fail-closed cleanup when `AlreadyMerged`.
pub(super) fn finalize_early(
    outcome: PrMergeOutcome,
    req: &PrMergeRequest,
    pr: &PrInfo,
    caller_cwd: Option<&Path>,
    lock_held: bool,
    clock: &dyn Clock,
) -> (PrMergeOutcome, Option<PrCleanupReport>) {
    if !matches!(outcome, PrMergeOutcome::AlreadyMerged { .. }) {
        return (outcome, None);
    }
    let report = cleanup_already_merged(req, pr, caller_cwd, lock_held, clock);
    (outcome, report)
}

fn try_lock_for_already_merged(
    data_dir: &Path,
    main_repo: &Path,
) -> Result<MergeLockGuard, String> {
    let locks_dir = data_dir.join("locks");
    let common_id = gate::stats_repo_id(main_repo);
    crate::db::private::create_dir(&locks_dir).map_err(|e| format!("lock dir error: {e}"))?;
    let lock_path = locks_dir.join(format!("{common_id}.merge.lock"));
    crate::db::private::open_file(&lock_path).map_err(|e| format!("lock file error: {e}"))?;
    match MergeLockGuard::acquire(&locks_dir, &common_id, Duration::ZERO) {
        Ok(guard) => Ok(guard),
        Err(GitError::LockTimeout(_)) => Err("merge lock is busy".to_string()),
        Err(err) => Err(format!("lock error: {err}")),
    }
}

fn verify_already_merged_ancestry(
    main_repo: &Path,
    remote: &str,
    base: &str,
    merge_oid: &str,
) -> Result<(), String> {
    if remote.is_empty() || remote.starts_with('-') || check_branch_name(main_repo, base).is_err() {
        return Err("invalid remote or base branch".to_string());
    }
    let refspec = format!("+refs/heads/{base}:refs/remotes/{remote}/{base}");
    if !git_raw(main_repo, &["fetch", "--quiet", "--", remote, &refspec])
        .is_ok_and(|out| out.success())
    {
        return Err(format!("could not fetch {remote}/{base}"));
    }
    let remote_base = format!("refs/remotes/{remote}/{base}");
    if !git_raw(
        main_repo,
        &["merge-base", "--is-ancestor", merge_oid, &remote_base],
    )
    .is_ok_and(|out| out.success())
    {
        return Err(format!(
            "merge commit {merge_oid} is not an ancestor of {remote_base}"
        ));
    }
    Ok(())
}

/// Fail-closed local cleanup for `AlreadyMerged`: requires `!dry_run`,
/// `!is_cross_repository`, matching `head_ref_oid == expect_head`, `.merge.lock`,
/// and `mergeCommit.oid` verified as an ancestor of `refs/remotes/<remote>/<base>`.
pub(super) fn cleanup_already_merged(
    req: &PrMergeRequest,
    pr: &PrInfo,
    caller_cwd: Option<&Path>,
    lock_held: bool,
    clock: &dyn Clock,
) -> Option<PrCleanupReport> {
    if req.dry_run
        || pr.is_cross_repository
        || is_protected_branch(&pr.head_ref_name, &pr.base_ref_name)
        || !pr.head_ref_oid.eq_ignore_ascii_case(&req.expect_head)
    {
        return None;
    }
    let merge_oid = pr
        .merge_commit
        .as_ref()
        .map(|m| m.oid.trim())
        .filter(|oid| valid_sha40(oid))?;

    let main_repo = main_work_tree(&req.repo_root);
    let mut report = PrCleanupReport::default();
    let _guard = if lock_held {
        None
    } else {
        match try_lock_for_already_merged(&req.data_dir, &main_repo) {
            Ok(guard) => Some(guard),
            Err(why) => {
                report
                    .warnings
                    .push(format!("skipped local cleanup: {why}"));
                return Some(report);
            }
        }
    };
    if let Err(why) =
        verify_already_merged_ancestry(&main_repo, &req.remote, &pr.base_ref_name, merge_oid)
    {
        report
            .warnings
            .push(format!("skipped local cleanup: {why}"));
        return Some(report);
    }
    cleanup_merged_pr_local_into(&main_repo, req, pr, caller_cwd, clock, &mut report);
    Some(report)
}

/// Public entry point for local fail-closed cleanup of a verified merged PR.
pub fn cleanup_merged_pr_local(
    req: &PrMergeRequest,
    pr: &PrInfo,
    caller_cwd: Option<&Path>,
    clock: &dyn Clock,
) -> PrCleanupReport {
    let main_repo = main_work_tree(&req.repo_root);
    let mut report = PrCleanupReport::default();
    cleanup_merged_pr_local_into(&main_repo, req, pr, caller_cwd, clock, &mut report);
    report
}

fn delete_remote_branch(
    main_repo: &Path,
    req: &PrMergeRequest,
    pr: &PrInfo,
    report: &mut PrCleanupReport,
) {
    if pr.is_cross_repository || is_protected_branch(&pr.head_ref_name, &pr.base_ref_name) {
        return;
    }
    if req.remote.is_empty()
        || req.remote.starts_with('-')
        || pr.head_ref_name.contains("..")
        || check_branch_name(main_repo, &pr.head_ref_name).is_err()
    {
        report.warnings.push(format!(
            "skipped remote branch delete: invalid remote {:?} or branch {:?}",
            req.remote, pr.head_ref_name
        ));
        return;
    }
    let refspec = format!(":refs/heads/{}", pr.head_ref_name);
    match git_raw(main_repo, &["push", "--", &req.remote, &refspec]) {
        Ok(out) if out.success() => report.remote_branch_deleted = true,
        Ok(out)
            if out
                .stderr
                .to_ascii_lowercase()
                .contains("remote ref does not exist") => {}
        Ok(out) => report.warnings.push(format!(
            "could not delete remote branch `{}/{}`: {}",
            req.remote,
            pr.head_ref_name,
            out.stderr.trim()
        )),
        Err(err) => report.warnings.push(format!(
            "could not delete remote branch `{}/{}`: {err}",
            req.remote, pr.head_ref_name
        )),
    }
}

fn cleanup_merged_pr_local_into(
    main_repo: &Path,
    req: &PrMergeRequest,
    pr: &PrInfo,
    caller_cwd: Option<&Path>,
    clock: &dyn Clock,
    report: &mut PrCleanupReport,
) {
    if pr.is_cross_repository || is_protected_branch(&pr.head_ref_name, &pr.base_ref_name) {
        return;
    }
    if pr.head_ref_name.contains("..") || check_branch_name(main_repo, &pr.head_ref_name).is_err() {
        report.warnings.push(format!(
            "skipped local cleanup: invalid branch name {:?}",
            pr.head_ref_name
        ));
        return;
    }

    let task_id_opt = pr
        .head_ref_name
        .strip_prefix(worktree::BRANCH_PREFIX)
        .filter(|id| worktree::validate_task_id(id).is_ok());

    if let Some(task_id) = task_id_opt {
        let db_path = req.data_dir.join("aaf.db");
        if db_path.is_file() {
            match transition_review_task_done(
                &db_path,
                main_repo,
                task_id,
                pr.number,
                &req.expect_head,
                clock.now(),
            ) {
                Ok(DbDecision::Proceed { marked_done }) => report.task_marked_done = marked_done,
                Ok(DbDecision::Stop(reason)) => {
                    report.warnings.push(reason);
                    return;
                }
                Err(err) => {
                    report.warnings.push(format!(
                        "db error for `{task_id}`; skipped local cleanup: {err}"
                    ));
                    return;
                }
            }
        }
    }

    let entries = match worktree::list(main_repo) {
        Ok(list) => list,
        Err(err) => {
            report.warnings.push(format!(
                "could not list worktrees; skipped local cleanup: {err}"
            ));
            return;
        }
    };
    if active_in_progress_branches(&entries).contains(&pr.head_ref_name) {
        report.warnings.push(format!(
            "branch `{}` has an in-progress git operation; skipped local cleanup",
            pr.head_ref_name
        ));
        return;
    }

    let main_can = fs::canonicalize(main_repo).ok();
    let mut any_skipped = false;
    for (idx, entry) in entries.iter().enumerate() {
        if entry.branch.as_deref() != Some(pr.head_ref_name.as_str()) {
            continue;
        }
        match try_remove_linked_worktree(
            main_repo,
            main_can.as_deref(),
            idx == 0,
            entry,
            &req.expect_head,
            caller_cwd,
        ) {
            Ok(removed) => report.worktree_removed |= removed,
            Err(warning) => {
                any_skipped = true;
                report.warnings.push(warning);
            }
        }
    }
    if any_skipped {
        return;
    }

    if !delete_local_branch(main_repo, &pr.head_ref_name, &req.expect_head, report) {
        return;
    }
    if let Some(task_id) = task_id_opt {
        match worktree::delete_snapshot_refs(main_repo, task_id) {
            Ok(count) => report.snapshots_deleted = count,
            Err(err) => report.warnings.push(format!(
                "could not delete snapshot refs for `{task_id}`: {err}"
            )),
        }
    }
}

fn try_remove_linked_worktree(
    main_repo: &Path,
    main_can: Option<&Path>,
    is_first: bool,
    entry: &WorktreeEntry,
    expect_head: &str,
    caller_cwd: Option<&Path>,
) -> Result<bool, String> {
    let is_main =
        is_first || main_can.is_some_and(|m| fs::canonicalize(&entry.path).is_ok_and(|p| p == m));
    if is_main {
        return Err(format!(
            "branch `{}` is checked out in the main worktree; skipped cleanup",
            entry.branch.as_deref().unwrap_or_default()
        ));
    }
    if entry.locked {
        return Err(format!(
            "worktree {} is locked; skipped cleanup",
            entry.path.display()
        ));
    }
    if !entry.path.exists() {
        if caller_cwd.is_some_and(|cwd| fs::canonicalize(cwd).is_err()) {
            return Err("could not canonicalize cwd; skipped cleanup".to_string());
        }
        worktree::prune(main_repo).map_err(|e| format!("could not prune worktree: {e}"))?;
        return Ok(false);
    }
    if let Some(cwd) = caller_cwd {
        match (fs::canonicalize(cwd), fs::canonicalize(&entry.path)) {
            (Ok(cwd_can), Ok(wt_can)) if cwd_can.starts_with(&wt_can) => {
                return Err(format!(
                    "current working directory {} is inside worktree {}; skipped cleanup",
                    cwd.display(),
                    entry.path.display()
                ));
            }
            (Ok(_), Ok(_)) => {}
            _ => {
                return Err(format!(
                    "could not canonicalize cwd or worktree {}; skipped cleanup",
                    entry.path.display()
                ));
            }
        }
    }
    if is_operation_in_progress(&entry.path) {
        return Err(format!(
            "worktree {} has an in-progress git operation; skipped cleanup",
            entry.path.display()
        ));
    }
    let head = git(&entry.path, &["rev-parse", "HEAD"])
        .map_err(|e| format!("could not read HEAD of {}: {e}", entry.path.display()))?;
    if !head.eq_ignore_ascii_case(expect_head) {
        return Err(format!(
            "worktree {} HEAD {head} does not match expected {expect_head}; skipped cleanup",
            entry.path.display()
        ));
    }
    let clean = git_raw(&entry.path, &["status", "--porcelain"])
        .is_ok_and(|out| out.success() && out.stdout.trim().is_empty());
    if !clean {
        return Err(format!(
            "worktree {} has uncommitted changes; skipped cleanup",
            entry.path.display()
        ));
    }
    let wt_str = path_str(&entry.path)
        .map_err(|e| format!("invalid worktree path {}: {e}", entry.path.display()))?;
    let out = git_raw(main_repo, &["worktree", "remove", wt_str])
        .map_err(|e| format!("could not remove worktree {}: {e}", entry.path.display()))?;
    if out.success() {
        Ok(true)
    } else {
        Err(format!(
            "could not remove worktree {}: {}",
            entry.path.display(),
            out.stderr.trim()
        ))
    }
}

/// Returns `true` when the local branch is deleted or already absent.
fn delete_local_branch(
    main_repo: &Path,
    branch: &str,
    expect_head: &str,
    report: &mut PrCleanupReport,
) -> bool {
    let branch_ref = format!("refs/heads/{branch}");
    let rev = match git_raw(
        main_repo,
        &["rev-parse", "--verify", "--quiet", &branch_ref],
    ) {
        Ok(out) => out,
        Err(err) => {
            report
                .warnings
                .push(format!("could not inspect local branch `{branch}`: {err}"));
            return false;
        }
    };
    if !rev.success() {
        return true;
    }
    let tip = rev.stdout.trim();
    if !tip.eq_ignore_ascii_case(expect_head) {
        report.warnings.push(format!(
            "local branch `{branch}` tip {tip} != expected {expect_head}; skipped branch/snapshot cleanup"
        ));
        return false;
    }
    match git_raw(main_repo, &["branch", "-D", branch]) {
        Ok(out) if out.success() => {
            report.branch_deleted = true;
            true
        }
        Ok(out) if out.stderr.to_ascii_lowercase().contains("not found") => true,
        Ok(out) => {
            report.warnings.push(format!(
                "could not delete local branch `{branch}`: {}",
                out.stderr.trim()
            ));
            false
        }
        Err(err) => {
            report
                .warnings
                .push(format!("could not delete local branch `{branch}`: {err}"));
            false
        }
    }
}

fn worktree_git_dir(wt_path: &Path) -> Option<PathBuf> {
    let raw = git(wt_path, &["rev-parse", "--absolute-git-dir"]).ok()?;
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
}

fn git_dir_has_operation(git_dir: &Path) -> bool {
    OP_MARKERS.iter().any(|m| git_dir.join(m).exists())
}

pub(crate) fn is_operation_in_progress(wt_path: &Path) -> bool {
    worktree_git_dir(wt_path).is_none_or(|dir| git_dir_has_operation(&dir))
}

fn active_in_progress_branches(entries: &[WorktreeEntry]) -> HashSet<String> {
    let mut active = HashSet::new();
    for entry in entries {
        if !entry.path.exists() {
            continue;
        }
        let Some(git_dir) = worktree_git_dir(&entry.path) else {
            continue;
        };
        for dir_name in ["rebase-merge", "rebase-apply"] {
            if let Ok(head_name) = fs::read_to_string(git_dir.join(dir_name).join("head-name"))
                && let Some(branch) = head_name.trim().strip_prefix("refs/heads/")
            {
                active.insert(branch.to_string());
            }
        }
        if git_dir_has_operation(&git_dir)
            && let Some(branch) = &entry.branch
        {
            active.insert(branch.clone());
        }
    }
    active
}
