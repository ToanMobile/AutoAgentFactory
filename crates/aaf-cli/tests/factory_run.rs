#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `aaf run` through the real binary: real store, real git worktrees, real
//! gate commands. The "agent" is a shell one-liner configured as a custom
//! executor; no `claude` process is ever started here.

#[path = "factory_support.rs"]
mod support;

use std::fs;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::sync::mpsc;
use std::time::Duration;

use aaf_domain::ports::{KvStore, META_FACTORY_FROZEN};
use serde_json::json;
use support::{Fixture, git};

/// Commits `.autofactory/config.toml` with a custom executor running
/// `script` (the prompt is its `$0`) and `test` as the scoped test command.
fn configure(fx: &Fixture, script: &str, test: &str) {
    let dir = fx.repo.join(".autofactory");
    fs::create_dir_all(&dir).unwrap();
    let config = format!(
        "[commands]\ntest_scoped = {}\n\n[run]\nexecutor = \"custom\"\n\
         executor_cmd = [\"/bin/sh\", \"-c\", {}, \"{{prompt}}\"]\nrun_timeout_secs = 60\n",
        json!(test),
        json!(script)
    );
    fs::write(dir.join("config.toml"), config).unwrap();
    git(&fx.repo, &["add", "-A"]);
    git(&fx.repo, &["commit", "--quiet", "-m", "configure aaf"]);
}

#[test]
fn a_ready_task_ends_as_a_gated_branch_with_a_receipt_in_review() {
    let fx = Fixture::new();
    configure(&fx, "echo made > done.txt", "test -f done.txt");
    let id = fx.add("make the file");

    let out = fx.aaf(&["run", "--unsandboxed"]);

    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["status"], json!("ok"));
    assert_eq!(out.json["review"], json!([id]));
    assert_eq!(out.json["agent_runs"], json!(1));
    assert_eq!(out.json["gate_runs"], json!(1));
    assert_eq!(out.json["errors"], json!([]));

    // The work is committed on the task branch, not on main.
    let branch = format!("aaf/{id}");
    assert_eq!(
        git(&fx.repo, &["show", &format!("{branch}:done.txt")]),
        "made"
    );
    assert!(!fx.repo.join("done.txt").exists());
    // The worktree is where the plan says, and its gate left a signed receipt.
    let tree = fx.worktrees.join(aaf_fp::repo_id(&fx.repo)).join(&id);
    assert!(tree.join("done.txt").is_file());
    assert!(aaf_fp::receipt_path(&fx.data, &tree).is_file());

    let shown = fx.show(&id);
    assert_eq!(shown["task"]["state"], json!("review"));
    assert_eq!(shown["runs"][0]["status"], json!("succeeded"));
    assert_eq!(shown["runs"][0]["branch"], json!(branch));
    let kinds: Vec<&str> = shown["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"gate_result"), "{kinds:?}");
    // No remote: the branch and the receipt are what is handed over.
    assert!(
        shown["events"][0]["payload"]["why"]
            .as_str()
            .unwrap()
            .contains("no pull request")
    );

    // Nothing is Ready any more: a second run has nothing to do.
    let again = fx.aaf(&["run", "--unsandboxed"]);
    assert_eq!(again.json["agent_runs"], json!(0), "{}", again.stderr);
}

#[test]
fn auto_merge_lands_the_task_on_the_base_branch() {
    let fx = Fixture::new();
    configure(&fx, "echo made > done.txt", "test -f done.txt");
    let id = fx.add("make the file");

    let out = fx.aaf(&["run", "--unsandboxed", "--auto-merge"]);

    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["done"], json!([id]), "{}", out.stderr);
    assert_eq!(fx.state(&id), "done");
    assert_eq!(
        fs::read_to_string(fx.repo.join("done.txt")).unwrap(),
        "made\n"
    );
    assert_eq!(git(&fx.repo, &["status", "--porcelain"]), "");
    // A done task gives its worktree and branch back.
    let tree = fx.worktrees.join(aaf_fp::repo_id(&fx.repo)).join(&id);
    assert!(!tree.exists(), "{}", out.stderr);
    assert_eq!(git(&fx.repo, &["branch", "--list", "aaf/*"]), "");
}

#[test]
fn a_task_that_breaks_the_gate_goes_back_with_a_note_and_the_others_pass() {
    let fx = Fixture::new();
    // The task whose prompt mentions BREAK writes the file the gate forbids.
    let script = "case \"$0\" in *BREAK*) echo x > broken.txt;; *) echo ok > \"ok-$$.txt\";; esac";
    configure(&fx, script, "test ! -f broken.txt");
    let good = fx.add("a good one");
    let bad = fx.add("BREAK the build");
    let also_good = fx.add("another good one");

    let out = fx.aaf(&["run", "--unsandboxed", "--max-parallel", "3"]);

    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["requeued"], json!([bad]), "{}", out.stderr);
    assert_eq!(out.json["errors"], json!([]), "{}", out.stderr);
    assert_eq!(out.json["blocked"], json!([]), "{}", out.stderr);
    assert_eq!(fx.state(&good), "review");
    assert_eq!(fx.state(&also_good), "review");
    let shown = fx.show(&bad);
    assert_eq!(
        shown["task"]["state"],
        json!("ready"),
        "back for a retry, not blocked"
    );
    let note = shown["task"]["context_notes"][0].as_str().unwrap();
    assert!(note.starts_with("unblock[T3] digest="), "{note}");
    assert!(note.contains("gate_failed"), "{note}");

    // The retry runs in the same worktree and fails the same way. The
    // "agent" proposes no split, so the ladder ends with a human.
    let out = fx.aaf(&["run", "--unsandboxed"]);
    assert_eq!(out.json["agent_runs"], json!(1), "{}", out.stderr);
    assert_eq!(out.json["errors"], json!([]), "{}", out.stderr);
    assert_eq!(out.json["blocked"], json!([bad]), "{}", out.stderr);
    let shown = fx.show(&bad);
    assert_eq!(
        shown["task"]["blocked_reason"]["kind"],
        json!("needs_human")
    );
    assert_eq!(
        shown["task"]["context_notes"][0],
        json!(note),
        "marker kept"
    );
    let runs = shown["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 3, "2 impl runs + 1 T4 proposer run: {runs:?}");
    assert_eq!(
        runs.iter()
            .filter(|r| r["executor"] == json!("unblock-proposer"))
            .count(),
        1
    );

    // A human looks at it and sends it back.
    assert_eq!(fx.aaf(&["board", "unblock", &bad]).code, 0);
    assert_eq!(fx.state(&bad), "ready");
}

