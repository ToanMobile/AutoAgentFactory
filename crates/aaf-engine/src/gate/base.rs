//! Base ref resolution: what the current work is compared against.

use std::path::Path;
use std::time::Duration;

use super::GateError;

const AUTO_CANDIDATES: [&str; 5] = [
    "origin/HEAD",
    "origin/main",
    "origin/master",
    "main",
    "master",
];

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
/// `auto` tries the usual trunk names and falls back to `HEAD` (only
/// uncommitted work is then in scope). An explicit ref that does not resolve
/// is an error: silently using `HEAD` would hide every committed change.
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
        let mut found = None;
        for name in AUTO_CANDIDATES {
            found = commit_of(repo_root, name, timeout)?;
            if found.is_some() {
                break;
            }
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
