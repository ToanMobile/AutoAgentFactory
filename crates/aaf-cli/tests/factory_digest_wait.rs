#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `aaf run` through the real binary: a gate failure that waited for a heavy
//! slot the first time and not the second is still the same failure. Seen as
//! a flake of `factory_run::a_task_that_breaks_the_gate_goes_back_with_a_note_
//! and_the_others_pass` under load: the throttle's wait note was part of the
//! failure digest, so the second run looked like a new failure and the task
//! was requeued instead of blocked. Here the wait is made to happen.

#[path = "factory_support.rs"]
mod support;

#[path = "cli_support.rs"]
mod cli_support;

use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::json;
use support::{Fixture, git};

#[test]
fn a_gate_failure_that_waited_for_a_slot_once_is_the_same_failure_the_next_time() {
    if !cli_support::memory_is_calm("a_gate_failure_that_waited_for_a_slot_once") {
        return;
    }
    let fx = Fixture::new();
    // One heavy slot, so a slot held by the test makes the gate wait.
    fs::write(
        fx.root().join("home/.config/autofactory/config.toml"),
        "[machine]\nmax_load_percent = 0\nmax_heavy_parallel = 1\n",
    )
    .unwrap();
    let dir = fx.repo.join(".autofactory");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("config.toml"),
        "[commands]\ntest_scoped = \"test ! -f broken.txt\"\n\n[run]\nexecutor = \"custom\"\n\
         executor_cmd = [\"/bin/sh\", \"-c\", \"echo x > broken.txt\", \"{prompt}\"]\n\
         run_timeout_secs = 60\n",
    )
    .unwrap();
    git(&fx.repo, &["add", "-A"]);
    git(&fx.repo, &["commit", "--quiet", "-m", "configure aaf"]);
    let bad = fx.add("BREAK the build");

    // First run: another gate holds the only heavy slot.
    let slot = aaf_engine::throttle::slots::slot_path(&fx.data, 0);
    fs::create_dir_all(slot.parent().unwrap()).unwrap();
    let other = File::create(&slot).unwrap();
    other.try_lock().unwrap();
    let mut child = fx
        .command(&fx.repo, &["run", "--once", "--unsandboxed"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = BufReader::new(child.stderr.take().unwrap());
    let (tx, lines) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut all = String::new();
        for line in stderr.lines().map_while(Result::ok) {
            all.push_str(&line);
            all.push('\n');
            // The receiver stops listening after the first line.
            let _ = tx.send(line);
        }
        all
    });
    // The gate cannot pass the slot before it is free, so its waiting line
    // comes first. The timeout only ends a run that never says anything.
    let first = lines.recv_timeout(Duration::from_secs(120));
    // Not a synchronisation: the wait has to last at least one whole second
    // to be recorded at all (the receipt counts whole seconds).
    let seen = Instant::now();
    while seen.elapsed() < Duration::from_millis(1500) {
        std::thread::sleep(Duration::from_millis(100));
    }
    other.unlock().unwrap();
    let out = child.wait_with_output().unwrap();
    let stderr = reader.join().unwrap();
    let first = first.expect("aaf run printed nothing while its gate waited");
    assert!(first.contains("waiting for a heavy slot"), "{first}");
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["requeued"], json!([bad]), "{stderr}");
    let note = fx.show(&bad)["task"]["context_notes"][0]
        .as_str()
        .unwrap()
        .to_string();
    assert!(note.starts_with("unblock[T3] digest="), "{note}");

    // Second run: no wait, the same failure. The ladder sees the repeat.
    let out = fx.aaf(&["run", "--once", "--unsandboxed"]);
    assert_eq!(out.json["blocked"], json!([bad]), "{}", out.stderr);
    assert_eq!(
        fx.show(&bad)["task"]["blocked_reason"]["kind"],
        json!("needs_human")
    );
}
