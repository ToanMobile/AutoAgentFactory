//! Trusted base-SHA SOP loading, repository run lock, and isolated worktree
//! management for `aaf sop run` (`ORCH-03`, `V2-SEC-12`).

use std::collections::HashSet;
use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};

use aaf_domain::sop::{SopCheckpoint, SopSpec, StageKind};
use aaf_engine::db::private;
use aaf_engine::factory::resolve_base;
use aaf_engine::gate::{GIT_TIMEOUT, base};
use aaf_engine::git::worktree::{self, TaskWorktree};
use aaf_engine::sop::builtin::find_builtin;
use aaf_engine::sop::{DiscoveredSop, REPO_SOPS_REL, SopSource};

use crate::ctx::Ctx;
use crate::output::CmdError;

const MAX_SOP_TOML_BYTES: usize = 512 * 1024;

/// Resolves an SOP by name strictly from `git show <base_sha>:.autofactory/sops/*.toml`
/// (never reading uncommitted or dirty working-tree files), then falls back to
/// built-in blueprints.
pub(super) fn resolve_trusted_sop(
    repo_root: Option<&Path>,
    base_ref: &str,
    name: &str,
) -> Result<(DiscoveredSop, Option<String>), CmdError> {
    if name.trim().is_empty() {
        return Err(CmdError::new("SOP name must not be empty"));
    }
    let mut resolved_base_sha = None;
    if let Some(root) = repo_root {
        let base_info = base::resolve(root, base_ref, GIT_TIMEOUT)
            .map_err(|e| CmdError::new(format!("resolve base branch: {e}")))?;
        resolved_base_sha = base_info.sha.clone();
        if let Some(base_sha) = base_info.sha.as_deref()
            && let Some(found) = load_sop_from_base_sha(root, base_sha, name)?
        {
            return Ok((found, resolved_base_sha));
        }
    }
    if let Some(spec) = find_builtin(name) {
        spec.validate()
            .map_err(|e| CmdError::new(format!("invalid built-in SOP {name:?}: {e}")))?;
        return Ok((
            DiscoveredSop {
                spec,
                source: SopSource::Builtin,
            },
            resolved_base_sha,
        ));
    }
    Err(CmdError::new(format!(
        "not found: SOP {name:?} (custom repo SOPs must be committed on the base branch under {REPO_SOPS_REL}/)"
    )))
}

fn load_sop_from_base_sha(
    repo_root: &Path,
    base_sha: &str,
    target_name: &str,
) -> Result<Option<DiscoveredSop>, CmdError> {
    let dir_arg = format!("{REPO_SOPS_REL}/");
    let listed = aaf_fp::git(
        repo_root,
        &["ls-tree", base_sha, "--", &dir_arg],
        &[],
        GIT_TIMEOUT,
    )
    .map_err(|e| CmdError::new(format!("list {REPO_SOPS_REL} at {base_sha}: {e}")))?;

    let mut paths: Vec<String> = Vec::new();
    for line in listed.lines().filter(|l| !l.trim().is_empty()) {
        let Some((meta, rel_path)) = line.split_once('\t') else {
            continue;
        };
        if !rel_path.ends_with(".toml")
            || !rel_path.starts_with(&dir_arg)
            || rel_path.contains("..")
        {
            continue;
        }
        let mut parts = meta.split_whitespace();
        let mode = parts.next().unwrap_or("");
        let kind = parts.next().unwrap_or("");
        // Reject git symlinks (`120000`) and non-blobs.
        if kind != "blob" || !matches!(mode, "100644" | "100755") {
            return Err(CmdError::new(format!(
                "unsafe SOP entry {rel_path:?} at {base_sha} (mode={mode}, type={kind})"
            )));
        }
        paths.push(rel_path.to_string());
    }
    paths.sort();

    let mut seen_names = HashSet::with_capacity(paths.len());
    let mut matched: Option<DiscoveredSop> = None;
    for rel_path in paths {
        let spec_ref = format!("{base_sha}:{rel_path}");
        let raw = aaf_fp::git(repo_root, &["show", &spec_ref], &[], GIT_TIMEOUT)
            .map_err(|e| CmdError::new(format!("git show {spec_ref}: {e}")))?;
        if raw.len() > MAX_SOP_TOML_BYTES {
            return Err(CmdError::new(format!(
                "SOP file {spec_ref} exceeds {MAX_SOP_TOML_BYTES} bytes"
            )));
        }
        let spec = aaf_engine::sop::parse_sop_toml(&raw)
            .map_err(|e| CmdError::new(format!("SOP {spec_ref}: {e}")))?;
        if !seen_names.insert(spec.name.clone()) {
            return Err(CmdError::new(format!(
                "duplicate SOP {:?} declared in {spec_ref}",
                spec.name
            )));
        }
        if spec.name == target_name {
            matched = Some(DiscoveredSop {
                spec,
                source: SopSource::Repo(PathBuf::from(spec_ref)),
            });
        }
    }
    Ok(matched)
}

