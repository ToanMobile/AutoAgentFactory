//! Submodules of a new task worktree: those checked out in the main checkout
//! are checked out in the task worktree too, so the agent and the gate build
//! what the developer builds. Best effort: a failure leaves the worktree as
//! `git worktree add` made it and is only a warning.

use std::fs;
use std::path::{Component, Path};
use std::time::Duration;

use super::super::cmd::{git, git_timed};

/// Budget for cloning and checking out every submodule of one worktree.
const SUBMODULE_TIMEOUT: Duration = Duration::from_secs(600);

/// Gitlink paths of `tree`'s index.
fn gitlinks(tree: &Path) -> Vec<String> {
    let Ok(index) = git(tree, &["ls-files", "--stage", "-z"]) else {
        return Vec::new();
    };
    index
        .split('\0')
        .filter(|entry| entry.starts_with("160000 "))
        .filter_map(|entry| entry.split_once('\t').map(|(_, path)| path.to_string()))
        .filter(|path| {
            !path.is_empty()
                && Path::new(path)
                    .components()
                    .all(|c| matches!(c, Component::Normal(_)))
        })
        .collect()
}

/// Checks out in `worktree` the submodules the main checkout `repo_root`
/// has checked out. `Some(warning)` when that failed; the worktree is then
/// put back as it was before.
pub(super) fn init(repo_root: &Path, worktree: &Path) -> Option<String> {
    let wanted: Vec<String> = gitlinks(repo_root)
        .into_iter()
        .filter(|path| repo_root.join(path).join(".git").exists())
        .collect();
    if wanted.is_empty() {
        return None;
    }
    let present = gitlinks(worktree);
    let paths: Vec<&str> = wanted
        .iter()
        .filter(|path| present.contains(path))
        .map(String::as_str)
        .collect();
    if paths.is_empty() {
        return None;
    }
    let mut args = vec![
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "protocol.ext.allow=never",
        "-c",
        "submodule.alternateLocation=superproject",
        "-c",
        "submodule.alternateErrorStrategy=info",
        "submodule",
        "update",
        "--init",
        "--recursive",
        "--",
    ];
    args.extend(paths.iter().copied());
    let error = match git_timed(worktree, &args, SUBMODULE_TIMEOUT) {
        Ok(out) if out.success() => return None,
        Ok(out) => out.stderr.trim().lines().last().unwrap_or("").to_string(),
        Err(err) => err.to_string(),
    };
    roll_back(worktree, &paths);
    Some(format!(
        "{}: submodules not checked out ({error}); the task works without them",
        worktree.display()
    ))
}

/// Undoes a partial `submodule update` in a worktree nobody used yet: empty
/// submodule directories and no `<git-dir>/modules`, which would make every
/// later plain `git worktree remove` refuse.
fn roll_back(worktree: &Path, paths: &[&str]) {
    for path in paths {
        let dir = worktree.join(path);
        if fs::symlink_metadata(dir.join(".git")).is_ok() {
            let _ = fs::remove_dir_all(&dir);
            let _ = fs::create_dir_all(&dir);
        }
    }
    if let Ok(git_dir) = git(worktree, &["rev-parse", "--absolute-git-dir"]) {
        let git_dir = Path::new(git_dir.trim());
        // Only a linked worktree's own `<common>/worktrees/<name>`, never
        // the main checkout's modules.
        let linked = git_dir
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|n| n == "worktrees");
        let modules = git_dir.join("modules");
        if linked
            && fs::symlink_metadata(&modules)
                .is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
        {
            let _ = fs::remove_dir_all(modules);
        }
    }
}
