#![allow(clippy::unwrap_used, clippy::expect_used)]

//! `aaf board adopt <N>`: every input rule of plan I8 refuses, with its own
//! reason code, and creates nothing (no task, no `aaf/*` ref, no worktree, no
//! comment). The `gh` here is a script on `PATH`; GitHub is never reached.

#[path = "adopt_cmd_support.rs"]
mod support;

use support::{Fixture, View};

/// Runs the adoption and checks it was refused for `reason`.
fn refused_with(fx: &Fixture, number: u64, reason: &str) {
    let run = fx.adopt(number);
    assert_ne!(run.code, 0, "{reason}: {}", run.stderr);
    assert_eq!(run.json["status"], "refused", "{reason}: {}", run.stderr);
    assert_eq!(run.json["reason"], reason, "{}", run.stderr);
    fx.assert_nothing_created(reason);
    assert!(
        !fx.gh_calls().iter().any(|c| c.starts_with("pr comment")),
        "{reason}: a refused adoption must not comment"
    );
}

#[test]
fn a_closed_or_merged_pr_is_refused() {
    let fx = Fixture::new("\"alex\"");
    let head = fx.chat_pr("feat/chat", 7);
    for state in ["CLOSED", "MERGED"] {
        let mut view = View::open(7, &head, "feat/chat");
        view.state = state;
        fx.show_pr(&view);
        refused_with(&fx, 7, "pr_not_open");
    }
}

#[test]
fn a_draft_pr_is_refused() {
    let fx = Fixture::new("\"alex\"");
    let head = fx.chat_pr("feat/chat", 7);
    let mut view = View::open(7, &head, "feat/chat");
    view.draft = true;
    fx.show_pr(&view);
    refused_with(&fx, 7, "pr_is_draft");
}

#[test]
fn a_fork_pr_is_refused() {
    let fx = Fixture::new("\"alex\"");
    let head = fx.chat_pr("feat/chat", 7);
    let mut view = View::open(7, &head, "feat/chat");
    view.fork = true;
    fx.show_pr(&view);
    refused_with(&fx, 7, "pr_is_fork");
}

#[test]
fn a_pr_aimed_at_another_base_is_refused() {
    let fx = Fixture::new("\"alex\"");
    let head = fx.chat_pr("feat/chat", 7);
    let mut view = View::open(7, &head, "feat/chat");
    view.base_name = "develop";
    fx.show_pr(&view);
    refused_with(&fx, 7, "base_mismatch");
}

#[test]
fn a_base_without_a_remote_is_refused() {
    let fx = Fixture::without_remote();
    let head = support::git(&fx.repo, &["rev-parse", "HEAD"]);
    fx.show_pr(&View::open(7, &head, "feat/chat"));
    refused_with(&fx, 7, "base_has_no_remote");
}

#[test]
fn a_head_in_the_factory_namespace_is_refused_in_any_case() {
    let fx = Fixture::new("\"alex\"");
    let head = fx.chat_pr("work/chat", 7);
    for name in ["aaf/t-1234abcd", "AAF/t-1234abcd", "Aaf/x", "aaf", "AAF"] {
        fx.show_pr(&View::open(7, &head, name));
        refused_with(&fx, 7, "head_is_factory_branch");
    }
}

#[test]
fn a_protected_head_is_refused() {
    let fx = Fixture::new("\"alex\"");
    let head = fx.chat_pr("feat/chat", 7);
    for name in ["main", "release/1.0", "public-site"] {
        fx.show_pr(&View::open(7, &head, name));
        refused_with(&fx, 7, "head_is_protected");
    }
}

#[test]
fn a_pr_without_an_author_is_refused() {
    let fx = Fixture::new("\"alex\"");
    let head = fx.chat_pr("feat/chat", 7);
    let mut view = View::open(7, &head, "feat/chat");
    view.author = None;
    fx.show_pr(&view);
    refused_with(&fx, 7, "author_unknown");
}

