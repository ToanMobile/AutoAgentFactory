#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `aaf` against a store holding an `Adopting` task (inserted directly, as the
//! adopt command will): every by-id write refuses it and leaves the store as it
//! was, `board cancel` still works, and the generic board moves do not reach it.

#[path = "factory_support.rs"]
mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;

use aaf_domain::event::EventKind;
use aaf_domain::ports::{
    EventSink, FingerprintStore, KvStore, META_CONSECUTIVE_ROOT_FAILS, TaskStore,
};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use serde_json::json;
use support::Fixture;

const FINGERPRINT: &str = "fp-adopted";

/// An adopted PR for `fx`'s repository, with `consecutive_root_fails = 3`.
fn adopt(fx: &Fixture, id: &str) {
    let db = fx.db();
    let spec = ImmutableSpec {
        description: "adopted pull request".into(),
        acceptance: vec!["gate and dual review pass".into()],
    };
    let mut task = Task::new_root(
        id,
        fx.repo.to_str().unwrap(),
        "adopted",
        spec,
        Priority::P1,
        10,
    );
    task.state = TaskState::Adopting;
    task.fingerprint = Some(FINGERPRINT.into());
    db.insert_task(&task).unwrap();
    db.emit(&aaf_domain::event::FactoryEvent::new(EventKind::PrAdopted, "{}", 11).for_task(id))
        .unwrap();
    db.set_meta(META_CONSECUTIVE_ROOT_FAILS, "3").unwrap();
}

/// What a refused command must leave alone.
#[derive(Debug, PartialEq)]
struct Snapshot {
    state: TaskState,
    updated_at: i64,
    closed_fingerprint: bool,
    root_fails: Option<String>,
    events: usize,
    tasks: usize,
    deps: Vec<(String, String)>,
    notes: Vec<String>,
}

fn snapshot(fx: &Fixture, id: &str) -> Snapshot {
    let db = fx.db();
    let task = db.get_task(id).unwrap();
    Snapshot {
        state: task.state,
        updated_at: task.updated_at,
        closed_fingerprint: db
            .find_closed_since(FINGERPRINT, fx.repo.to_str().unwrap(), 0)
            .unwrap()
            .is_some(),
        root_fails: db.get_meta(META_CONSECUTIVE_ROOT_FAILS).unwrap(),
        events: db.events_for_task(id, 100).unwrap().len(),
        tasks: db.list_tasks(None, None).unwrap().len(),
        deps: db.all_deps().unwrap(),
        notes: task.context_notes,
    }
}

#[test]
fn board_done_refuses_adopting_even_with_force_and_changes_nothing() {
    let fx = Fixture::new();
    adopt(&fx, "t-ad");
    let before = snapshot(&fx, "t-ad");
    assert_eq!(before.root_fails.as_deref(), Some("3"));

    for force in [false, true] {
        let mut args = vec!["board", "done", "t-ad", "--reason", "merged by hand"];
        if force {
            args.push("--force");
        }
        let out = fx.aaf(&args);
        assert_eq!(out.code, 2, "force={force}: {}", out.stderr);
        assert_eq!(out.json["status"], json!("refused"), "{}", out.stderr);
        assert_eq!(out.json["state"], json!("adopting"), "{}", out.stderr);
        assert_eq!(snapshot(&fx, "t-ad"), before, "force={force}");
    }
}

