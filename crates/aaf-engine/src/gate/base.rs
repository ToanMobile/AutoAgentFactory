//! Base ref resolution: what the current work is compared against.

use std::path::Path;
use std::time::Duration;

use super::GateError;

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
    let task = branch.starts_with(crate::git::worktree::BRANCH_PREFIX);
    let parts_ok = branch.split('/').all(|p| !matches!(p, "" | "." | ".."));
    (parts_ok && branch != "HEAD" && !task).then_some(branch)
}

/// `None` without a `refs/remotes/origin/HEAD`, `Some(None)` with one that
/// is not usable (dangling or not a commit is caught by the caller).
fn origin_head(repo_root: &Path, timeout: Duration) -> Result<Option<Option<String>>, GateError> {
    let name = "refs/remotes/origin/HEAD";
    let args = ["symbolic-ref", "--quiet", name];
    if let Some(target) = aaf_fp::git_or_none(repo_root, &args, timeout)? {
        let usable = origin_head_branch(&target).is_some();
        return Ok(Some(usable.then_some(target)));
    }
    let args = ["rev-parse", "--verify", "--quiet", name];
    Ok(aaf_fp::git_or_none(repo_root, &args, timeout)?.map(|_| None))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    /// Commit recorded in the receipt; `None` in a repo with no commit yet.
    pub sha: Option<String>,
    /// What to diff against: `sha`, or the empty tree when there is no commit.
    pub diff_target: String,
}

/// `None` only when `name` resolves to no commit. A git that could not
/// answer is an error: a failed `HEAD` read as "no commit yet" made the
/// whole tree the change set (an edited test then passed the head lock as
/// an added file), and a failed trunk lookup fell back to `HEAD`, dropping
/// the committed work from the scope.
fn commit_of(repo_root: &Path, name: &str, timeout: Duration) -> Result<Option<String>, GateError> {
    let spec = format!("{name}^{{commit}}");
    let args = ["rev-parse", "--verify", "--quiet", spec.as_str()];
    Ok(aaf_fp::git_or_none(repo_root, &args, timeout)?)
}

/// `HEAD` commit for the receipt; `None` in a repository with no commit
/// yet, an error when git could not answer.
pub fn head(repo_root: &Path, timeout: Duration) -> Result<Option<String>, GateError> {
    commit_of(repo_root, "HEAD", timeout)
}

/// Resolves `gate.base_ref` to the merge-base with `HEAD`.
///
/// `auto` tries the branch the `origin/HEAD` symref names, then the usual
/// trunk refs, and takes the first that shares history with `HEAD`. With
/// none it falls back to `HEAD` (only uncommitted work is then in scope),
/// unless `origin/HEAD` exists: a broken one must not empty the change set.
/// An explicit ref that does not resolve is an error: silently using `HEAD`
/// would hide every committed change.
pub fn resolve(repo_root: &Path, base_ref: &str, timeout: Duration) -> Result<Base, GateError> {
    let Some(head) = commit_of(repo_root, "HEAD", timeout)? else {
        let empty_tree = aaf_fp::git(
            repo_root,
            &["hash-object", "-t", "tree", "/dev/null"],
            &[],
            timeout,
        )?;
        return Ok(Base {
            sha: None,
            diff_target: empty_tree,
        });
    };
    let wanted = base_ref.trim();
    let target = if wanted.is_empty() || wanted == "auto" {
        let origin_head = origin_head(repo_root, timeout)?;
        let first = origin_head.clone().flatten();
        let mut found = None;
        for name in first.iter().map(String::as_str).chain(AUTO_FALLBACKS) {
            let Some(target) = commit_of(repo_root, name, timeout)? else {
                continue;
            };
            // An orphan branch (say `origin/gh-pages`) is no base: try on.
            found = aaf_fp::git_or_none(repo_root, &["merge-base", &target, &head], timeout)?;
            if found.is_some() {
                break;
            }
        }
        if found.is_none() && origin_head.is_some() {
            let msg = "origin/HEAD is set but no base shares history with HEAD";
            return Err(GateError::Config(format!("{msg}; set gate.base_ref")));
        }
        found
    } else {
        // A leading dash would be read by git as an option.
        if wanted.starts_with('-') {
            return Err(GateError::Config(format!(
                "invalid gate.base_ref {wanted:?}"
            )));
        }
        Some(commit_of(repo_root, wanted, timeout)?.ok_or_else(|| {
            GateError::Config(format!(
                "gate.base_ref {wanted:?} does not resolve to a commit"
            ))
        })?)
    };
    let merge_base = match target {
        // Unrelated histories have no merge-base (git says so with exit 1
        // and no message): fall back to HEAD. A failed git is an error.
        Some(target) => aaf_fp::git_or_none(repo_root, &["merge-base", &target, &head], timeout)?,
        None => None,
    };
    let sha = merge_base.unwrap_or(head);
    Ok(Base {
        diff_target: sha.clone(),
        sha: Some(sha),
    })
}
