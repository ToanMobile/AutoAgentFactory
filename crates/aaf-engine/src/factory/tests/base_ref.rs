//! `gate.base_ref` forms and the full ref names `BaseRef::start_point`
//! hands to git (SEC-022: a tag must never shadow the base branch).

use aaf_domain::ports::PortError;

use super::git_real::{Fixture, git};
use crate::factory::{BaseRef, resolve_base};

fn base(remote: Option<&str>, branch: &str) -> BaseRef {
    BaseRef {
        remote: remote.map(str::to_string),
        branch: branch.to_string(),
    }
}

#[test]
fn start_point_qualifies_branches_and_keeps_explicit_commits() {
    assert_eq!(base(None, "main").start_point(), "refs/heads/main");
    assert_eq!(
        base(Some("origin"), "main").start_point(),
        "refs/remotes/origin/main"
    );
    let sha1 = "a".repeat(40);
    let sha256 = "b".repeat(64);
    for kept in [
        sha1.as_str(),
        sha256.as_str(),
        "HEAD",
        "refs/heads/main",
        "refs/remotes/origin/main",
    ] {
        assert_eq!(base(None, kept).start_point(), kept);
    }
}

#[test]
fn explicit_base_refs_resolve_to_branches_or_a_pinned_sha() {
    let fx = Fixture::new();
    let head = git(&fx.repo, &["rev-parse", "HEAD"]);
    let bare = fx.tmp.path().join("origin.git");
    git(fx.tmp.path(), &["init", "--quiet", "--bare", "origin.git"]);
    git(
        &fx.repo,
        &["remote", "add", "origin", bare.to_str().unwrap()],
    );
    git(&fx.repo, &["push", "--quiet", "origin", "main"]);
    git(&fx.repo, &["tag", "v1", &head]);
    // A remote-tracking-looking ref of a remote that is not configured.
    git(&fx.repo, &["update-ref", "refs/remotes/ghost/main", &head]);

    let local = base(None, "main");
    let remote = base(Some("origin"), "main");
    let pinned = base(None, &head);
    for (wanted, expected) in [
        ("main", &local),
        ("refs/heads/main", &local),
        ("origin/main", &remote),
        ("refs/remotes/origin/main", &remote),
        (head.as_str(), &pinned),
        (&head[..7], &pinned),
        ("refs/tags/v1", &pinned),
        ("refs/remotes/ghost/main", &pinned),
    ] {
        let got = resolve_base(&fx.repo, wanted).unwrap();
        assert_eq!(&got, expected, "{wanted}");
    }
    assert_eq!(pinned.start_point(), head);
    // A bare tag name is refused: it must be spelled `refs/tags/v1`.
    assert!(matches!(
        resolve_base(&fx.repo, "v1"),
        Err(PortError::Invalid(_))
    ));
}

#[test]
fn a_tag_naming_a_tree_does_not_refuse_the_branch_it_shadows() {
    let fx = Fixture::new();
    let tree = git(&fx.repo, &["rev-parse", "HEAD^{tree}"]);
    git(&fx.repo, &["tag", "main", &tree]);
    assert_eq!(resolve_base(&fx.repo, "main").unwrap(), base(None, "main"));
}

#[test]
fn a_remote_branch_without_its_tracking_ref_is_refused_not_read_from_a_tag() {
    let fx = Fixture::new();
    let head = git(&fx.repo, &["rev-parse", "HEAD"]);
    let bare = fx.tmp.path().join("origin.git");
    git(fx.tmp.path(), &["init", "--quiet", "--bare", "origin.git"]);
    git(
        &fx.repo,
        &["remote", "add", "origin", bare.to_str().unwrap()],
    );
    git(&fx.repo, &["push", "--quiet", "origin", "main"]);
    // Tracking ref pruned; a tag now answers to `origin/main`.
    git(&fx.repo, &["update-ref", "-d", "refs/remotes/origin/main"]);
    git(&fx.repo, &["tag", "origin/main", &head]);
    assert!(matches!(
        resolve_base(&fx.repo, "origin/main"),
        Err(PortError::Invalid(_))
    ));
}

#[test]
fn a_tag_named_like_the_branch_does_not_change_the_explicit_base() {
    let fx = Fixture::new();
    let trunk = git(&fx.repo, &["rev-parse", "HEAD"]);
    git(
        &fx.repo,
        &["commit", "--quiet", "--allow-empty", "-m", "other"],
    );
    let other = git(&fx.repo, &["rev-parse", "HEAD"]);
    git(&fx.repo, &["reset", "--quiet", "--hard", &trunk]);
    git(&fx.repo, &["tag", "main", &other]);

    let got = resolve_base(&fx.repo, "main").unwrap();
    assert_eq!(got, base(None, "main"));
    let sha = git(&fx.repo, &["rev-parse", "--verify", &got.start_point()]);
    assert_eq!(sha, trunk);
}
