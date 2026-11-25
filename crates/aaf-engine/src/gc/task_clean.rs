//! Safe per-task worktree, branch, and snapshot cleanup shared by `aaf gc`,
//! `gc/review.rs`, and offline `startup_sweep`.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use aaf_domain::event::{EventKind, FactoryEvent};

use super::refs::RepoRefs;
use super::{GcError, GcReport, PRUNE_EXPIRE};
use crate::config::{self, ConfigInputs, ConfigOverlay};
use crate::git::base_ref::{BaseRef, resolve_base};
use crate::git::cmd::{git, git_raw};
use crate::git::worktree::{self, BRANCH_PREFIX, WorktreeEntry};
use crate::git::{main_work_tree, path_str};

pub(super) struct ResolvedBase {
    pub(super) base: BaseRef,
    pub(super) start: String,
    pub(super) oid: String,
}

pub(super) struct TaskCleanOpts<'a> {
    pub(super) dry_run: bool,
    pub(super) force: bool,
    pub(super) require_verified_merge: bool,
    pub(super) base: Option<&'a ResolvedBase>,
    pub(super) cwd: Option<&'a Path>,
    pub(super) expected_head: Option<&'a str>,
}

fn is_valid_oid(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Extracts a verified merged commit SHA from a `MergeResult` event payload
/// (accepts both `"head_sha"` and legacy `"head_oid"`). Returns `None` for
/// failed/conflicted merges or missing/malformed SHAs.
pub(crate) fn extract_merged_head_sha(payload_json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(payload_json).ok()?;
    if value.get("merged").and_then(serde_json::Value::as_bool) == Some(false) {
        return None;
    }
    if value.get("conflict").is_some() {
        return None;
    }
    let sha = value
        .get("head_sha")
        .or_else(|| value.get("head_oid"))
        .and_then(serde_json::Value::as_str)?
        .trim();
    is_valid_oid(sha).then(|| sha.to_string())
}

/// Returns the verified merged head SHA from the newest `MergeResult` event
/// (events must be ordered newest-first). Fails closed (`None`) if the newest
/// `MergeResult` was a failure/conflict or has no valid SHA.
pub(crate) fn verified_merged_head_from_events(events: &[FactoryEvent]) -> Option<String> {
    let latest = events.iter().find(|ev| ev.kind == EventKind::MergeResult)?;
    extract_merged_head_sha(&latest.payload_json)
}

/// Resolves the repository's configured base ref once for both `Done` merge
/// verification and `Review`/orphan sweeps.
pub(super) fn resolve_repo_base(repo: &Path) -> Result<ResolvedBase, String> {
    let no_env = |_: &str| None;
    let configured = config::load(&ConfigInputs {
        global_config: None,
        repo_root: repo.to_path_buf(),
        env: &no_env,
        cli: ConfigOverlay::default(),
    })
    .map(|eff| eff.repo.gate.base_ref)
    .unwrap_or_else(|_| "auto".to_string());
    let wanted = configured.trim();
    if (wanted.is_empty() || wanted == "auto")
        && let Some(resolved) = resolve_auto_base_fast(repo)
    {
        return Ok(resolved);
    }
    let base = resolve_base(repo, &configured)
        .map_err(|err| format!("resolve_base({configured}): {err}"))?;
    let start = base.start_point();
    let spec = format!("{start}^{{commit}}");
    let oid = git(repo, &["rev-parse", "--verify", &spec])
        .map_err(|err| format!("base {start}: {err}"))?;
    Ok(ResolvedBase { base, start, oid })
}

fn resolve_auto_base_fast(repo: &Path) -> Option<ResolvedBase> {
    if let Ok(head) = git_raw(
        repo,
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
    ) && head.success()
    {
        let origin_head = head.stdout.trim();
        if let Some(branch) = crate::gate::base::origin_head_branch(origin_head) {
            let spec = format!("{origin_head}^{{commit}}");
            if let Ok(oid) = git(repo, &["rev-parse", "--verify", &spec]) {
                return Some(ResolvedBase {
                    base: BaseRef {
                        remote: Some("origin".to_string()),
                        branch: branch.to_string(),
                    },
                    start: origin_head.to_string(),
                    oid,
                });
            }
        }
    }
    let listed = git(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname)%09%(objectname)",
            "refs/remotes/origin/main",
            "refs/remotes/origin/master",
            "refs/heads/main",
            "refs/heads/master",
        ],
    )
    .ok()?;
    let mut found = std::collections::HashMap::new();
    for line in listed.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some((refname, oid)) = line.split_once('\t')
            && !oid.trim().is_empty()
        {
            found.insert(refname.to_string(), oid.trim().to_string());
        }
    }
    for name in crate::gate::base::AUTO_FALLBACKS {
        if let Some(oid) = found.remove(name) {
            let remote = name.strip_prefix("refs/remotes/origin/");
            let branch = remote.or(name.strip_prefix("refs/heads/")).unwrap_or(name);
            return Some(ResolvedBase {
                base: BaseRef {
                    remote: remote.map(|_| "origin".to_string()),
                    branch: branch.to_string(),
                },
                start: name.to_string(),
                oid,
            });
        }
    }
    None
}