#[test]
fn a_frozen_factory_refuses_to_start() {
    let fx = Fixture::new();
    configure(&fx, "echo made > done.txt", "true");
    let id = fx.add("make the file");
    fx.db().set_meta(META_FACTORY_FROZEN, "1").unwrap();

    let out = fx.aaf(&["run", "--unsandboxed"]);

    assert_eq!(out.code, 2, "{}", out.stderr);
    assert_eq!(out.json["status"], json!("frozen"));
    assert_eq!(fx.state(&id), "ready");
    assert_eq!(fx.show(&id)["runs"], json!([]));

    assert_eq!(fx.aaf(&["board", "unfreeze"]).code, 0);
    let out = fx.aaf(&["run", "--unsandboxed"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(fx.state(&id), "review");
}

#[test]
fn run_is_refused_inside_an_agent_run_and_while_another_run_holds_the_repository() {
    let fx = Fixture::new();
    configure(&fx, "echo made > done.txt", "true");
    let id = fx.add("make the file");

    let nested = fx.aaf_in(
        &fx.repo,
        &["run", "--unsandboxed"],
        &[("AAF_RUN_ID", "run-1")],
    );
    assert_eq!(nested.code, 2, "{}", nested.stderr);
    assert_eq!(nested.json["status"], json!("refused"));

    let locks = fx.data.join("locks");
    fs::create_dir_all(&locks).unwrap();
    let lock_path = locks.join(format!("{}.run.lock", aaf_fp::repo_id(&fx.repo)));
    let lock = File::create(&lock_path).unwrap();
    lock.try_lock().unwrap();
    let second = fx.aaf(&["run", "--unsandboxed"]);
    assert_eq!(second.code, 2, "{}", second.stderr);
    assert_eq!(second.json["status"], json!("refused"));
    assert_eq!(fx.state(&id), "ready");

    drop(lock);
    assert_eq!(fx.aaf(&["run", "--unsandboxed"]).code, 0);
    assert_eq!(fx.state(&id), "review");
}

#[test]
fn run_shows_on_stderr_that_its_gate_waits_for_a_heavy_slot() {
    let fx = Fixture::new();
    configure(&fx, "echo made > done.txt", "test -f done.txt");
    let id = fx.add("make the file");
    // A gate of another repository holds the machine's first heavy slot.
    let slot = aaf_engine::throttle::slots::slot_path(&fx.data, 0);
    fs::create_dir_all(slot.parent().unwrap()).unwrap();
    let other = File::create(&slot).unwrap();
    other.try_lock().unwrap();

    let mut child = fx
        .command(&fx.repo, &["run", "--unsandboxed"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = BufReader::new(child.stderr.take().unwrap());
    let (tx, lines) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in stderr.lines().map_while(Result::ok) {
            // The receiver stops listening after the first line.
            let _ = tx.send(line);
        }
    });
    // The run cannot get past the gate before the slot is free, so the
    // line is there before the slot is given up. The timeout only ends a
    // run that never says anything.
    let first = lines.recv_timeout(Duration::from_secs(120));
    other.unlock().unwrap();
    let out = child.wait_with_output().unwrap();
    reader.join().unwrap();

    let first = first.expect("aaf run printed nothing while its gate waited");
    assert!(
        first.starts_with(&format!(
            "aaf run: gate {id}: test_scoped: waiting for a heavy slot"
        )),
        "{first}"
    );
    assert_eq!(out.status.code(), Some(0));
    // stdout stays the one JSON object.
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["review"], json!([id]));
}

#[test]
fn a_missing_test_command_blocks_the_task_for_a_human_instead_of_passing() {
    let fx = Fixture::new();
    configure(&fx, "echo made > done.txt", "");
    let id = fx.add("make the file");

    let out = fx.aaf(&["run", "--unsandboxed"]);

    // Was exit 0 / `ok: true`: a caller could not tell this run from one
    // that left everything in Review (PLAN C.10: 2 = Blocked).
    assert_eq!(out.code, 2, "{}", out.stderr);
    assert_eq!(out.json["ok"], json!(false));
    assert_eq!(out.json["status"], json!("blocked"));
    assert_eq!(out.json["blocked"], json!([id]));
    let shown = fx.show(&id);
    assert_eq!(
        shown["task"]["blocked_reason"]["kind"],
        json!("needs_human")
    );
    assert!(
        shown["task"]["blocked_reason"]["why"]
            .as_str()
            .unwrap()
            .contains("could not verify")
    );
}

#[test]
fn run_outside_a_git_repository_is_an_error() {
    let fx = Fixture::new();
    let out = fx.aaf_in(&fx.root().join("home"), &["run", "--unsandboxed"], &[]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert_eq!(out.json["status"], json!("error"));
}
