//! Opt-in fail-closed cleanup of merged non-`aaf/*` branches (`audit/*`,
//! `fix/*`, `lane/*`) and their clean linked worktrees (`aaf gc --merged-branches`).
//!
//! Safety invariants:
//! - Disabled by default (`opts.merged_branches == false`).
//! - Never applies `--force` to dirty worktrees, even when `opts.force == true`.
//!   When a worktree is skipped, its branch is always kept too.
//! - Protects `main`, `master`, `develop`, `development`, `trunk`, `head`,
//!   `release/*`, `public-*`, non-ASCII branch names, `base.branch` (all
//!   case-insensitive ASCII), main worktree's `HEAD`, locked worktrees,
//!   worktrees containing `cwd` (fail-closed on canonicalize failure), and any
//!   branch/worktree undergoing a git operation (`rebase`, `merge`,
//!   `cherry-pick`, `revert`).
//! - Defends against the 0-commit trap (including `git branch -m`): refuses
//!   branches where `branch_oid == base_oid` (unless verified by a merged PR
//!   matching `headRefOid`) OR whose non-empty reflog contains only branch
//!   create/rename entries.
//! - Never runs `git fetch` or mutates disk/refs during `--dry-run`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::squash::verify_branch_squash_merges_with_warnings;
use super::{GcOptions, GcReport};
use crate::config::{ConfigInputs, ConfigOverlay, load as load_config};
use crate::factory::{BaseRef, resolve_base};
use crate::git::cmd::{git, git_raw};
use crate::git::pr_cleanup::{is_protected_branch, main_work_tree};
use crate::git::worktree::{self, BRANCH_PREFIX, WorktreeEntry};
use crate::git::{GitError, check_branch_name, path_str};
use crate::process::{CommandRunner, SystemRunner};

const ALLOWED_PREFIXES: [&str; 3] = ["audit/", "fix/", "lane/"];
const GH_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug)]
struct Candidate {
    branch: String,
    oid: String,
    existing_worktrees: Vec<PathBuf>,
    needs_prune: bool,
}

fn push_warn(report: &mut GcReport, msg: impl std::fmt::Display) {
    report.warnings.push(format!("merged-branches: {msg}"));
}

/// Runs the opt-in `--merged-branches` sweep for `repo` using the system `gh`
/// runner. Does nothing unless `opts.merged_branches` is `true`.
pub(super) fn sweep_merged_branches(repo: &Path, opts: &GcOptions, report: &mut GcReport) {
    if !opts.merged_branches {
        return;
    }
    let runner = SystemRunner {
        timeout: GH_TIMEOUT,
    };
    sweep_merged_branches_with_runner(repo, opts, &runner, report);
}

