//! Base ref resolution: what the current work is compared against.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use super::GateError;
use super::hidden::HiddenPolicy;

/// Optional per-run overrides that keep `config_hash(cfg)` aligned with the
/// committed repository config so callers such as `aaf merge` can reuse a
/// signed common receipt while enforcing an explicit base or independent
/// approval requirement.
#[derive(Debug, Clone, Copy, Default)]
pub struct GateRunOptions<'a> {
    pub policy: HiddenPolicy,
    pub base_ref_override: Option<&'a str>,
    pub require_independent_approval: bool,
    /// Permit publishing a cross-worktree common receipt even if ignored files
    /// exist; set `true` only by trusted clean merge verifiers (`aaf merge`).
    pub allow_common_receipt_write: bool,
    /// Strip compiler wrapper and shared cache env vars (`RUSTC_WRAPPER`,
    /// `SCCACHE_DIR`, etc.) from gate commands; set `true` by `aaf merge`.
    pub strip_compiler_wrappers: bool,
    /// Optional Tier-1 `[proof].secret_env_allowlist` override; when `None`,
    /// proof flows read it from the global config file.
    pub proof_secret_allowlist: Option<&'a [String]>,
}

/// Trunks `auto` tries after `origin/HEAD`, as full ref names: a short
/// name would let a tag or local branch called `origin/main` stand in.
pub(crate) const AUTO_FALLBACKS: [&str; 4] = [
    "refs/remotes/origin/main",
    "refs/remotes/origin/master",
    "refs/heads/main",
    "refs/heads/master",
];

const ORIGIN_HEAD: &str = "refs/remotes/origin/HEAD";
const AUTO_REFS_FORMAT: &str = "--format=%(refname)%09%(symref)%09%(objecttype)%09%(objectname)%09%(*objecttype)%09%(*objectname)%09end";

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

/// Parses a full remote-tracking ref (`refs/remotes/<remote>/<branch>`) into
/// `(remote, branch)`, rejecting invalid segments, `HEAD`, and task branches
/// (`aaf/*`) so an upstream ref can never empty the change set.
pub(crate) fn parse_remote_tracking_ref(full_ref: &str) -> Option<(&str, &str)> {
    let rest = full_ref.strip_prefix("refs/remotes/")?;
    let (remote, branch) = rest.split_once('/')?;
    if remote.is_empty() || remote.starts_with('-') || matches!(remote, "." | "..") {
        return None;
    }
    let task = branch.starts_with(crate::git::worktree::BRANCH_PREFIX);
    let parts_ok = branch.split('/').all(|p| !matches!(p, "" | "." | ".."));
    (parts_ok && branch != "HEAD" && !task).then_some((remote, branch))
}

/// Probes `@{upstream}` as a fallback when `origin/HEAD` and `AUTO_FALLBACKS`
/// did not resolve, rejecting `aaf/*` task branches.
fn resolve_upstream_commit(
    repo_root: &Path,
    timeout: Duration,
) -> Result<Option<String>, GateError> {
    let full_ref = match aaf_fp::git_or_none(
        repo_root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--symbolic-full-name",
            "@{upstream}",
        ],
        timeout,
    ) {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(None),
        Err(aaf_fp::FpError::Git(msg))
            if msg.contains("no upstream configured")
                || msg.contains("does not point to a branch") =>
        {
            return Ok(None);
        }
        Err(err) => return Err(err.into()),
    };
    if parse_remote_tracking_ref(&full_ref).is_none() {
        return Ok(None);
    }
    commit_of(repo_root, &full_ref, timeout)
}