/// Acquires the repository-wide `aaf run` lock so an SOP run never races with
/// `aaf run` or another `aaf sop run` on the same repository.
pub(super) fn acquire_sop_run_lock(ctx: &Ctx, repo_root: &Path) -> Result<Option<File>, CmdError> {
    let dir = ctx.data_dir.join("locks");
    private::create_dir(&dir).map_err(|e| CmdError::new(format!("locks dir: {e}")))?;
    let path = dir.join(format!("{}.run.lock", aaf_fp::repo_id(repo_root)));
    let file =
        private::open_file(&path).map_err(|e| CmdError::new(format!("{}: {e}", path.display())))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(CmdError::new(format!("{}: {e}", path.display()))),
    }
}

/// Returns `true` when any stage in `spec` executes scripts, agents (even
/// `read_only`), or gates (`StageKind::Script`, `StageKind::Agent`, or
/// `StageKind::Gate`), guaranteeing Zero-Touch `main` isolation in a dedicated
/// `aaf/sop-*` worktree. Pure `StageKind::HumanGate` / empty SOPs do not need a
/// worktree.
pub(super) fn spec_needs_worktree(spec: &SopSpec) -> bool {
    spec.stages.iter().any(|stage| {
        matches!(
            stage.kind,
            StageKind::Script { .. } | StageKind::Agent { .. } | StageKind::Gate { .. }
        )
    })
}

/// Computes a `worktree::validate_task_id`-compliant task id with `sop-` prefix
/// so the worktree branch is always `aaf/sop-<...>`.
pub(super) fn sop_worktree_task_id(run_id: &str) -> String {
    if run_id.starts_with("sop-") && worktree::validate_task_id(run_id).is_ok() {
        return run_id.to_string();
    }
    let prefixed = format!("sop-{run_id}");
    if worktree::validate_task_id(&prefixed).is_ok() {
        return prefixed;
    }
    format!("sop-{}", &aaf_fp::sha256_hex(run_id.as_bytes())[..16])
}

/// Reuses an existing registered worktree for `cp.run_id` or creates a fresh
/// `aaf/sop-<...>` worktree from the resolved base branch, updating
/// `cp.worktree_path` and `cp.branch`.
pub(super) fn ensure_sop_worktree(
    ctx: &Ctx,
    repo_root: &Path,
    base_ref: &str,
    cp: &mut SopCheckpoint,
) -> Result<TaskWorktree, CmdError> {
    let wt_task_id = sop_worktree_task_id(&cp.run_id);
    if let Some((wt, _head)) =
        worktree::existing_registered(repo_root, &ctx.worktrees_dir, &wt_task_id)
            .map_err(|e| CmdError::new(format!("inspect SOP worktree: {e}")))?
    {
        cp.worktree_path = Some(wt.path.to_string_lossy().into_owned());
        cp.branch = Some(wt.branch.clone());
        return Ok(wt);
    }

    let base_info = resolve_base(repo_root, base_ref)
        .map_err(|e| CmdError::new(format!("base branch: {e}")))?;
    let start_point = base_info.start_point();
    let wt = worktree::create(repo_root, &ctx.worktrees_dir, &wt_task_id, &start_point)
        .map_err(|e| CmdError::new(format!("create SOP worktree: {e}")))?;
    cp.worktree_path = Some(wt.path.to_string_lossy().into_owned());
    cp.branch = Some(wt.branch.clone());
    Ok(wt)
}