fn is_ancestor_of(repo: &Path, oid: &str, base: &ResolvedBase) -> bool {
    if oid.eq_ignore_ascii_case(&base.oid) {
        return true;
    }
    git_raw(repo, &["merge-base", "--is-ancestor", oid, &base.start])
        .is_ok_and(|out| out.code == Some(0))
}

fn resolve_branch_oid(
    repo: &Path,
    refs: &RepoRefs,
    branch_ref: &str,
) -> Result<Option<String>, String> {
    if let Some(oid) = refs.branch_oid(branch_ref) {
        return Ok(Some(oid.to_string()));
    }
    let rev = git_raw(repo, &["rev-parse", "--verify", "--quiet", branch_ref])
        .map_err(|err| err.to_string())?;
    if !rev.success() {
        return Ok(None);
    }
    let oid = rev.stdout.trim();
    Ok((!oid.is_empty()).then(|| oid.to_string()))
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

    let branch_ref = format!("refs/heads/{branch}");
    let has_branch = refs.has_branch(&branch_ref);
    let snapshots = refs.snapshot_count(task_id);
    if existing_wt_path.is_none() && reg_entry.is_none() && !has_branch && snapshots == 0 {
        return Ok(());
    }

    let branch_oid = if has_branch {
        match resolve_branch_oid(repo, refs, &branch_ref) {
            Ok(oid) => oid,
            Err(err) => {
                report
                    .warnings
                    .push(format!("{task_id}: read branch tip failed: {err}"));
                return Ok(());
            }
        }
    } else {
        None
    };

    // Verify merge/head-SHA invariants BEFORE removing the worktree or branch.
    if let Some(expected_head) = clean_opts.expected_head {
        if let Some(tip) = branch_oid.as_deref()
            && !tip.eq_ignore_ascii_case(expected_head)
        {
            report.warnings.push(format!(
                "{task_id}: branch tip {tip} != verified {expected_head}: kept"
            ));
            return Ok(());
        }
    } else if clean_opts.require_verified_merge
        && !clean_opts.force
        && (branch_oid.is_some() || existing_wt_path.is_some())
    {
        let Some(base) = clean_opts.base else {
            report.warnings.push(format!(
                "{task_id}: cannot resolve base ref to verify merge: kept"
            ));
            return Ok(());
        };
        if let Some(tip) = branch_oid.as_deref()
            && !is_ancestor_of(repo, tip, base)
        {
            report.warnings.push(format!(
                "{task_id}: unmerged branch tip {tip} (not an ancestor of {}): kept",
                base.start
            ));
            return Ok(());
        }
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
        } else if clean_opts.require_verified_merge && !clean_opts.force {
            let Some(base) = clean_opts.base else {
                report.warnings.push(format!(
                    "{task_id}: cannot resolve base ref to verify merge: kept"
                ));
                return Ok(());
            };
            let actual = match git(wt_path, &["rev-parse", "HEAD"]) {
                Ok(oid) => oid,
                Err(err) => {
                    report
                        .warnings
                        .push(format!("{task_id}: read HEAD failed: {err}"));
                    return Ok(());
                }
            };
            let already_verified = branch_oid
                .as_deref()
                .is_some_and(|tip| tip.eq_ignore_ascii_case(&actual));
            if !already_verified && !is_ancestor_of(repo, &actual, base) {
                report.warnings.push(format!(
                    "{task_id}: unmerged worktree HEAD {actual} (not an ancestor of {}): kept",
                    base.start
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

    if let Some(verified_oid) = clean_opts.expected_head.or(branch_oid.as_deref())
        && has_branch
    {
        if !clean_opts.dry_run {
            let del = git_raw(repo, &["update-ref", "-d", &branch_ref, verified_oid])?;
            if !del.success() {
                let still_exists =
                    git_raw(repo, &["rev-parse", "--verify", "--quiet", &branch_ref])
                        .is_ok_and(|rev| rev.success());
                if still_exists {
                    report
                        .warnings
                        .push(format!("{task_id}: branch delete: {}", del.stderr.trim()));
                    return Ok(());
                }
            }
        }
        report.removed_branches.push(branch);
    }

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
