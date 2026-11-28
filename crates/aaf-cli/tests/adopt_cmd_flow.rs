#![allow(clippy::unwrap_used, clippy::expect_used)]

//! `aaf board adopt <N>` end to end against a fake `gh`: the pin (ref, worktree
//! on the branch, clean), the task and its two events exactly as the adopt
//! lane reads them, the comment on the PR, and what happens on a second call,
//! after a cancel, when the store refuses, inside an agent run, and in
//! `board list` / `show`.

#[path = "adopt_cmd_support.rs"]
mod support;

use std::fs;

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{EventSink, TaskStore};
use aaf_domain::task::TaskState;
use aaf_engine::git::worktree::{existing_registered, worktree_path};
use serde_json::json;
use support::{Fixture, Run, View, git};

fn ready(fx: &Fixture, branch: &str, number: u64) -> String {
    let head = fx.chat_pr(branch, number);
    fx.show_pr(&View::open(number, &head, branch));
    head
}

/// What the lane's `check_pin` and `Lane::new` need, checked on the real result.
fn assert_pinned(fx: &Fixture, run: &Run, head: &str, number: u64) -> String {
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.json["status"], "adopting", "{}", run.stderr);
    let id = run.json["task_id"].as_str().unwrap().to_string();
    assert_eq!(run.json["head_sha"], head);

    // The branch is the pin.
    assert_eq!(
        git(&fx.repo, &["rev-parse", &format!("refs/heads/aaf/{id}")]),
        head
    );
    // The worktree is where the lane looks for it, at the pin, on the branch, clean.
    let expected = worktree_path(&fx.repo, &fx.worktrees, &id).unwrap();
    let (tree, tree_head) = existing_registered(&fx.repo, &fx.worktrees, &id)
        .unwrap()
        .expect("the adopt worktree is registered");
    assert_eq!(tree.path, expected);
    assert_eq!(tree_head, head);
    assert_eq!(git(&tree.path, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        git(&tree.path, &["symbolic-ref", "HEAD"]),
        format!("refs/heads/aaf/{id}")
    );
    assert_eq!(git(&tree.path, &["status", "--porcelain"]), "");
    assert_eq!(
        fs::read_to_string(tree.path.join("src/lib.rs")).unwrap(),
        "pub fn v() -> i32 { 2 }\n",
        "the worktree holds the PR's content"
    );

    // One task, adopting, under the main checkout, with its two events.
    let tasks = fx.tasks();
    let task = tasks.iter().find(|t| t.id == id).expect("the task");
    assert_eq!(task.state, TaskState::Adopting);
    assert_eq!(task.repo_root, fx.repo.to_str().unwrap());
    assert_eq!(
        task.fingerprint.as_deref(),
        Some(format!("gh-pr:org/repo#{number}").as_str())
    );
    assert!(task.spec.description.contains("UNTRUSTED_DATA_BEGIN"));
    assert!(task.spec.description.contains("Please adopt."));
    let kinds: Vec<EventKind> = fx.events(&id).iter().map(|e| e.kind).collect();
    assert_eq!(kinds, [EventKind::TaskCreated, EventKind::PrAdopted]);

    // The payload the lane parses, with a valid pinned head.
    let pin = fx.pr_adopted(&id);
    assert!(pin.head_is_valid());
    assert_eq!(pin.pinned(), head);
    assert_eq!(pin.pr_number, number);
    assert_eq!(
        pin.url,
        format!("https://github.com/org/repo/pull/{number}")
    );
    assert_eq!(pin.base_ref, "main");
    assert_eq!(pin.author_login, "alex");
    id
}

#[test]
fn adopting_from_the_main_checkout_pins_the_head_and_posts_the_bot_comment() {
    let fx = Fixture::new("\"alex\"");
    let head = ready(&fx, "feat/chat", 7);
    let run = fx.adopt(7);
    let id = assert_pinned(&fx, &run, &head, 7);

    let pin = fx.pr_adopted(&id);
    assert_eq!(pin.head_ref, "feat/chat");
    assert_eq!(pin.declared_executor, None);
    assert_eq!(run.json["commented"], true);
    assert_eq!(run.json["branch"], format!("aaf/{id}"));
    let comment = fx.comment().expect("a comment was posted");
    assert!(
        comment.starts_with("--body=\u{1F916} aaf-bot: "),
        "the bot marker leads the comment: {comment}"
    );
    assert!(comment.contains(&id));
    assert!(
        fx.gh_calls()
            .iter()
            .all(|c| c.starts_with("pr view") || c.starts_with("pr comment")),
        "nothing but view and comment: {:?}",
        fx.gh_calls()
    );
    // The chat's own branch is untouched.
    assert_eq!(git(&fx.repo, &["rev-parse", "refs/heads/feat/chat"]), head);
}