pub(crate) fn sweep_merged_branches_with_runner(
    repo: &Path,
    opts: &GcOptions,
    runner: &dyn CommandRunner,
    report: &mut GcReport,
) {
    if !opts.merged_branches {
        return;
    }
    let main_repo = main_work_tree(repo);
    let (base, base_start, base_oid) = match resolve_sweep_base(&main_repo, opts) {
        Ok(resolved) => resolved,
        Err(why) => {
            push_warn(report, why);
            return;
        }
    };
    report.merged_branches_base = Some(base_start.clone());

    let registered = match worktree::list(&main_repo) {
        Ok(list) => list,
        Err(err) => return push_warn(report, format!("could not list worktrees: {err}")),
    };
    let active_ops = match worktree::active_in_progress_branches(&main_repo) {
        Ok(active) => active,
        Err(err) => {
            return push_warn(
                report,
                format!("could not inspect active git operations: {err}"),
            );
        }
    };
    let listed = match list_prefixed_branches(&main_repo) {
        Ok(items) => items,
        Err(err) => return push_warn(report, format!("could not list branches: {err}")),
    };

    let main_can = fs::canonicalize(&main_repo).ok();
    let main_head_branch = registered.first().and_then(|e| e.branch.as_deref());
    let cwd_can = opts
        .cwd
        .as_deref()
        .map(|cwd| fs::canonicalize(cwd).map_err(|e| format!("{}: {e}", cwd.display())));

    let mut ready = Vec::new();
    let mut needs_gh = Vec::new();

    for (branch, oid) in listed {
        if !has_allowed_prefix(&branch)
            || branch
                .to_ascii_lowercase()
                .starts_with(&BRANCH_PREFIX.to_ascii_lowercase())
            || is_protected_branch(&branch, &base.branch)
            || main_head_branch.is_some_and(|m| m.eq_ignore_ascii_case(&branch))
        {
            continue;
        }
        if branch.contains("..") || check_branch_name(&main_repo, &branch).is_err() {
            push_warn(report, format!("skipped invalid branch name `{branch}`"));
            continue;
        }
        if active_ops.iter().any(|b| b.eq_ignore_ascii_case(&branch)) {
            push_warn(
                report,
                format!("skipped `{branch}` (git operation in progress)"),
            );
            continue;
        }
        if is_zero_commit_reflog(&main_repo, &branch) {
            push_warn(
                report,
                format!("skipped `{branch}` (0-commit reflog: created/renamed only)"),
            );
            continue;
        }
        let (existing_worktrees, needs_prune) = match inspect_branch_worktrees(
            &branch,
            &oid,
            &registered,
            main_can.as_deref(),
            cwd_can.as_ref(),
        ) {
            Ok(info) => info,
            Err(why) => {
                push_warn(report, why);
                continue;
            }
        };

        let is_ancestor = git_raw(
            &main_repo,
            &["merge-base", "--is-ancestor", &oid, &base_oid],
        )
        .is_ok_and(|out| out.code == Some(0));
        let candidate = Candidate {
            branch,
            oid: oid.clone(),
            existing_worktrees,
            needs_prune,
        };
        if is_ancestor && !oid.eq_ignore_ascii_case(&base_oid) {
            ready.push(candidate);
        } else {
            needs_gh.push(candidate);
        }
    }

    if !needs_gh.is_empty()
        && let Some(gh_base) = real_branch_for_gh(&main_repo, &base)
    {
        let verified = verify_squash_candidates(
            &main_repo,
            &base,
            gh_base,
            &base_start,
            &needs_gh,
            opts.dry_run,
            runner,
            report,
        );
        for candidate in needs_gh {
            if verified
                .get(&candidate.branch)
                .is_some_and(|oid| oid.eq_ignore_ascii_case(&candidate.oid))
            {
                ready.push(candidate);
            }
        }
    }

    for candidate in ready {
        apply_cleanup(&main_repo, &candidate, opts.dry_run, report);
    }
}

fn resolve_sweep_base(
    main_repo: &Path,
    opts: &GcOptions,
) -> Result<(BaseRef, String, String), String> {
    let base = if let Some(explicit) = &opts.base {
        let trimmed = explicit.trim();
        if trimmed.is_empty() {
            return Err("--base cannot be empty; pass --base <ref>".to_string());
        }
        resolve_base(main_repo, trimmed)
            .map_err(|e| format!("could not resolve --base {trimmed:?} ({e}); pass --base <ref>"))?
    } else {
        let configured = configured_base_ref(main_repo);
        resolve_base(main_repo, &configured)
            .map_err(|e| format!("could not resolve base ref ({e}); pass --base <ref>"))?
    };

    let base_start = base.start_point();
    let spec = format!("{base_start}^{{commit}}");
    let out = git_raw(main_repo, &["rev-parse", "--verify", "--quiet", &spec]).map_err(|e| {
        format!("could not resolve base ref {base_start:?} ({e}); pass --base <ref>")
    })?;
    let oid = out.stdout.trim();
    if !out.success() || oid.is_empty() {
        return Err(format!(
            "base ref {base_start:?} does not resolve to a commit ({}); pass --base <ref>",
            out.stderr.trim()
        ));
    }
    Ok((base, base_start, oid.to_string()))
}

fn configured_base_ref(main_repo: &Path) -> String {
    let env = |_: &str| None;
    let inputs = ConfigInputs {
        global_config: None,
        repo_root: main_repo.to_path_buf(),
        env: &env,
        cli: ConfigOverlay::default(),
    };
    load_config(&inputs)
        .map(|cfg| cfg.repo.gate.base_ref)
        .unwrap_or_else(|_| "auto".to_string())
}

fn is_full_oid(name: &str) -> bool {
    matches!(name.len(), 40 | 64) && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Returns the short branch name for `gh pr list --base` ONLY when `base`
/// resolved to a real branch (not a tag or raw commit SHA).
fn real_branch_for_gh<'a>(main_repo: &Path, base: &'a BaseRef) -> Option<&'a str> {
    if base.remote.is_some() {
        return Some(base.branch.as_str());
    }
    if base.branch.is_empty()
        || base.branch == "HEAD"
        || base.branch.starts_with("refs/")
        || is_full_oid(&base.branch)
    {
        return None;
    }
    let full = format!("refs/heads/{}", base.branch);
    let exists = git_raw(main_repo, &["show-ref", "--verify", "--quiet", &full])
        .is_ok_and(|out| out.success());
    exists.then_some(base.branch.as_str())
}

