#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `aaf run --auto` through the real binary, the way an unattended daemon runs
//! it: it keeps polling for new Ready tasks, and after it is killed mid-task a
//! fresh `--auto` run reaps the interrupted turn and finishes the task. The
//! "agent" is a shell one-liner configured as a custom executor.

#[path = "factory_support.rs"]
mod support;

use std::fs;
use std::path::Path;
use std::process::{Child, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde_json::json;
use support::{Fixture, git};

/// Long enough for `--auto` to notice a new task (it polls every 5 s) and run
/// the agent and the gate, short enough to fail fast on a hang.
const WAIT: Duration = Duration::from_secs(90);

fn configure(fx: &Fixture, script: &str, test: &str) {
    let dir = fx.repo.join(".autofactory");
    fs::create_dir_all(&dir).unwrap();
    let config = format!(
        "[commands]\ntest_scoped = {}\n\n[run]\nexecutor = \"custom\"\n\
         executor_cmd = [\"/bin/sh\", \"-c\", {}, \"{{prompt}}\"]\nrun_timeout_secs = 120\n",
        json!(test),
        json!(script)
    );
    fs::write(dir.join("config.toml"), config).unwrap();
    git(&fx.repo, &["add", "-A"]);
    git(&fx.repo, &["commit", "--quiet", "-m", "configure aaf"]);
}

/// Kills the daemon when a test ends, even on a failed assertion.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_auto(fx: &Fixture, log: &Path) -> Daemon {
    let file = fs::File::create(log).unwrap();
    let child = fx
        .command(&fx.repo, &["run", "--auto", "--unsandboxed"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(file)
        .spawn()
        .unwrap();
    Daemon(child)
}

/// Polls `aaf board show` until `pred` holds for the task's state.
fn wait_until(fx: &Fixture, id: &str, log: &Path, what: &str, pred: impl Fn(&str) -> bool) {
    let start = Instant::now();
    loop {
        let state = fx.state(id);
        if pred(&state) {
            return;
        }
        assert!(
            start.elapsed() < WAIT,
            "{id} never became {what} (last state {state}); daemon log:\n{}",
            fs::read_to_string(log).unwrap_or_default()
        );
        sleep(Duration::from_millis(250));
    }
}

#[test]
fn auto_keeps_picking_up_tasks_added_while_it_runs() {
    let fx = Fixture::new();
    configure(&fx, "echo made > \"made-$$.txt\"", "ls made-*.txt");
    let log = fx.root().join("auto.log");
    let first = fx.add("make a file");

    let _daemon = spawn_auto(&fx, &log);
    wait_until(&fx, &first, &log, "review", |s| s == "review");

    // Added after the daemon went idle: no restart, no operator.
    let second = fx.add("make another file");
    wait_until(&fx, &second, &log, "review", |s| s == "review");

    for id in [&first, &second] {
        let shown = fx.show(id);
        assert_eq!(shown["runs"][0]["status"], json!("succeeded"), "{shown}");
    }
}

#[test]
fn a_killed_auto_run_is_resumed_by_the_next_one() {
    let fx = Fixture::new();
    // The first turn marks itself and hangs (as if the machine died mid-run);
    // every later turn does the work at once.
    let mark = fx.root().join("first-turn-started");
    let script = format!(
        "if [ -f {m} ]; then echo made > done.txt; else touch {m}; sleep 60; fi",
        m = mark.display()
    );
    configure(&fx, &script, "test -f done.txt");
    let id = fx.add("make the file");

    let log = fx.root().join("auto-1.log");
    let daemon = spawn_auto(&fx, &log);
    wait_until(&fx, &id, &log, "running", |s| s == "running");
    let start = Instant::now();
    while !mark.exists() {
        assert!(start.elapsed() < WAIT, "the first agent turn never started");
        sleep(Duration::from_millis(100));
    }
    // SIGKILL: no clean shutdown, the task is left `running` in the store.
    drop(daemon);
    assert_eq!(fx.state(&id), "running");

    let log2 = fx.root().join("auto-2.log");
    let _daemon = spawn_auto(&fx, &log2);
    wait_until(&fx, &id, &log2, "review", |s| s == "review");

    let shown = fx.show(&id);
    let runs = shown["runs"].as_array().unwrap();
    assert!(runs.len() >= 2, "the interrupted turn is recorded: {shown}");
    assert!(
        runs.iter().any(|r| r["status"] == json!("succeeded")),
        "{shown}"
    );
    assert_eq!(
        git(&fx.repo, &["show", &format!("aaf/{id}:done.txt")]),
        "made"
    );
}
