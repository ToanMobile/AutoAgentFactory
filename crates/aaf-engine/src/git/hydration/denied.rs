//! Resolves hydrated `clone_dirs` + `copy_files` paths as sandbox `write_denied`.

use std::fs;
use std::path::{Path, PathBuf};

use super::super::base_ref::resolve_base;
use super::super::cmd::git_raw;
use super::super::{GitError, main_work_tree, path_str, worktree};
use super::{load_base_worktree_config, validate_relative_entry};
use crate::config::{self, ConfigInputs, ConfigOverlay};
use crate::confine_core::git_refs::{AUTO_FALLBACKS, origin_head_branch};

/// Resolves all `clone_dirs` + `copy_files` destinations inside `worktree` from
/// trusted base commits (union of configured base tip and merge-base with HEAD)
/// so sandboxed agent turns cannot mutate hydrated entries. Never trusts a
/// linked worktree's own `HEAD` commit.
pub fn worktree_write_denied_for_cwd(worktree: &Path) -> Result<Vec<PathBuf>, GitError> {
    let top = git_raw(worktree, &["rev-parse", "--show-toplevel"])?;
    let raw_top = top.stdout.trim();
    if !top.success() || raw_top.is_empty() {
        return Ok(Vec::new());
    }
    let wt_root = fs::canonicalize(raw_top).unwrap_or_else(|_| PathBuf::from(raw_top));
    let shas = resolve_trusted_base_shas(&wt_root)?;
    let mut denied = Vec::new();
    for sha in shas {
        let cfg = load_base_worktree_config(&wt_root, &sha)?;
        for entry in cfg.clone_dirs.iter().chain(&cfg.copy_files) {
            let full = wt_root.join(validate_relative_entry(entry)?);
            if !denied.contains(&full) {
                denied.push(full);
            }
        }
    }
    Ok(denied)
}

/// UTF-8 string variant of [`worktree_write_denied_for_cwd`] for sandbox profiles.
pub fn worktree_write_denied_strings(worktree: &Path) -> Result<Vec<String>, GitError> {
    worktree_write_denied_for_cwd(worktree)?
        .into_iter()
        .map(|p| path_str(&p).map(str::to_string))
        .collect()
}

fn push_unique(shas: &mut Vec<String>, sha: String) {
    if !sha.is_empty() && !shas.contains(&sha) {
        shas.push(sha);
    }
}

fn resolve_trusted_base_shas(wt_root: &Path) -> Result<Vec<String>, GitError> {
    let head_out = git_raw(
        wt_root,
        &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
    )?;
    let head = head_out.stdout.trim().to_string();
    if !head_out.success() || head.is_empty() {
        return Ok(Vec::new());
    }
    let main_repo = main_work_tree(wt_root);
    let main_canon = fs::canonicalize(&main_repo).unwrap_or_else(|_| main_repo.clone());
    let is_linked = main_canon != wt_root;

    let mut shas = Vec::new();
    if let Some(base_sha) = resolve_configured_base_sha(&main_canon) {
        push_unique(&mut shas, base_sha.clone());
        let mb = git_raw(wt_root, &["merge-base", &base_sha, &head])?;
        if mb.success() {
            push_unique(&mut shas, mb.stdout.trim().to_string());
        }
        return Ok(shas);
    }

    let mut candidates = Vec::new();
    let origin_head = git_raw(
        wt_root,
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
    )?;
    let sym = origin_head.stdout.trim().to_string();
    if origin_head.success() && origin_head_branch(&sym).is_some() {
        candidates.push(sym);
    }
    candidates.extend(AUTO_FALLBACKS.map(String::from));
    for target in candidates {
        let spec = format!("{target}^{{commit}}");
        let rev = git_raw(wt_root, &["rev-parse", "--verify", "--quiet", &spec])?;
        if !rev.success() {
            continue;
        }
        let tip = rev.stdout.trim().to_string();
        push_unique(&mut shas, tip.clone());
        let mb = git_raw(wt_root, &["merge-base", &tip, &head])?;
        if mb.success() {
            push_unique(&mut shas, mb.stdout.trim().to_string());
        }
        return Ok(shas);
    }
    if is_linked
        && let Ok(entries) = worktree::list(wt_root)
        && let Some(main_wt) = entries.first()
        && let Some(main_head) = &main_wt.head
    {
        push_unique(&mut shas, main_head.clone());
        let mb = git_raw(wt_root, &["merge-base", main_head, &head])?;
        if mb.success() {
            push_unique(&mut shas, mb.stdout.trim().to_string());
        }
        return Ok(shas);
    }
    if !is_linked {
        push_unique(&mut shas, head);
    }
    Ok(shas)
}

fn resolve_configured_base_sha(main_repo: &Path) -> Option<String> {
    let no_env = |_: &str| None;
    let base_ref = config::load(&ConfigInputs {
        global_config: None,
        repo_root: main_repo.to_path_buf(),
        env: &no_env,
        cli: ConfigOverlay::default(),
    })
    .ok()?
    .repo
    .gate
    .base_ref;
    let trimmed = base_ref.trim();
    if trimmed.is_empty() || trimmed == "auto" {
        return None;
    }
    let resolved = resolve_base(main_repo, trimmed).ok()?;
    let spec = format!("{}^{{commit}}", resolved.start_point());
    let out = git_raw(main_repo, &["rev-parse", "--verify", "--quiet", &spec]).ok()?;
    out.success()
        .then(|| out.stdout.trim().to_string())
        .filter(|s| !s.is_empty())
}