fn has_allowed_prefix(branch: &str) -> bool {
    ALLOWED_PREFIXES
        .iter()
        .any(|prefix| branch.starts_with(prefix) && branch.len() > prefix.len())
}

fn list_prefixed_branches(main_repo: &Path) -> Result<Vec<(String, String)>, GitError> {
    let out = git(
        main_repo,
        &[
            "for-each-ref",
            "--format=%(refname)%09%(objectname)",
            "refs/heads/audit",
            "refs/heads/fix",
            "refs/heads/lane",
        ],
    )?;
    let mut rows = Vec::new();
    for line in out.lines().filter(|l| !l.trim().is_empty()) {
        if let Some((full_ref, oid)) = line.split_once('\t')
            && let Some(branch) = full_ref.strip_prefix("refs/heads/")
            && !branch.is_empty()
            && !oid.trim().is_empty()
        {
            rows.push((branch.to_string(), oid.trim().to_string()));
        }
    }
    Ok(rows)
}

fn is_zero_commit_reflog_line(subject: &str) -> bool {
    let lower = subject.trim().to_ascii_lowercase();
    lower.starts_with("branch: created") || lower.starts_with("branch: renamed")
}

/// True when `git reflog show --format=%gs refs/heads/<branch>` is non-empty
/// and EVERY line is purely a branch creation or rename (zero commit/merge/
/// rebase/cherry-pick/reset/pull records).
fn is_zero_commit_reflog(main_repo: &Path, branch: &str) -> bool {
    let branch_ref = format!("refs/heads/{branch}");
    let Ok(out) = git_raw(main_repo, &["reflog", "show", "--format=%gs", &branch_ref]) else {
        return false;
    };
    if !out.success() {
        return false;
    }
    let lines: Vec<&str> = out
        .stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    !lines.is_empty() && lines.iter().all(|line| is_zero_commit_reflog_line(line))
}

fn inspect_branch_worktrees(
    branch: &str,
    branch_oid: &str,
    registered: &[WorktreeEntry],
    main_can: Option<&Path>,
    cwd_can: Option<&Result<PathBuf, String>>,
) -> Result<(Vec<PathBuf>, bool), String> {
    let mut existing = Vec::new();
    let mut needs_prune = false;

    for (idx, entry) in registered.iter().enumerate() {
        if !entry
            .branch
            .as_deref()
            .is_some_and(|b| b.eq_ignore_ascii_case(branch))
        {
            continue;
        }
        let wt_disp = entry.path.display();
        let is_main = idx == 0
            || main_can.is_some_and(|m| fs::canonicalize(&entry.path).is_ok_and(|p| p == m));
        if is_main {
            return Err(format!("skipped `{branch}` (checked out in main worktree)"));
        }
        if entry.locked {
            return Err(format!(
                "skipped locked worktree {wt_disp} (branch `{branch}` kept)"
            ));
        }
        if !entry.path.exists() {
            if let Some(Err(why)) = cwd_can {
                return Err(format!(
                    "could not canonicalize cwd ({why}); skipped `{branch}`"
                ));
            }
            needs_prune = true;
            continue;
        }
        if let Some(cwd_res) = cwd_can {
            let cwd_path = cwd_res
                .as_ref()
                .map_err(|why| format!("could not canonicalize cwd ({why}); skipped `{branch}`"))?;
            let wt_can = fs::canonicalize(&entry.path).map_err(|err| {
                format!("could not canonicalize worktree {wt_disp} ({err}); skipped `{branch}`")
            })?;
            if cwd_path.starts_with(&wt_can) {
                return Err(format!(
                    "skipped worktree {wt_disp} containing cwd (branch `{branch}` kept)"
                ));
            }
        }
        if worktree::is_operation_in_progress(&entry.path).unwrap_or(true) {
            return Err(format!(
                "skipped worktree {wt_disp} (git operation in progress or unreadable; branch `{branch}` kept)"
            ));
        }
        let clean = git_raw(&entry.path, &["status", "--porcelain"])
            .is_ok_and(|out| out.success() && out.stdout.trim().is_empty());
        if !clean {
            return Err(format!(
                "skipped dirty worktree {wt_disp} (branch `{branch}` kept; --force never applies)"
            ));
        }
        match worktree::removal_refusal(&entry.path) {
            Ok(None) => {}
            Ok(Some(reason)) => {
                return Err(format!(
                    "skipped worktree {wt_disp} ({reason}; branch `{branch}` kept)"
                ));
            }
            Err(err) => {
                return Err(format!(
                    "could not inspect worktree {wt_disp} ({err}); skipped `{branch}`"
                ));
            }
        }
        let wt_head = git(&entry.path, &["rev-parse", "HEAD"])
            .map_err(|e| format!("could not read HEAD of {wt_disp} ({e}); skipped `{branch}`"))?;
        if !wt_head.eq_ignore_ascii_case(branch_oid) {
            return Err(format!(
                "worktree {wt_disp} HEAD {wt_head} != branch `{branch}` tip {branch_oid}; skipped"
            ));
        }
        existing.push(entry.path.clone());
    }

    Ok((existing, needs_prune))
}