#[test]
fn adopting_from_a_linked_worktree_uses_the_main_checkout() {
    let fx = Fixture::new("\"alex\"");
    let head = ready(&fx, "feat/chat", 7);
    let linked = fx.root.join("linked");
    git(
        &fx.repo,
        &[
            "worktree",
            "add",
            "-b",
            "scratch",
            linked.to_str().unwrap(),
            "main",
        ],
    );
    let run = fx.run_in(&linked, &["board", "adopt", "7"], None);
    let id = assert_pinned(&fx, &run, &head, 7);
    // Registered under the main checkout's repo id, not the linked one's.
    let path = worktree_path(&fx.repo, &fx.worktrees, &id).unwrap();
    assert!(path.join("src/lib.rs").is_file());
    assert!(
        !worktree_path(&linked, &fx.worktrees, &id).unwrap().exists(),
        "no second worktree under the linked checkout's id"
    );
}

#[test]
fn no_hook_runs_while_the_pr_head_is_checked_out() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::new("\"alex\"");
    let marker = fx.root.join("hook-ran");
    // A repository that keeps its hooks in the tree (`.githooks`), so the PR's
    // checkout has them too.
    fs::create_dir_all(fx.repo.join(".githooks")).unwrap();
    let hook = fx.repo.join(".githooks/post-checkout");
    fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    git(&fx.repo, &["add", "-A"]);
    git(&fx.repo, &["commit", "-m", "hooks in the tree"]);
    git(&fx.repo, &["push", "origin", "main"]);
    let head = fx.chat_pr("feat/hook", 7);
    git(&fx.repo, &["config", "core.hooksPath", ".githooks"]);
    fx.show_pr(&View::open(7, &head, "feat/hook"));

    let run = fx.adopt(7);
    assert_pinned(&fx, &run, &head, 7);
    assert!(
        !marker.exists(),
        "a post-checkout hook ran for the adopt worktree"
    );
}

#[test]
fn author_executor_is_recorded_and_nothing_else_changes() {
    let fx = Fixture::new("\"alex\"");
    let head = ready(&fx, "feat/chat", 7);
    let run = fx.run_in(
        &fx.repo,
        &["board", "adopt", "7", "--author-executor", "codex"],
        None,
    );
    let id = assert_pinned(&fx, &run, &head, 7);
    assert_eq!(
        fx.pr_adopted(&id).declared_executor.as_deref(),
        Some("codex")
    );
}

#[test]
fn a_comment_that_fails_does_not_fail_the_adoption() {
    let fx = Fixture::new("\"alex\"");
    let head = ready(&fx, "feat/chat", 7);
    fx.comment_fails();
    let run = fx.adopt(7);
    assert_pinned(&fx, &run, &head, 7);
    assert_eq!(run.json["commented"], false);
    assert!(
        run.json["warning"].as_str().unwrap().contains("not posted"),
        "{}",
        run.stderr
    );
}

#[test]
fn a_second_adoption_of_the_same_pr_is_refused_and_leaves_no_ref_or_worktree() {
    let fx = Fixture::new("\"alex\"");
    let head = ready(&fx, "feat/chat", 7);
    let first = fx.adopt(7);
    let id = assert_pinned(&fx, &first, &head, 7);

    let second = fx.adopt(7);
    assert_ne!(second.code, 0, "{}", second.stderr);
    assert_eq!(second.json["status"], "refused", "{}", second.stderr);
    assert_eq!(second.json["reason"], "already_adopting");
    assert!(second.json["error"].is_null());

    assert_eq!(fx.aaf_refs(), [format!("refs/heads/aaf/{id}")]);
    assert_eq!(fx.worktree_dirs().len(), 1, "{:?}", fx.worktree_dirs());
    let registered = git(&fx.repo, &["worktree", "list", "--porcelain"]);
    assert_eq!(registered.matches("worktree ").count(), 2, "{registered}");
    assert_eq!(fx.tasks().len(), 1);
    assert_eq!(fx.events(&id).len(), 2);
    let comments = fx
        .gh_calls()
        .iter()
        .filter(|c| c.starts_with("pr comment"))
        .count();
    assert_eq!(comments, 1, "only the first adoption comments");
}