#[test]
fn merge_task_refuses_adopting_before_any_gh_call() {
    let fx = Fixture::new();
    adopt(&fx, "t-ad");
    let before = snapshot(&fx, "t-ad");

    // A `gh` that records every call: the refusal must come first.
    let bin = fx.root().join("fake-bin");
    fs::create_dir_all(&bin).unwrap();
    let marker = fx.root().join("gh-was-called");
    let gh = bin.join("gh");
    fs::write(
        &gh,
        format!("#!/bin/sh\necho \"$@\" >> '{}'\nexit 1\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let out = fx.aaf_in(
        &fx.repo,
        &["merge", "--task", "t-ad", "--dry-run"],
        &[("PATH", &path)],
    );
    assert_eq!(out.code, 1, "{}", out.stderr);
    let error = out.json["error"].as_str().unwrap_or_default();
    assert!(error.contains("adopting"), "{}", out.stderr);
    assert!(!marker.exists(), "gh ran before the refusal");
    assert_eq!(snapshot(&fx, "t-ad"), before);
}

#[test]
fn steer_refuses_adopting_and_a_ready_task_still_takes_a_note() {
    let fx = Fixture::new();
    adopt(&fx, "t-ad");
    let ready = fx.add("ordinary");
    let before = snapshot(&fx, "t-ad");

    let out = fx.aaf(&["board", "steer", "t-ad", "also fix the typo"]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(
        out.json["error"].as_str().unwrap().contains("adopting"),
        "{}",
        out.stderr
    );
    assert_eq!(snapshot(&fx, "t-ad"), before);

    let ok = fx.aaf(&["board", "steer", &ready, "prefer the small fix"]);
    assert_eq!(ok.code, 0, "{}", ok.stderr);
}

#[test]
fn dep_add_and_rm_refuse_when_either_end_is_adopting() {
    let fx = Fixture::new();
    adopt(&fx, "t-ad");
    let ready = fx.add("ordinary");
    let before = snapshot(&fx, "t-ad");

    for (verb, task, blocker) in [
        ("add", "t-ad", ready.as_str()),
        ("add", ready.as_str(), "t-ad"),
        ("rm", "t-ad", ready.as_str()),
        ("rm", ready.as_str(), "t-ad"),
    ] {
        let out = fx.aaf(&["board", "dep", verb, task, "--blocked-by", blocker]);
        assert_eq!(out.code, 1, "{verb} {task} {blocker}: {}", out.stderr);
        assert!(
            out.json["error"].as_str().unwrap().contains("adopting"),
            "{verb} {task} {blocker}: {}",
            out.stderr
        );
    }
    assert_eq!(snapshot(&fx, "t-ad"), before);
}

#[test]
fn board_add_blocked_by_an_adopting_task_is_refused_and_leaves_no_task_behind() {
    let fx = Fixture::new();
    adopt(&fx, "t-ad");
    let before = snapshot(&fx, "t-ad");

    let out = fx.aaf(&[
        "board",
        "add",
        "--title",
        "after the adopted PR",
        "--spec",
        "s",
        "--accept",
        "a",
        "--blocked-by",
        "t-ad",
    ]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(
        out.json["error"].as_str().unwrap().contains("adopting"),
        "{}",
        out.stderr
    );
    // A task row left behind without its blocker would run unblocked.
    assert_eq!(snapshot(&fx, "t-ad"), before);
    assert_eq!(snapshot(&fx, "t-ad").tasks, 1);
}

#[test]
fn board_cancel_stops_an_adopting_task() {
    let fx = Fixture::new();
    adopt(&fx, "t-ad");

    let out = fx.aaf(&["board", "cancel", "t-ad", "--reason", "chat pushed a fix"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let after = snapshot(&fx, "t-ad");
    assert_eq!(after.state, TaskState::Cancelled);
    // Not a failed root, and nothing closed.
    assert_eq!(after.root_fails.as_deref(), Some("3"));
    assert!(!after.closed_fingerprint);
}

#[test]
fn generic_board_moves_do_not_reach_an_adopting_task() {
    let fx = Fixture::new();
    adopt(&fx, "t-ad");
    let before = snapshot(&fx, "t-ad");

    for args in [
        vec!["board", "unblock", "t-ad"],
        vec!["board", "ready", "t-ad"],
        vec!["board", "rework", "t-ad", "--reason", "again"],
        vec!["board", "rework", "t-ad", "--reason", "again", "--rerun"],
        vec!["board", "approve-plan", "t-ad"],
        vec!["board", "split", "t-ad", "--dry-run", "--unsandboxed"],
    ] {
        let out = fx.aaf(&args);
        assert_ne!(out.code, 0, "{args:?}: {}", out.stderr);
        assert_eq!(snapshot(&fx, "t-ad"), before, "{args:?}");
    }
    // `unblock --all` and `ready --all` select by state: still nothing.
    for args in [["board", "unblock", "--all"], ["board", "ready", "--all"]] {
        let out = fx.aaf(&args);
        assert_eq!(out.code, 0, "{args:?}: {}", out.stderr);
        assert_eq!(snapshot(&fx, "t-ad"), before, "{args:?}");
    }
}

#[test]
fn board_list_and_show_report_the_adopting_state() {
    let fx = Fixture::new();
    adopt(&fx, "t-ad");
    fx.add("ordinary");

    let list = fx.aaf(&["board", "list", "--state", "adopting"]);
    assert_eq!(list.code, 0, "{}", list.stderr);
    assert_eq!(list.json["count"], json!(1));
    assert_eq!(list.json["tasks"][0]["id"], json!("t-ad"));
    assert_eq!(fx.state("t-ad"), "adopting");
}