#[allow(clippy::too_many_arguments)]
fn verify_squash_candidates(
    main_repo: &Path,
    base: &BaseRef,
    gh_base: &str,
    base_start: &str,
    candidates: &[Candidate],
    dry_run: bool,
    runner: &dyn CommandRunner,
    report: &mut GcReport,
) -> HashMap<String, String> {
    let pairs: Vec<(String, String)> = candidates
        .iter()
        .map(|c| (c.branch.clone(), c.oid.clone()))
        .collect();
    let fetch_base = || -> Result<(), GitError> {
        if dry_run {
            return Ok(());
        }
        let Some(remote) = &base.remote else {
            return Ok(());
        };
        if remote.is_empty() || remote.starts_with('-') {
            return Err(GitError::Invalid(format!("remote {remote:?}")));
        }
        let refspec = format!(
            "+refs/heads/{}:refs/remotes/{remote}/{}",
            base.branch, base.branch
        );
        git(main_repo, &["fetch", "--quiet", "--", remote, &refspec]).map(|_| ())
    };
    let mut on_warning = |msg: String| push_warn(report, msg);
    match verify_branch_squash_merges_with_warnings(
        main_repo,
        gh_base,
        base_start,
        &pairs,
        &fetch_base,
        runner,
        &mut on_warning,
    ) {
        Ok(map) => map,
        Err(err) => {
            push_warn(report, format!("squash verification failed: {err}"));
            HashMap::new()
        }
    }
}

fn remove_worktree_safe(main_repo: &Path, wt: &Path, branch: &str) -> Result<(), String> {
    let d = wt.display();
    let wt_str = path_str(wt).map_err(|e| format!("invalid path {d} ({e}); `{branch}` kept"))?;
    // Never pass `--force` here: dirty/locked/submodule trees must fail closed.
    let out = git_raw(main_repo, &["worktree", "remove", wt_str])
        .map_err(|e| format!("remove worktree {d} ({e}); `{branch}` kept"))?;
    if out.success() {
        Ok(())
    } else {
        let stderr = out.stderr.trim();
        Err(format!("remove worktree {d} ({stderr}); `{branch}` kept"))
    }
}

fn apply_cleanup(main_repo: &Path, candidate: &Candidate, dry_run: bool, report: &mut GcReport) {
    let branch = &candidate.branch;
    if dry_run {
        report
            .merged_worktrees_removed
            .extend(candidate.existing_worktrees.iter().cloned());
        report.merged_branches_deleted.push(branch.clone());
        return;
    }
    for wt in &candidate.existing_worktrees {
        if let Err(why) = remove_worktree_safe(main_repo, wt, branch) {
            return push_warn(report, why);
        }
        report.merged_worktrees_removed.push(wt.clone());
    }
    if candidate.needs_prune
        && let Err(e) = worktree::prune(main_repo)
    {
        return push_warn(report, format!("prune for `{branch}` ({e}); branch kept"));
    }
    let branch_ref = format!("refs/heads/{branch}");
    match git_raw(
        main_repo,
        &["update-ref", "-d", &branch_ref, &candidate.oid],
    ) {
        Ok(out) if out.success() => report.merged_branches_deleted.push(branch.clone()),
        Ok(out) => {
            let still_exists = git_raw(
                main_repo,
                &["rev-parse", "--verify", "--quiet", &branch_ref],
            )
            .is_ok_and(|rev| rev.success());
            if still_exists {
                push_warn(report, format!("delete `{branch}`: {}", out.stderr.trim()));
            }
        }
        Err(e) => push_warn(report, format!("delete `{branch}`: {e}")),
    }
}