#[test]
fn a_pr_can_be_adopted_again_after_it_was_cancelled_or_done() {
    let fx = Fixture::new("\"alex\"");
    let head = ready(&fx, "feat/chat", 7);
    let first = assert_pinned(&fx, &fx.adopt(7), &head, 7);

    let cancelled = fx.board(&["cancel", &first, "--reason", "wrong PR"]);
    assert_eq!(cancelled.code, 0, "{}", cancelled.stderr);
    let second = assert_pinned(&fx, &fx.adopt(7), &head, 7);
    assert_ne!(first, second);

    {
        let db = fx.db();
        assert!(
            db.compare_and_set_state(&second, TaskState::Adopting, TaskState::Done, None, 60)
                .unwrap()
        );
    }
    let third = assert_pinned(&fx, &fx.adopt(7), &head, 7);
    assert!(![&first, &second].contains(&&third));
    let states: Vec<TaskState> = fx.tasks().iter().map(|t| t.state).collect();
    assert_eq!(
        states.iter().filter(|s| **s == TaskState::Adopting).count(),
        1,
        "{states:?}"
    );
}

#[test]
fn two_prs_are_two_adoptions() {
    let fx = Fixture::new("\"alex\"");
    let head7 = ready(&fx, "feat/one", 7);
    let a = assert_pinned(&fx, &fx.adopt(7), &head7, 7);
    let head8 = ready(&fx, "feat/two", 8);
    let b = assert_pinned(&fx, &fx.adopt(8), &head8, 8);
    assert_ne!(a, b);
    assert_eq!(fx.aaf_refs().len(), 2);
}

#[test]
fn a_store_that_refuses_the_insert_leaves_no_task_no_ref_and_no_worktree() {
    let fx = Fixture::new("\"alex\"");
    ready(&fx, "feat/chat", 7);
    drop(fx.db());
    // The task row goes in first; the second event then fails: the whole
    // transaction has to roll back, and the pin with it.
    let conn = rusqlite::Connection::open(fx.data.join("aaf.db")).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER refuse_pr_adopted BEFORE INSERT ON events \
         WHEN NEW.kind = 'pr_adopted' BEGIN SELECT RAISE(ABORT, 'store says no'); END;",
    )
    .unwrap();
    drop(conn);

    let run = fx.adopt(7);
    assert_eq!(run.code, 1, "{}", run.stderr);
    assert_eq!(run.json["status"], "error");
    assert!(
        run.json["error"]
            .as_str()
            .unwrap()
            .contains("store says no"),
        "{}",
        run.stderr
    );
    fx.assert_nothing_created("a failed insert");
    assert!(
        !fx.gh_calls().iter().any(|c| c.starts_with("pr comment")),
        "no comment for an adoption that did not happen"
    );
}

#[test]
fn an_agent_run_may_not_adopt_and_gh_is_never_called() {
    let fx = Fixture::new("\"alex\"");
    ready(&fx, "feat/chat", 7);
    let run = fx.run_in(&fx.repo, &["board", "adopt", "7"], Some("run-1"));
    assert_ne!(run.code, 0);
    assert_eq!(run.json["status"], "refused", "{}", run.stderr);
    assert!(run.json["ok"] == json!(false), "{}", run.stderr);
    assert!(fx.gh_calls().is_empty(), "{:?}", fx.gh_calls());
    assert!(!fx.db_exists(), "refused before the store was opened");
    fx.assert_nothing_created("an agent run");
}

fn add_events(fx: &Fixture, id: &str) {
    let db = fx.db();
    let at = fx.tasks().iter().find(|t| t.id == id).unwrap().created_at + 5;
    for (phase, retry, extra) in [
        ("verify", false, json!({})),
        ("review", false, json!({})),
        (
            "review",
            true,
            json!({ "code": "ci_pending", "reason": "checks still running", "retry_in_secs": 60 }),
        ),
    ] {
        let mut payload = json!({ "phase": phase, "attempt": 1, "retry": retry });
        payload
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        db.emit(&FactoryEvent::new(EventKind::AdoptStep, payload.to_string(), at).for_task(id))
            .unwrap();
    }
    db.set_retry_schedule(id, Some(i64::MAX / 4), 0, at)
        .unwrap();
}

