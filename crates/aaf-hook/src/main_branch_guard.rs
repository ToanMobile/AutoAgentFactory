//! Interactive Zero-Touch `main` guard (`main_branch_guard`).
//!
//! Protects `aaf`-managed primary checkouts on `main`/`master`/`develop` from
//! direct interactive `FileWrite` edits and `git commit` calls while keeping
//! linked worktrees (`aaf/<id>`), feature branches, headless worker runs
//! (`AAF_RUN_ID`), and explicit maintenance break-glass (`AAF_ALLOW_MAIN_EDIT=1`)
//! completely unblocked.

use std::fs;
use std::path::{Path, PathBuf};

use crate::shell::Cmd;
use crate::{Ctx, Denial, base_ref, git_guard, paths};

pub const RULE: &str = "main_branch_guard";
pub const FIX: &str = "create an isolated git worktree or use aaf run; set AAF_ALLOW_MAIN_EDIT=1 \
                       only for deliberate host maintenance";

fn allow_main_edit() -> bool {
    std::env::var_os("AAF_ALLOW_MAIN_EDIT").is_some_and(|val| val == "1")
}

fn is_active(ctx: &Ctx) -> bool {
    !ctx.env.in_factory_run && !allow_main_edit()
}

/// Returns the protected primary branch name (`"main"`, `"master"`, `"develop"`,
/// or `[gate].base_ref` configured in `.autofactory/config.toml`) when `root`
/// is the primary checkout of an `aaf`-managed repository. Uses only direct
/// filesystem reads (no `git` subprocess).
pub(crate) fn protected_primary_branch(root: &Path) -> Option<String> {
    if !root.join(".autofactory/config.toml").is_file() {
        return None;
    }
    let dot_git = root.join(".git");
    if !fs::symlink_metadata(&dot_git).is_ok_and(|meta| meta.is_dir()) {
        return None;
    }
    let head = fs::read_to_string(dot_git.join("HEAD")).ok()?;
    let branch = head.trim().strip_prefix("ref: refs/heads/")?;
    if matches!(branch, "main" | "master" | "develop") {
        return Some(branch.to_string());
    }
    if let Some(configured) = base_ref::configured_base_ref(root)
        && branch.eq_ignore_ascii_case(&configured)
    {
        return Some(branch.to_string());
    }
    None
}

fn deny(branch: &str) -> Denial {
    Denial::new(
        RULE,
        format!(
            "direct edits on protected primary branch '{branch}' are blocked in aaf-managed repos"
        ),
        FIX,
    )
}

fn is_inside_root(path: &Path, root: &Path, ctx: &Ctx) -> bool {
    if paths::is_inside(path, root) {
        return true;
    }
    let real_path = ctx.real_of(path);
    paths::is_inside(&real_path, root)
}

fn nearest_existing_ancestor(mut probe: &Path) -> &Path {
    while fs::symlink_metadata(probe).is_err() {
        let Some(parent) = probe.parent() else {
            break;
        };
        if parent == probe || parent.as_os_str().is_empty() {
            break;
        }
        probe = parent;
    }
    probe
}

fn target_repo_root(path: &Path, ctx: &Ctx) -> Option<PathBuf> {
    if let Some(root) = ctx.root()
        && is_inside_root(path, &root, ctx)
    {
        return Some(root);
    }
    if let Some(root) = ctx.workspace_root()
        && is_inside_root(path, &root, ctx)
    {
        return Some(root);
    }
    let raw_probe = path.parent().unwrap_or(path);
    if !raw_probe.is_absolute() {
        return None;
    }
    let probe = nearest_existing_ancestor(raw_probe);
    if probe.is_absolute()
        && let Some(root) = (ctx.env.repo_root)(probe)
        && is_inside_root(path, &root, ctx)
    {
        return Some(root);
    }
    None
}

/// Denies interactive file writes inside a protected primary checkout.
pub fn check_file(path: &Path, ctx: &Ctx) -> Option<Denial> {
    if !is_active(ctx) {
        return None;
    }
    let root = target_repo_root(path, ctx)?;
    let branch = protected_primary_branch(&root)?;
    Some(deny(&branch))
}

/// Denies interactive `git commit` targeting a protected primary checkout.
pub fn check_cmd(cmd: &Cmd, ctx: &Ctx) -> Option<Denial> {
    if !is_active(ctx) || cmd.dynamic_name || cmd.name() != "git" {
        return None;
    }
    let call = git_guard::split(cmd.args());
    if call.sub != "commit" {
        return None;
    }
    let target_dir = call.resolve_dir(ctx);
    if let Some(raw_git_dir) = call.explicit_git_dir() {
        let git_dir = paths::expand_real_dotdot(raw_git_dir, &target_dir, ctx.env.home.as_deref());
        if let Some(parent) = git_dir.parent()
            && git_dir.file_name().is_some_and(|name| name == ".git")
            && let Some(branch) = protected_primary_branch(parent)
        {
            return Some(deny(&branch));
        }
        return None;
    }
    let root = (ctx.env.repo_root)(&target_dir)?;
    let branch = protected_primary_branch(&root)?;
    Some(deny(&branch))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io;
    use std::path::Path;

    use super::nearest_existing_ancestor;

    #[test]
    fn nearest_existing_ancestor_climbs_missing_subdirs_and_stops_at_root() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let existing = dir.path().join("primary/notes");
        fs::create_dir_all(&existing)?;

        // Existing directory returns itself.
        assert_eq!(nearest_existing_ancestor(&existing), existing.as_path());

        // Missing nested descendants climb back to the nearest existing ancestor.
        let missing = existing.join("2026-11-27/sub/deep");
        assert_eq!(nearest_existing_ancestor(&missing), existing.as_path());

        // Filesystem root terminates cleanly.
        let root = Path::new("/");
        assert_eq!(nearest_existing_ancestor(root), root);

        Ok(())
    }
}