#[test]
fn an_author_outside_the_allowlist_is_refused() {
    let fx = Fixture::new("\"alex\", \"sam\"");
    let head = fx.chat_pr("feat/chat", 7);
    let mut view = View::open(7, &head, "feat/chat");
    view.author = Some("mallory");
    fx.show_pr(&view);
    refused_with(&fx, 7, "author_not_allowed");
}

#[test]
fn an_empty_or_blank_allowlist_refuses_before_gh_is_asked() {
    for allowlist in ["", "\"\"", "\"  \", \"\""] {
        let fx = Fixture::new(allowlist);
        let head = fx.chat_pr("feat/chat", 7);
        fx.show_pr(&View::open(7, &head, "feat/chat"));
        refused_with(&fx, 7, "allowlist_empty");
        assert!(
            fx.gh_calls().is_empty(),
            "no gh call without a trusted login: {:?}",
            fx.gh_calls()
        );
    }
}

#[test]
fn an_allowlisted_author_matches_whatever_the_case() {
    let fx = Fixture::new("\" ALEX \"");
    let head = fx.chat_pr("feat/chat", 7);
    fx.show_pr(&View::open(7, &head, "feat/chat"));
    let run = fx.adopt(7);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.json["status"], "adopting");
}

#[test]
fn a_pr_that_merges_to_nothing_is_refused() {
    let fx = Fixture::new("\"alex\"");
    let head = fx.self_cancelling_pr("feat/noop", 7);
    // Not an ancestor of main, and main is not an ancestor of it either:
    // only the tree comparison sees that it adds nothing.
    fx.show_pr(&View::open(7, &head, "feat/noop"));
    refused_with(&fx, 7, "diff_empty");
}

#[test]
fn a_head_that_is_already_in_the_base_is_refused() {
    let fx = Fixture::new("\"alex\"");
    let head = support::git(&fx.repo, &["rev-parse", "HEAD"]);
    support::git(&fx.bare, &["update-ref", "refs/pull/7/head", &head]);
    fx.show_pr(&View::open(7, &head, "feat/chat"));
    refused_with(&fx, 7, "diff_empty");
}

#[test]
fn a_head_oid_gh_names_but_the_remote_does_not_serve_is_refused() {
    let fx = Fixture::new("\"alex\"");
    let served = fx.chat_pr("feat/chat", 7);
    // `gh` names a commit that is in the local object store, so a check that
    // trusts "it exists here" would pass; the remote serves another one.
    let local_only = support::git(&fx.repo, &["rev-parse", "main"]);
    assert_ne!(served, local_only);
    fx.show_pr(&View::open(7, &local_only, "feat/chat"));
    refused_with(&fx, 7, "head_oid_mismatch");
}

#[test]
fn a_head_oid_that_is_not_a_commit_id_is_refused() {
    let fx = Fixture::new("\"alex\"");
    fx.chat_pr("feat/chat", 7);
    fx.show_pr(&View::open(7, "abc123", "feat/chat"));
    refused_with(&fx, 7, "head_oid_invalid");
}

#[test]
fn a_pr_number_gh_does_not_confirm_is_refused() {
    let fx = Fixture::new("\"alex\"");
    let head = fx.chat_pr("feat/chat", 7);
    fx.show_pr(&View::open(8, &head, "feat/chat"));
    refused_with(&fx, 7, "pr_mismatch");
}

#[test]
fn a_bad_author_executor_is_an_error_before_anything_happens() {
    let fx = Fixture::new("\"alex\"");
    let head = fx.chat_pr("feat/chat", 7);
    fx.show_pr(&View::open(7, &head, "feat/chat"));
    let run = fx.run_in(
        &fx.repo,
        &["board", "adopt", "7", "--author-executor", "  "],
        None,
    );
    assert_ne!(run.code, 0);
    assert!(fx.gh_calls().is_empty());
    fx.assert_nothing_created("blank executor");
}