#[test]
fn board_show_and_list_describe_an_adopting_task() {
    let fx = Fixture::new("\"alex\"");
    let head = ready(&fx, "feat/chat", 7);
    let id = assert_pinned(&fx, &fx.adopt(7), &head, 7);
    add_events(&fx, &id);

    let show = fx.board(&["show", &id]);
    assert_eq!(show.code, 0, "{}", show.stderr);
    let adopt = &show.json["adopt"];
    assert_eq!(adopt["pr_number"], 7);
    assert_eq!(adopt["head_sha"], head);
    assert_eq!(adopt["head_ref"], "feat/chat");
    assert_eq!(adopt["phase"], "review");
    assert_eq!(adopt["retries"], 1);
    assert_eq!(adopt["last_retry"]["code"], "ci_pending");
    assert!(adopt["retry_after"].as_i64().unwrap() > 0);
    assert!(adopt["abort"].is_null());

    let text = fx.board_text(&["show", &id]);
    assert!(text.contains("adopt: PR #7"), "{text}");
    assert!(
        text.contains(&format!("adopt pinned head: {head}")),
        "{text}"
    );
    assert!(text.contains("adopt phase: review"), "{text}");
    assert!(text.contains("adopt retries: 1, next turn in"), "{text}");
    assert!(text.contains("ci_pending: checks still running"), "{text}");

    let list = fx.board(&["list"]);
    assert_eq!(list.json["tasks"][0]["adopt"]["pr_number"], 7);
    assert_eq!(list.json["tasks"][0]["state"], "adopting");
    let list_text = fx.board_text(&["list"]);
    assert!(
        list_text.contains(&format!(
            "PR #7 head {} phase=review retry=1 retry_after=",
            &head[..12]
        )),
        "{list_text}"
    );
}

#[test]
fn board_show_gives_the_code_and_reason_of_a_cancelled_adoption() {
    let fx = Fixture::new("\"alex\"");
    let head = ready(&fx, "feat/chat", 7);
    let id = assert_pinned(&fx, &fx.adopt(7), &head, 7);
    fx.lane_cancels(&id, "head_changed", "the PR moved on");

    let show = fx.board(&["show", &id]);
    assert_eq!(show.json["task"]["state"], "cancelled");
    assert_eq!(show.json["adopt"]["abort"]["code"], "head_changed");
    assert_eq!(show.json["adopt"]["abort"]["reason"], "the PR moved on");
    assert!(
        show.json["adopt"]["phase"].is_null(),
        "no phase once it is over"
    );
    let text = fx.board_text(&["show", &id]);
    assert!(
        text.contains("adopt aborted: head_changed: the PR moved on"),
        "{text}"
    );
    assert!(fx.board_text(&["list"]).contains("aborted=head_changed"));
}

#[test]
fn a_user_cancel_shows_as_cancelled_by_user() {
    let fx = Fixture::new("\"alex\"");
    let head = ready(&fx, "feat/chat", 7);
    let id = assert_pinned(&fx, &fx.adopt(7), &head, 7);
    let cancel = fx.board(&["cancel", &id, "--reason", "not now"]);
    assert_eq!(cancel.code, 0, "{}", cancel.stderr);
    let show = fx.board(&["show", &id]);
    assert_eq!(show.json["adopt"]["abort"]["code"], "cancelled_by_user");
    assert_eq!(show.json["adopt"]["abort"]["reason"], "not now");
}

#[test]
fn an_ordinary_task_shows_no_adoption() {
    let fx = Fixture::new("\"alex\"");
    let added = fx.board(&["add", "--title", "plain", "--spec", "s", "--accept", "a"]);
    let id = added.json["task"]["id"].as_str().unwrap().to_string();
    let show = fx.board(&["show", &id]);
    assert!(show.json["adopt"].is_null());
    assert!(fx.board(&["list"]).json["tasks"][0].get("adopt").is_none());
    assert!(!fx.board_text(&["show", &id]).contains("adopt"));
}

#[test]
fn the_help_says_where_it_runs_and_what_always_waits_for_a_person() {
    let out = std::process::Command::new(support::AAF_BIN)
        .args(["board", "adopt", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&out.stdout).to_string();
    for needle in [
        "aaf run --auto",
        "draft",
        "adopt it again",
        ".autofactory/**",
        "[commands]",
        "--author-executor",
    ] {
        assert!(help.contains(needle), "help lacks {needle:?}:\n{help}");
    }
}
