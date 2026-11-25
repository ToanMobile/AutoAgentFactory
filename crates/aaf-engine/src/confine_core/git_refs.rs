//! Pure git ref classification and common-repository root/id helpers shared
//! across `git`, `gate`, `gc`, and `pr_ops`.

use std::path::{Path, PathBuf};

use super::GIT_TIMEOUT;

/// Trunks `auto` tries after `origin/HEAD`, as full ref names: a short
/// name would let a tag or local branch called `origin/main` stand in.
pub(crate) const AUTO_FALLBACKS: [&str; 4] = [
    "refs/remotes/origin/main",
    "refs/remotes/origin/master",
    "refs/heads/main",
    "refs/heads/master",
];

/// The branch of `origin` the `origin/HEAD` symref names, if it may be a
/// base. A direct ref, a symref leaving `refs/remotes/origin/` or naming a
/// task branch would let the change pick its own base and empty the change
/// set.
pub(crate) fn origin_head_branch(target: &str) -> Option<&str> {
    let branch = target.strip_prefix("refs/remotes/origin/")?;
    let task = branch.starts_with(aaf_domain::engine_contracts::BRANCH_PREFIX);
    let parts_ok = branch.split('/').all(|p| !matches!(p, "" | "." | ".."));
    (parts_ok && branch != "HEAD" && !task).then_some(branch)
}

/// Parses a full remote-tracking ref (`refs/remotes/<remote>/<branch>`) into
/// `(remote, branch)`, rejecting invalid segments, `HEAD`, and task branches
/// (`aaf/*`) so an upstream ref can never empty the change set.
pub(crate) fn parse_remote_tracking_ref(full_ref: &str) -> Option<(&str, &str)> {
    let rest = full_ref.strip_prefix("refs/remotes/")?;
    let (remote, branch) = rest.split_once('/')?;
    if remote.is_empty() || remote.starts_with('-') || matches!(remote, "." | "..") {
        return None;
    }
    let task = branch.starts_with(aaf_domain::engine_contracts::BRANCH_PREFIX);
    let parts_ok = branch.split('/').all(|p| !matches!(p, "" | "." | ".."));
    (parts_ok && branch != "HEAD" && !task).then_some((remote, branch))
}

/// Resolves the primary repository root enclosing `repo_root` (across linked
/// worktrees), or `None` if git cannot determine `--git-common-dir`.
pub(crate) fn common_repo_root(repo_root: &Path) -> Option<PathBuf> {
    let args = ["rev-parse", "--path-format=absolute", "--git-common-dir"];
    let common = aaf_fp::git(repo_root, &args, &[], GIT_TIMEOUT).ok()?;
    let common = Path::new(&common);
    let main_root = match common.parent() {
        Some(parent) if common.file_name().is_some_and(|name| name == ".git") => parent,
        _ => common,
    };
    Some(main_root.to_path_buf())
}

fn stats_id_of_common_dir(common: &Path) -> String {
    let main_root = match common.parent() {
        Some(parent) if common.file_name().is_some_and(|name| name == ".git") => parent,
        _ => common,
    };
    aaf_fp::repo_id(main_root)
}

/// Key of the repository in the learned memory statistics. Every worktree
/// of one repository shares it, so a task's fresh worktree starts with the
/// measurements of its siblings instead of as "never measured".
pub fn stats_repo_id(repo_root: &Path) -> String {
    let args = ["rev-parse", "--path-format=absolute", "--git-common-dir"];
    let Ok(common) = aaf_fp::git(repo_root, &args, &[], GIT_TIMEOUT) else {
        return aaf_fp::repo_id(repo_root);
    };
    stats_id_of_common_dir(Path::new(&common))
}

/// `(aaf_fp::repo_root(dir), stats_repo_id(root))` from one git call where
/// git answers both at once; the two calls otherwise. `None` when `dir` is
/// not in a work tree git can read.
pub fn repo_root_and_stats_id(dir: &Path) -> Option<(PathBuf, String)> {
    repo_root_and_stats_id_by(
        |args| aaf_fp::git(dir, args, &[], GIT_TIMEOUT).ok(),
        || {
            let root = aaf_fp::repo_root(dir).ok()?;
            let id = stats_repo_id(&root);
            Some((root, id))
        },
    )
}

/// [`repo_root_and_stats_id`] with its combined git call and its two-call
/// fallback passed in, so a test can count which one answered.
pub(crate) fn repo_root_and_stats_id_by(
    git: impl FnOnce(&[&str]) -> Option<String>,
    fallback: impl FnOnce() -> Option<(PathBuf, String)>,
) -> Option<(PathBuf, String)> {
    let args = [
        "rev-parse",
        "--show-toplevel",
        "--path-format=absolute",
        "--git-common-dir",
    ];
    if let Some(out) = git(&args)
        && let Some((root, common)) = out.split_once('\n')
        && !root.is_empty()
        && !common.is_empty()
        && !common.contains('\n')
    {
        return Some((
            PathBuf::from(root),
            stats_id_of_common_dir(Path::new(common)),
        ));
    }
    fallback()
}
