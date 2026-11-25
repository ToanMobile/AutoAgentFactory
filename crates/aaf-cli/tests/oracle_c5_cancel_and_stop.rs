#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Oracle tests for Package C5:
//! - C5a: `aaf board cancel <id> [--reason ...] [--cascade]`, DAG warnings,
//!   and `aaf gc --include-cancelled` cleanup of worktree + branch + snapshots.
//! - C5b: cooperative graceful stop via `aaf run --stop`.

#[path = "factory_support.rs"]
mod support;

use std::fs;
use std::process::{Child, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use aaf_domain::blocked::BlockedReason;
use aaf_domain::ports::{RunStore, TaskStore};
use aaf_domain::run::{RunRecord, RunStatus};
use aaf_domain::task::{ImmutableSpec, Task, TaskState};
use aaf_engine::git::worktree;
use serde_json::{Value, json};
use support::{Fixture, git};

const WAIT: Duration = Duration::from_secs(60);

struct ChildGuard(Option<Child>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn configure_executor(fx: &Fixture, script: &str, test_cmd: &str) {
    let dir = fx.repo.join(".autofactory");
    fs::create_dir_all(&dir).unwrap();
    let config = format!(
        "[commands]\ntest_scoped = {}\n\n[run]\nexecutor = \"custom\"\n\
         executor_cmd = [\"/bin/sh\", \"-c\", {}, \"{{prompt}}\"]\n\
         max_parallel = 1\nrun_timeout_secs = 120\n",
        json!(test_cmd),
        json!(script)
    );
    fs::write(dir.join("config.toml"), config).unwrap();
    git(&fx.repo, &["add", "-A"]);
    git(&fx.repo, &["commit", "--quiet", "-m", "configure aaf"]);
}

fn ref_exists(repo: &std::path::Path, full_ref: &str) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["show-ref", "--verify", "--quiet", full_ref])
        .status()
        .unwrap()
        .success()
}