/// Probes `refs/remotes/origin/HEAD` and [`AUTO_FALLBACKS`] in a single
/// `git for-each-ref` call. Returns `(origin_head_seen, candidate_commits)` in
/// priority order.
fn resolve_auto_candidates(
    repo_root: &Path,
    timeout: Duration,
) -> Result<(bool, Vec<String>), GateError> {
    let mut args = vec!["for-each-ref", AUTO_REFS_FORMAT, ORIGIN_HEAD];
    args.extend(AUTO_FALLBACKS);
    let out = aaf_fp::git(repo_root, &args, &[], timeout)?;
    let mut origin_head_seen = false;
    let mut origin_head_commit: Option<String> = None;
    let mut fallbacks: HashMap<&str, String> = HashMap::new();
    for line in out.lines().filter(|line| !line.is_empty()) {
        let parts: Vec<&str> = line.split('\t').collect();
        let Some(refname) = parts.first().copied() else {
            continue;
        };
        let symref = parts.get(1).copied().unwrap_or("");
        let obj_type = parts.get(2).copied().unwrap_or("");
        let obj_name = parts.get(3).copied().unwrap_or("");
        let peeled_type = parts.get(4).copied().unwrap_or("");
        let peeled_name = parts.get(5).copied().unwrap_or("");
        let commit_sha = if obj_type == "commit" && !obj_name.is_empty() {
            Some(obj_name.to_string())
        } else if peeled_type == "commit" && !peeled_name.is_empty() {
            Some(peeled_name.to_string())
        } else {
            None
        };
        if refname == ORIGIN_HEAD {
            origin_head_seen = true;
            if !symref.is_empty() && origin_head_branch(symref).is_some() {
                origin_head_commit = commit_sha;
            }
        } else if let Some(sha) = commit_sha {
            fallbacks.insert(refname, sha);
        }
    }
    let mut candidates = Vec::with_capacity(1 + AUTO_FALLBACKS.len());
    if let Some(sha) = origin_head_commit {
        candidates.push(sha);
    }
    for name in AUTO_FALLBACKS {
        if let Some(sha) = fallbacks.remove(name)
            && !candidates.contains(&sha)
        {
            candidates.push(sha);
        }
    }
    Ok((origin_head_seen, candidates))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    /// Commit recorded in the receipt; `None` in a repo with no commit yet.
    pub sha: Option<String>,
    /// What to diff against: `sha`, or the empty tree when there is no commit.
    pub diff_target: String,
    /// `HEAD` commit at resolution time; `None` in a repo with no commit yet.
    pub head_sha: Option<String>,
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

/// Resolves an explicit `gate.base_ref` without letting a tag shadow a
/// branch (SEC-034). Short names try `refs/heads/<name>` then
/// `refs/remotes/<name>`; a name that only exists as a tag must be spelled
/// `refs/tags/<name>`. Full refs and other revisions (a SHA, `HEAD~2`)
/// resolve as git spells them.
fn explicit_base_commit(
    repo_root: &Path,
    wanted: &str,
    timeout: Duration,
) -> Result<String, GateError> {
    let unresolved = || {
        GateError::Config(format!(
            "gate.base_ref {wanted:?} does not resolve to a commit"
        ))
    };
    if wanted.starts_with("refs/") {
        return commit_of(repo_root, wanted, timeout)?.ok_or_else(unresolved);
    }
    let mut found: Option<(String, String)> = None;
    for full in [
        format!("refs/heads/{wanted}"),
        format!("refs/remotes/{wanted}"),
    ] {
        if let Some(sha) = commit_of(repo_root, &full, timeout)? {
            // A local branch named like a remote-tracking ref (or the other
            // way round) is refused rather than guessed at.
            if let Some((other, _)) = &found {
                return Err(GateError::Config(format!(
                    "gate.base_ref {wanted:?} is ambiguous ({other} and {full}); \
                     write the full ref name"
                )));
            }
            found = Some((full, sha));
        }
    }
    if let Some((_, sha)) = found {
        return Ok(sha);
    }
    let tag = format!("refs/tags/{wanted}");
    if aaf_fp::git_or_none(
        repo_root,
        &["rev-parse", "--verify", "--quiet", &tag],
        timeout,
    )?
    .is_some()
    {
        return Err(GateError::Config(format!(
            "gate.base_ref {wanted:?} only names a tag; write {tag:?} to use it"
        )));
    }
    commit_of(repo_root, wanted, timeout)?.ok_or_else(unresolved)
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
            head_sha: None,
        });
    };
    let wanted = base_ref.trim();
    let merge_base = if wanted.is_empty() || wanted == "auto" {
        let (origin_head_seen, candidates) = resolve_auto_candidates(repo_root, timeout)?;
        let mut found = None;
        for target in candidates {
            // An orphan branch (say `origin/gh-pages`) is no base: try on.
            found = aaf_fp::git_or_none(repo_root, &["merge-base", &target, &head], timeout)?;
            if found.is_some() {
                break;
            }
        }
        if found.is_none() {
            // `for-each-ref` omits a dangling symref; check explicitly before
            // allowing a fallback to `HEAD`.
            let has_origin_head = origin_head_seen
                || aaf_fp::git_or_none(
                    repo_root,
                    &["symbolic-ref", "--quiet", ORIGIN_HEAD],
                    timeout,
                )?
                .is_some();
            if has_origin_head {
                let msg = "origin/HEAD is set but no base shares history with HEAD";
                return Err(GateError::Config(format!("{msg}; set gate.base_ref")));
            }
            if let Some(upstream_sha) = resolve_upstream_commit(repo_root, timeout)? {
                found =
                    aaf_fp::git_or_none(repo_root, &["merge-base", &upstream_sha, &head], timeout)?;
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
        let target = explicit_base_commit(repo_root, wanted, timeout)?;
        // Unrelated histories have no merge-base (git says so with exit 1
        // and no message): fall back to HEAD. A failed git is an error.
        aaf_fp::git_or_none(repo_root, &["merge-base", &target, &head], timeout)?
    };
    let sha = merge_base.unwrap_or_else(|| head.clone());
    Ok(Base {
        diff_target: sha.clone(),
        sha: Some(sha),
        head_sha: Some(head),
    })
}
