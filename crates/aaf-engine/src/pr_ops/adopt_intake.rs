//! What `aaf board adopt` checks before it creates anything (plan I8): the PR
//! is open, not a draft, not a fork, aimed at this repository's base, from a
//! head that is not the factory's own or a protected branch, written by a
//! trusted login; the head `gh` names is the head the remote serves, it is
//! here locally, and the PR changes something.
//!
//! Every refusal carries a stable `code` (one per rule) next to the message
//! for the person, so a test or a script can tell the rules apart.

use std::path::Path;

use super::pr_merge_support::{InMemoryMerge, fetch_base, in_memory_merge_tree, valid_sha40};
use crate::git::base_ref::BaseRef;
use crate::git::cmd::{git, git_raw};
use crate::git::{GhClient, GitError, PrInfo, has_remote, is_protected_branch};

/// A rule the PR breaks. Nothing has been created when this is returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: &'static str,
    pub message: String,
}

impl Refusal {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// A PR that passed every check, with the head the checks were made on.
#[derive(Debug, Clone)]
pub struct Intake {
    pub pr: PrInfo,
    /// The 40-hex head commit, lower case: the one `gh` named and the remote
    /// served, now in the local object store.
    pub head_sha: String,
    /// The base branch tip the emptiness check used.
    pub base_sha: String,
    /// The remote the base lives on.
    pub remote: String,
}

/// True when `login` is one of `allowlist` (trimmed, case-insensitive, blank
/// entries never match).
pub fn login_is_trusted(login: &str, allowlist: &[String]) -> bool {
    let login = login.trim();
    !login.is_empty()
        && allowlist.iter().any(|entry| {
            let entry = entry.trim();
            !entry.is_empty() && entry.eq_ignore_ascii_case(login)
        })
}

/// True when the head branch is the factory's own namespace: `aaf` or `aaf/...`,
/// in any case. Deliberately not `is_protected_branch`: that one is about the
/// base, this one is about a name the factory would claim as its own.
pub fn is_factory_branch(head: &str) -> bool {
    let lower = head.trim().to_ascii_lowercase();
    lower == "aaf" || lower.starts_with("aaf/")
}

/// The dedup key of an adoption: the repository path of the PR URL and the PR
/// number, `gh-pr:<owner>/<repo>#<N>`. Distinct from the board's `gh-issue:`.
pub fn fingerprint(pr: &PrInfo) -> String {
    let repo = pr
        .url
        .split_once("://")
        .map_or(pr.url.as_str(), |(_, rest)| rest)
        .split_once("/pull/")
        .and_then(|(head, _)| head.split_once('/'))
        .map_or("unknown", |(_, repo)| repo);
    format!("gh-pr:{repo}#{}", pr.number)
}

/// The rules that need only what `gh pr view` said.
pub fn check_view(pr: &PrInfo, base: &BaseRef, allowlist: &[String]) -> Result<(), Refusal> {
    if !pr.state.eq_ignore_ascii_case("OPEN") {
        return Err(Refusal::new(
            "pr_not_open",
            format!("PR #{} is {}, not OPEN", pr.number, pr.state),
        ));
    }
    if pr.is_draft {
        return Err(Refusal::new(
            "pr_is_draft",
            format!(
                "PR #{} is a draft; mark it ready for review first (a draft is how a PR stays out of the factory)",
                pr.number
            ),
        ));
    }
    if pr.is_cross_repository {
        return Err(Refusal::new(
            "pr_is_fork",
            format!(
                "PR #{} comes from a fork; only branches of this repository are adopted",
                pr.number
            ),
        ));
    }
    if base.remote.is_none() {
        return Err(Refusal::new(
            "base_has_no_remote",
            format!(
                "the base branch {} has no remote; there is nothing to merge a PR into",
                base.branch
            ),
        ));
    }
    if pr.base_ref_name != base.branch {
        return Err(Refusal::new(
            "base_mismatch",
            format!(
                "PR #{} targets {}, but this repository's base is {}",
                pr.number, pr.base_ref_name, base.branch
            ),
        ));
    }
    if is_factory_branch(&pr.head_ref_name) {
        return Err(Refusal::new(
            "head_is_factory_branch",
            format!(
                "PR #{} comes from {}; aaf and aaf/* are the factory's own branches",
                pr.number, pr.head_ref_name
            ),
        ));
    }
    if is_protected_branch(&pr.head_ref_name, &pr.base_ref_name) {
        return Err(Refusal::new(
            "head_is_protected",
            format!(
                "PR #{} comes from {}, a protected branch",
                pr.number, pr.head_ref_name
            ),
        ));
    }
    let login = pr.author.as_ref().map_or("", |a| a.login.trim());
    if login.is_empty() {
        return Err(Refusal::new(
            "author_unknown",
            format!("PR #{} has no author (a deleted account?)", pr.number),
        ));
    }
    if !login_is_trusted(login, allowlist) {
        return Err(Refusal::new(
            "author_not_allowed",
            format!(
                "PR #{} was opened by {login}, who is not in [safety].owner_github_allowlist",
                pr.number
            ),
        ));
    }
    if !valid_sha40(&pr.head_ref_oid) {
        return Err(Refusal::new(
            "head_oid_invalid",
            format!("gh reports no usable head commit for PR #{}", pr.number),
        ));
    }
    Ok(())
}

/// Fetches `refs/pull/<N>/head` and returns the commit it served. Nothing is
/// stored under a ref name: the object is read through `FETCH_HEAD`.
fn fetch_served_head(repo_root: &Path, remote: &str, number: u64) -> Result<String, GitError> {
    if !has_remote(repo_root, remote)? {
        return Err(GitError::Invalid(format!("no remote named {remote:?}")));
    }
    let pr_ref = format!("refs/pull/{number}/head");
    git(repo_root, &["fetch", "--quiet", "--", remote, &pr_ref])?;
    git(repo_root, &["rev-parse", "--verify", "FETCH_HEAD^{commit}"])
        .map(|oid| oid.to_ascii_lowercase())
}

fn has_commit_and_tree(repo_root: &Path, sha: &str) -> Result<bool, GitError> {
    for kind in ["commit", "tree"] {
        let spec = format!("{sha}^{{{kind}}}");
        if !git_raw(repo_root, &["cat-file", "-e", &spec])?.success() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// True when merging `head` into `base` changes nothing: the merged tree is
/// the base's own tree. A conflict is a change. Compares trees, so a head that
/// is not an ancestor of the base but cancels itself out (a commit and its
/// revert) is empty too.
pub fn merges_to_nothing(
    repo_root: &Path,
    base_sha: &str,
    head_sha: &str,
) -> Result<bool, GitError> {
    match in_memory_merge_tree(repo_root, base_sha, head_sha)? {
        InMemoryMerge::Conflict { .. } => Ok(false),
        InMemoryMerge::Clean { tree_oid } => {
            let base_tree = git(repo_root, &["rev-parse", &format!("{base_sha}^{{tree}}")])?;
            Ok(tree_oid.eq_ignore_ascii_case(&base_tree))
        }
        // A git without `merge-tree --write-tree`: what the PR adds on top of
        // the merge base is the nearest honest answer.
        InMemoryMerge::Unsupported => {
            let range = format!("{base_sha}...{head_sha}");
            Ok(git_raw(repo_root, &["diff", "--quiet", &range])?.success())
        }
    }
}

/// Runs every I8 check for PR `number`. `Err` is an infrastructure failure
/// (`gh`, `git`); `Ok(Err(_))` is a rule the PR breaks.
pub fn intake(
    gh: &dyn GhClient,
    repo_root: &Path,
    number: u64,
    base: &BaseRef,
    allowlist: &[String],
) -> Result<Result<Intake, Refusal>, GitError> {
    if !allowlist.iter().any(|entry| !entry.trim().is_empty()) {
        return Ok(Err(Refusal::new(
            "allowlist_empty",
            "[safety].owner_github_allowlist has no trusted login; add the PR authors you trust \
             to the global config before adopting",
        )));
    }
    let pr = gh.view_pr(repo_root, number)?;
    if pr.number != number {
        return Ok(Err(Refusal::new(
            "pr_mismatch",
            format!("asked gh for PR #{number} and got #{}", pr.number),
        )));
    }
    if let Err(refusal) = check_view(&pr, base, allowlist) {
        return Ok(Err(refusal));
    }
    let Some(remote) = base.remote.clone() else {
        return Ok(Err(Refusal::new(
            "base_has_no_remote",
            "the base has no remote",
        )));
    };
    let wanted = pr.head_ref_oid.to_ascii_lowercase();
    let served = fetch_served_head(repo_root, &remote, number)?;
    if served != wanted {
        return Ok(Err(Refusal::new(
            "head_oid_mismatch",
            format!(
                "gh says PR #{number} is at {wanted}, but the remote serves {served}; \
                 the PR moved while it was read: wait a moment and run adopt again"
            ),
        )));
    }
    if !has_commit_and_tree(repo_root, &served)? {
        return Ok(Err(Refusal::new(
            "head_not_fetched",
            format!("the commit {served} (or its tree) is not in the local object store"),
        )));
    }
    let base_sha = fetch_base(repo_root, &remote, &pr.base_ref_name)?;
    if merges_to_nothing(repo_root, &base_sha, &served)? {
        return Ok(Err(Refusal::new(
            "diff_empty",
            format!(
                "PR #{number} changes nothing against {}: merging it would add no change",
                pr.base_ref_name
            ),
        )));
    }
    Ok(Ok(Intake {
        pr,
        head_sha: served,
        base_sha,
        remote,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::gh_client::PrAuthor;

    fn pr() -> PrInfo {
        PrInfo {
            number: 7,
            state: "OPEN".into(),
            head_ref_oid: "a".repeat(40),
            head_ref_name: "feat/x".into(),
            base_ref_name: "main".into(),
            url: "https://github.com/org/repo/pull/7".into(),
            author: Some(PrAuthor {
                login: "Octodev".into(),
            }),
            ..PrInfo::default()
        }
    }

    fn base() -> BaseRef {
        BaseRef {
            remote: Some("origin".into()),
            branch: "main".into(),
        }
    }

    fn allow() -> Vec<String> {
        vec![" octodev ".to_string()]
    }

    fn code(pr: &PrInfo, base: &BaseRef, allow: &[String]) -> Option<&'static str> {
        check_view(pr, base, allow).err().map(|r| r.code)
    }

    #[test]
    fn a_good_pr_passes_the_view_rules() {
        assert_eq!(code(&pr(), &base(), &allow()), None);
    }

    #[test]
    fn every_view_rule_has_its_own_code() {
        let mut closed = pr();
        closed.state = "CLOSED".into();
        let mut merged = pr();
        merged.state = "MERGED".into();
        let mut draft = pr();
        draft.is_draft = true;
        let mut fork = pr();
        fork.is_cross_repository = true;
        let mut wrong_base = pr();
        wrong_base.base_ref_name = "develop".into();
        let mut no_author = pr();
        no_author.author = None;
        let mut blank_author = pr();
        blank_author.author = Some(PrAuthor { login: "  ".into() });
        let mut stranger = pr();
        stranger.author = Some(PrAuthor {
            login: "mallory".into(),
        });
        let mut short_oid = pr();
        short_oid.head_ref_oid = "abc".into();
        let cases = [
            (closed, "pr_not_open"),
            (merged, "pr_not_open"),
            (draft, "pr_is_draft"),
            (fork, "pr_is_fork"),
            (wrong_base, "base_mismatch"),
            (no_author, "author_unknown"),
            (blank_author, "author_unknown"),
            (stranger, "author_not_allowed"),
            (short_oid, "head_oid_invalid"),
        ];
        for (pr, expected) in cases {
            assert_eq!(code(&pr, &base(), &allow()), Some(expected), "{pr:?}");
        }
        let local_base = BaseRef {
            remote: None,
            branch: "main".into(),
        };
        assert_eq!(
            code(&pr(), &local_base, &allow()),
            Some("base_has_no_remote")
        );
    }

    #[test]
    fn the_factory_namespace_is_refused_in_any_case_and_before_protection() {
        for head in ["aaf/t-1", "AAF/t-1", "Aaf/x", "aaf", "AAF", " aaf/x "] {
            let mut p = pr();
            p.head_ref_name = head.into();
            assert_eq!(
                code(&p, &base(), &allow()),
                Some("head_is_factory_branch"),
                "{head}"
            );
        }
        for head in ["aafx", "my-aaf/x", "feat/aaf"] {
            let mut p = pr();
            p.head_ref_name = head.into();
            assert_eq!(code(&p, &base(), &allow()), None, "{head}");
        }
        for head in ["main", "MASTER", "release/1.2", "public-site", "tr\u{fc}nk"] {
            let mut p = pr();
            p.head_ref_name = head.into();
            assert_eq!(
                code(&p, &base(), &allow()),
                Some("head_is_protected"),
                "{head}"
            );
        }
    }

    #[test]
    fn the_allowlist_is_trimmed_and_case_insensitive_and_blank_never_matches() {
        assert!(login_is_trusted("OCTODEV", &allow()));
        assert!(login_is_trusted(" octodev", &allow()));
        assert!(!login_is_trusted("", &["".to_string()]));
        assert!(!login_is_trusted("octodev", &["  ".to_string()]));
        assert!(!login_is_trusted("alexx", &allow()));
    }

    #[test]
    fn the_fingerprint_names_the_repository_and_never_looks_like_an_issue() {
        assert_eq!(fingerprint(&pr()), "gh-pr:org/repo#7");
        let mut odd = pr();
        odd.url = String::new();
        assert_eq!(fingerprint(&odd), "gh-pr:unknown#7");
        assert!(!fingerprint(&pr()).starts_with("gh-issue:"));
    }
}