#[test]
fn cancel_blocked_task_reports_dependents_and_unlocks_gc_cleanup() {
    let fx = Fixture::new();
    let blocked_id = fx.add("upstream blocked task");
    let dep_out = fx.aaf(&[
        "board",
        "add",
        "--title",
        "downstream dependent",
        "--spec",
        "wait on upstream",
        "--accept",
        "done",
        "--blocked-by",
        &blocked_id,
    ]);
    assert_eq!(dep_out.code, 0, "{}", dep_out.stderr);
    let dep_id = dep_out.json["task"]["id"].as_str().unwrap().to_string();

    {
        let db = fx.db();
        db.set_state(
            &blocked_id,
            TaskState::Blocked,
            Some(&BlockedReason::NeedsHuman {
                why: "obsolete".to_string(),
            }),
            1_700_000_010,
        )
        .unwrap();
    }

    let wt = worktree::create(&fx.repo, &fx.worktrees, &blocked_id, "main").unwrap();
    let head_oid = git(&wt.path, &["rev-parse", "HEAD"]);
    let snap_ref = format!("refs/aaf/snapshots/{blocked_id}/snap-1");
    git(&fx.repo, &["update-ref", &snap_ref, &head_oid]);
    assert!(wt.path.is_dir());
    assert!(ref_exists(
        &fx.repo,
        &format!("refs/heads/aaf/{blocked_id}")
    ));
    assert!(ref_exists(&fx.repo, &snap_ref));

    let cancel = fx.aaf(&[
        "board",
        "cancel",
        &blocked_id,
        "--reason",
        "superseded by new design",
    ]);
    assert_eq!(cancel.code, 0, "{}", cancel.stderr);
    assert_eq!(cancel.json["ok"], json!(true), "{}", cancel.json);
    assert_eq!(cancel.json["command"], json!("board"), "{}", cancel.json);
    assert_eq!(cancel.json["status"], json!("cancelled"), "{}", cancel.json);
    assert_eq!(cancel.json["task_id"], json!(blocked_id), "{}", cancel.json);
    assert_eq!(
        cancel.json["cancelled_ids"],
        json!([blocked_id]),
        "{}",
        cancel.json
    );
    assert_eq!(
        cancel.json["affected_dependents"],
        json!([dep_id]),
        "{}",
        cancel.json
    );
    assert!(
        cancel.json["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap_or("").contains(&dep_id)),
        "expected warning naming affected dependent: {}",
        cancel.json
    );
    assert_eq!(fx.state(&blocked_id), "cancelled");

    // `aaf board graph` should flag the dependent as cascade-blocked by the cancelled upstream.
    let graph = fx.aaf(&["board", "graph"]);
    assert_eq!(graph.code, 0, "{}", graph.stderr);
    assert!(
        graph.json["blocked_upstream"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["task_id"] == json!(dep_id) && row["blocked_by"] == json!(blocked_id)),
        "graph must flag cancelled upstream blocker: {}",
        graph.json
    );

    let gc = fx.aaf(&["gc", "--include-cancelled"]);
    assert_eq!(gc.code, 0, "{}", gc.stderr);
    assert_eq!(gc.json["ok"], json!(true), "{}", gc.json);
    assert!(!wt.path.exists(), "worktree should be removed by gc");
    assert!(
        !ref_exists(&fx.repo, &format!("refs/heads/aaf/{blocked_id}")),
        "branch should be removed by gc"
    );
    assert!(
        !ref_exists(&fx.repo, &snap_ref),
        "snapshot ref should be removed by gc"
    );
}

#[test]
fn cancel_refuses_running_task_running_run_and_agent_env() {
    let fx = Fixture::new();
    let running_id = fx.add("currently running task");
    let run_id_task = fx.add("task with active run record");
    let ready_id = fx.add("ready task refused inside agent run");

    {
        let db = fx.db();
        db.set_state(&running_id, TaskState::Running, None, 1_700_000_010)
            .unwrap();
        db.insert_run(&RunRecord {
            id: "run-live-1".to_string(),
            task_id: run_id_task.clone(),
            account_id: None,
            executor: "custom".to_string(),
            worktree_path: "/tmp/wt".to_string(),
            branch: format!("aaf/{run_id_task}"),
            status: RunStatus::Running,
            exit_code: None,
            tokens_in: 0,
            tokens_out: 0,
            started_at: 1_700_000_010,
            ended_at: None,
        })
        .unwrap();
    }

    let res_running = fx.aaf(&["board", "cancel", &running_id]);
    assert_eq!(res_running.code, 2, "{}", res_running.stderr);
    assert_eq!(
        res_running.json["status"],
        json!("refused"),
        "{}",
        res_running.json
    );
    assert_eq!(
        res_running.json["command"],
        json!("board"),
        "{}",
        res_running.json
    );
    assert_eq!(fx.state(&running_id), "running");

    let res_live_run = fx.aaf(&["board", "cancel", &run_id_task]);
    assert_eq!(res_live_run.code, 2, "{}", res_live_run.stderr);
    assert_eq!(
        res_live_run.json["status"],
        json!("refused"),
        "{}",
        res_live_run.json
    );
    assert_eq!(
        res_live_run.json["command"],
        json!("board"),
        "{}",
        res_live_run.json
    );
    assert_eq!(fx.state(&run_id_task), "ready");

    let res_agent = fx.aaf_in(
        &fx.repo,
        &["board", "cancel", &ready_id],
        &[("AAF_RUN_ID", "r1")],
    );
    assert_eq!(res_agent.code, 2, "{}", res_agent.stderr);
    assert_eq!(
        res_agent.json["status"],
        json!("refused"),
        "{}",
        res_agent.json
    );
    assert_eq!(
        res_agent.json["command"],
        json!("board"),
        "{}",
        res_agent.json
    );
    assert_eq!(fx.state(&ready_id), "ready");
}

#[test]
fn cancel_cascade_requires_flag_for_unfinished_children_and_warns_on_subtask() {
    let fx = Fixture::new();
    let parent_id = fx.add("split parent task");
    let child_id = format!("{parent_id}-sub1");
    let sibling_id = format!("{parent_id}-sub2");

    {
        let db = fx.db();
        let parent = db.get_task(&parent_id).unwrap();
        let spec = ImmutableSpec {
            description: "subtask work".to_string(),
            acceptance: vec!["passes".to_string()],
        };
        let c1 =
            Task::new_subtask(&child_id, &parent, "child 1", spec.clone(), 1_700_000_020).unwrap();
        let c2 = Task::new_subtask(&sibling_id, &parent, "child 2", spec, 1_700_000_021).unwrap();
        db.insert_task(&c1).unwrap();
        db.insert_task(&c2).unwrap();
        db.add_dep(&parent_id, &child_id).unwrap();
        db.add_dep(&parent_id, &sibling_id).unwrap();
    }

    // Without `--cascade`, cancelling a parent with unfinished children is refused.
    let no_cascade = fx.aaf(&["board", "cancel", &parent_id]);
    assert_eq!(no_cascade.code, 2, "{}", no_cascade.stderr);
    assert_eq!(
        no_cascade.json["status"],
        json!("refused"),
        "{}",
        no_cascade.json
    );
    assert_eq!(
        no_cascade.json["command"],
        json!("board"),
        "{}",
        no_cascade.json
    );
    assert_eq!(fx.state(&parent_id), "ready");
    assert_eq!(fx.state(&child_id), "ready");

    // Cancelling a single child warns that the split parent will not auto-complete.
    let cancel_child = fx.aaf(&["board", "cancel", &sibling_id]);
    assert_eq!(cancel_child.code, 0, "{}", cancel_child.stderr);
    assert_eq!(cancel_child.json["status"], json!("cancelled"));
    assert!(
        cancel_child.json["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w
                .as_str()
                .unwrap_or("")
                .contains("try_auto_complete_split_parent")),
        "expected split-parent warning: {}",
        cancel_child.json
    );
    assert_eq!(fx.state(&sibling_id), "cancelled");

    // With `--cascade`, cancelling the parent cancels both parent and remaining unfinished child.
    let cascade = fx.aaf(&["board", "cancel", &parent_id, "--cascade"]);
    assert_eq!(cascade.code, 0, "{}", cascade.stderr);
    assert_eq!(cascade.json["status"], json!("cancelled"));
    let cancelled: Vec<&str> = cascade.json["cancelled_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(cancelled.contains(&parent_id.as_str()), "{:?}", cancelled);
    assert!(cancelled.contains(&child_id.as_str()), "{:?}", cancelled);
    assert_eq!(fx.state(&parent_id), "cancelled");
    assert_eq!(fx.state(&child_id), "cancelled");
}

#[test]
fn run_stop_drains_in_flight_task_and_leaves_second_task_ready() {
    let fx = Fixture::new();
    let stop_file = fx
        .data
        .join("locks")
        .join(format!("{}.stop", aaf_fp::repo_id(&fx.repo)));

    // Calling `--stop` while nothing is running must succeed without creating `.stop`.
    let idle_stop = fx.aaf(&["run", "--stop"]);
    assert_eq!(idle_stop.code, 0, "{}", idle_stop.stderr);
    assert_eq!(idle_stop.json["status"], json!("ok"), "{}", idle_stop.json);
    assert_eq!(
        idle_stop.json["running"],
        json!(false),
        "{}",
        idle_stop.json
    );
    assert!(
        !stop_file.exists(),
        "must not create .stop when no aaf run holds .run.lock"
    );

    let marker = fx.root().join("task1-started");
    let script = format!(
        "touch {m}; echo ok > work.txt; sleep 1.2",
        m = marker.display()
    );
    configure_executor(&fx, &script, "test -f work.txt");

    let t1 = fx.add("first task");
    let t2 = fx.add("second task");

    let child = fx
        .command(&fx.repo, &["run", "--auto", "--unsandboxed"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut daemon = ChildGuard(Some(child));

    let start = Instant::now();
    while !marker.exists() {
        assert!(
            start.elapsed() < WAIT,
            "task 1 never started; t1={}, t2={}",
            fx.state(&t1),
            fx.state(&t2)
        );
        sleep(Duration::from_millis(25));
    }

    let stop_res = fx.aaf(&["run", "--stop"]);
    assert_eq!(stop_res.code, 0, "{}", stop_res.stderr);
    assert_eq!(stop_res.json["ok"], json!(true), "{}", stop_res.json);
    assert_eq!(stop_res.json["status"], json!("ok"), "{}", stop_res.json);
    assert_eq!(
        stop_res.json["stop_requested"],
        json!(true),
        "{}",
        stop_res.json
    );

    let child = daemon.0.take().unwrap();
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "aaf run should exit 0 on graceful stop; stderr={stderr}\nstdout={stdout}"
    );
    let run_json: Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("invalid JSON from aaf run: {e}\nstdout={stdout}\nstderr={stderr}")
    });
    assert_eq!(run_json["ok"], json!(true), "{run_json}");
    assert_eq!(run_json["status"], json!("ok"), "{run_json}");
    assert_eq!(run_json["stopped"], json!(true), "{run_json}");
    assert!(run_json["halted"].is_null(), "must not halt: {run_json}");
    assert!(
        !stop_file.exists(),
        "aaf run must clean up .stop before exiting"
    );

    let s1 = fx.state(&t1);
    let s2 = fx.state(&t2);
    assert!(
        matches!(s1.as_str(), "gating" | "review" | "done"),
        "task 1 should have progressed past ready/running, got {s1}"
    );
    assert_eq!(s2, "ready", "task 2 must remain ready after graceful stop");
}
