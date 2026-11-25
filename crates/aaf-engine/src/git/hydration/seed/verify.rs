//! Post-clone verification (S2.4–S2.8) for seeded dependency caches.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use super::checks::{
    SeedKind, SeedTarget, check_npm_lock_match, check_pods_lock_match, check_unity_editor_not_open,
    check_unity_state_match_timed, read_regular_file_opt,
};
use super::clone::SeedBackend;
use super::git_bytes::{git_raw_bytes, remaining_time};
use crate::git::hydration::cleanup::MAX_WALK_DEPTH;
use crate::git::hydration::device::nearest_existing_canonical;
use crate::git::hydration::stage::truncate_stderr_first_line;

fn path_matches_rel(entry_path: &str, rel_dir: &str) -> bool {
    let trimmed = entry_path.trim_end_matches('/');
    trimmed == rel_dir
        || trimmed
            .strip_prefix(rel_dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Filters a single worktree-wide [`aaf_fp::HiddenFiles`] scan down to `rel_dir` (S2.7b).
pub(super) fn check_hidden_files_for_target(
    hidden: &aaf_fp::HiddenFiles,
    rel_dir: &str,
) -> Result<(), String> {
    if let Some(bad) = hidden.flagged.iter().find(|p| path_matches_rel(p, rel_dir)) {
        return Err(format!("hidden_files_after_seed: flagged {bad}"));
    }
    if let Some(bad) = hidden
        .ignored
        .iter()
        .find(|f| path_matches_rel(&f.path, rel_dir))
    {
        return Err(format!(
            "hidden_files_after_seed: {} (by {})",
            bad.path, bad.source
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn post_clone_verify(
    repo_root: &Path,
    wt_root: &Path,
    dst: &Path,
    base_sha: &str,
    target: &SeedTarget,
    base_lock: Option<&[u8]>,
    deadline: Instant,
    backend: &dyn SeedBackend,
) -> Result<usize, String> {
    let _ = remaining_time(deadline)?;

    // S2.4 (+ S2.5 for Unity) re-check after clone.
    match target.kind {
        SeedKind::Npm => {
            let l = base_lock.ok_or_else(|| "missing_base_lock".to_string())?;
            let i = read_regular_file_opt(&dst.join(".package-lock.json"))?
                .ok_or_else(|| "cloned_missing_package_lock".to_string())?;
            check_npm_lock_match(l, &i)?;
        }
        SeedKind::CocoaPods { .. } => {
            let l = base_lock.ok_or_else(|| "missing_base_lock".to_string())?;
            let i = read_regular_file_opt(&dst.join("Manifest.lock"))?
                .ok_or_else(|| "cloned_missing_manifest_lock".to_string())?;
            check_pods_lock_match(l, &i)?;
        }
        SeedKind::Unity => {
            check_unity_state_match_timed(repo_root, base_sha, deadline)?;
            let ps_rem = remaining_time(deadline)?;
            let ps_res = backend.ps_listing(ps_rem);
            check_unity_editor_not_open(repo_root, ps_res.as_deref().map_err(String::as_str))?;
        }
    }

    // S2.6: scan cloned tree for absolute/escaping symlinks, and ensure owner write bit
    // on cloned directories so `git worktree remove --force` never hits Permission denied.
    let canon_wt = fs::canonicalize(wt_root).map_err(|e| format!("canon_wt: {e}"))?;
    let canon_dst = canon_wt.join(target.rel_dir);
    let files = scan_cloned_tree(&canon_dst, &canon_wt, deadline, 0)?;

    // S2.7a: ensure git sees zero unignored untracked files under `rel_dir` (blocks F10 negations).
    let out = git_raw_bytes(
        wt_root,
        &[
            "-c",
            "core.excludesfile=/dev/null",
            "ls-files",
            "--others",
            "--exclude-standard",
            "--",
            target.rel_dir,
        ],
        deadline,
    )?;
    if !out.success() {
        return Err(format!(
            "post_clone_ls_files_failed (exit {:?}): {}",
            out.code,
            truncate_stderr_first_line(&out.stderr)
        ));
    }
    if !String::from_utf8_lossy(&out.stdout).trim().is_empty() {
        return Err("unignored_files_after_seed".to_string());
    }

    let _ = remaining_time(deadline)?;
    Ok(files)
}

fn ensure_dir_owner_writable(dir: &Path, meta: &fs::Metadata) {
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        if (mode & 0o200) == 0 {
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(mode | 0o700));
        }
    }
    #[cfg(not(unix))]
    {
        let mut perms = meta.permissions();
        if perms.readonly() {
            perms.set_readonly(false);
            let _ = fs::set_permissions(dir, perms);
        }
    }
}

fn lexical_join(base: &Path, rel: &Path) -> PathBuf {
    let mut out = base.to_path_buf();
    for comp in rel.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn verify_symlink_safe(path: &Path, canon_wt: &Path) -> Result<(), String> {
    let target = fs::read_link(path).map_err(|e| format!("scan_readlink: {e}"))?;
    if target.is_absolute() {
        return Err(format!("escaping_absolute_symlink: {}", path.display()));
    }
    let parent = path.parent().unwrap_or(canon_wt);
    let lexical = lexical_join(parent, &target);
    if !lexical.starts_with(canon_wt) {
        return Err(format!("escaping_relative_symlink: {}", path.display()));
    }
    match fs::canonicalize(path) {
        Ok(canon_target) => {
            if !canon_target.starts_with(canon_wt) {
                return Err(format!("escaping_symlink_target: {}", path.display()));
            }
        }
        Err(_) => {
            if target
                .components()
                .any(|c| matches!(c, Component::ParentDir))
            {
                return Err(format!("escaping_dangling_symlink: {}", path.display()));
            }
            let joined = parent.join(&target);
            match nearest_existing_canonical(&joined) {
                Ok(anc) if anc.starts_with(canon_wt) => {}
                _ => return Err(format!("escaping_dangling_symlink: {}", path.display())),
            }
        }
    }
    Ok(())
}

fn scan_cloned_tree(
    dir: &Path,
    canon_wt: &Path,
    deadline: Instant,
    depth: usize,
) -> Result<usize, String> {
    if depth > MAX_WALK_DEPTH {
        return Err("seed_scan_max_depth".to_string());
    }
    let _ = remaining_time(deadline)?;
    let dir_meta = fs::symlink_metadata(dir).map_err(|e| format!("scan_stat_dir: {e}"))?;
    if dir_meta.file_type().is_symlink() {
        return Err(format!("escaping_symlink_dir: {}", dir.display()));
    }
    ensure_dir_owner_writable(dir, &dir_meta);

    let rd = fs::read_dir(dir).map_err(|e| format!("scan_read_dir: {e}"))?;
    let mut count = 0usize;
    for entry in rd {
        let entry = entry.map_err(|e| format!("scan_entry: {e}"))?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path).map_err(|e| format!("scan_stat: {e}"))?;
        if meta.file_type().is_symlink() {
            verify_symlink_safe(&path, canon_wt)?;
            count += 1;
        } else if meta.is_dir() {
            count += scan_cloned_tree(&path, canon_wt, deadline, depth + 1)?;
        } else {
            count += 1;
        }
    }
    Ok(count)
}
