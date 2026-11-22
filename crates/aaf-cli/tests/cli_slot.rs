//! `aaf slot` through the built binary, with light commands only.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "cli_support.rs"]
mod support;

use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;
use support::{Sandbox, calm_machine, memory_is_calm, write};

const DAEMON_SWITCHES: &str = "-Dorg.gradle.daemon=false \
     -Dorg.gradle.project.kotlin.compiler.execution.strategy=in-process";

/// A sandbox with one heavy slot. `None` when the machine is under memory
/// pressure and the test has to be skipped.
fn calm_sandbox(test: &str) -> Option<Sandbox> {
    if !memory_is_calm(test) {
        return None;
    }
    let sb = Sandbox::new();
    calm_machine(&sb, 1);
    Some(sb)
}

/// Markers go next to the repo, not into it.
fn beside(sb: &Sandbox, name: &str) -> PathBuf {
    sb.home.join(name)
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// Polls until `path` holds `text`; false after a minute.
fn appears(path: &Path, text: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if read(path).contains(text) {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    false
}

/// What the holder of [`hold_slot_zero`] wrote beside the lock file.
const HELD_TOKEN: &str = "0123456789abcdef0123456789abcdef";

/// Takes slot 0 the way another `aaf` process would.
fn hold_slot_zero(sb: &Sandbox) -> File {
    let slot = aaf_engine::throttle::slots::slot_path(&sb.data, 0);
    let token = aaf_engine::throttle::paths::token_path(&sb.data, 0);
    write(&token, &format!("token={HELD_TOKEN}\n"));
    write(&slot, "reserved=unknown\n");
    let held = File::open(&slot).unwrap();
    held.lock().unwrap();
    held
}

/// `aaf slot` running a command that writes `start-a`, stays until `go`
/// exists (a minute at most, so a failed test leaves nothing behind) and
/// writes `end-a`.
fn slot_that_stays(sb: &Sandbox, log: &Path, go: &Path) -> Child {
    let script = format!(
        "echo start-a >> '{log}'; n=0; \
         until [ -e '{go}' ] || [ $n -ge 1200 ]; do sleep 0.05; n=$((n+1)); done; \
         echo end-a >> '{log}'",
        log = log.display(),
        go = go.display()
    );
    sb.command(&["slot", "--label", "a", "--", "sh", "-c", &script])
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// First line the child writes to stderr; empty when it writes none.
fn first_stderr_line(child: &mut Child) -> String {
    let mut line = String::new();
    BufReader::new(child.stderr.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    line
}

#[test]
fn two_slot_commands_run_strictly_one_after_the_other() {
    let Some(sb) = calm_sandbox("two_slot_commands_run_strictly_one_after_the_other") else {
        return;
    };
    let (log, go) = (beside(&sb, "log"), beside(&sb, "go"));
    let mut first = slot_that_stays(&sb, &log, &go);
    let first_started = appears(&log, "start-a");

    let script = format!("echo start-b >> '{0}'; echo end-b >> '{0}'", log.display());
    let mut second = sb
        .command(&["slot", "--label", "b", "--", "sh", "-c", &script])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The second one says that it waits: it is in the queue, not running.
    let waiting = first_stderr_line(&mut second);
    let while_waiting = read(&log);
    write(&go, "");
    let codes = (first.wait().unwrap().code(), second.wait().unwrap().code());

    assert!(first_started, "the first command never started");
    assert!(
        waiting.starts_with("aaf slot: b: waiting for a heavy slot"),
        "{waiting:?}"
    );
    assert_eq!(while_waiting, "start-a\n");
    assert_eq!(codes, (Some(0), Some(0)));
    assert_eq!(read(&log), "start-a\nend-a\nstart-b\nend-b\n");
}

#[test]
fn slot_command_and_a_gate_share_the_one_queue() {
    let Some(sb) = calm_sandbox("slot_command_and_a_gate_share_the_one_queue") else {
        return;
    };
    let (log, go) = (beside(&sb, "log"), beside(&sb, "go"));
    write(
        &sb.repo.join(".autofactory/config.toml"),
        &format!(
            "[commands]\ntest_scoped = \"echo start-gate >> '{0}'; echo end-gate >> '{0}'\"\n",
            log.display()
        ),
    );
    sb.commit_all("config");
    let mut slot = slot_that_stays(&sb, &log, &go);
    let slot_started = appears(&log, "start-a");

    let mut gate = sb
        .command(&["gate", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let waiting = first_stderr_line(&mut gate);
    let while_waiting = read(&log);
    write(&go, "");
    let slot_code = slot.wait().unwrap().code();
    let gate = gate.wait_with_output().unwrap();

    assert!(slot_started, "the slot command never started");
    assert!(
        waiting.starts_with("aaf gate: test_scoped: waiting for a heavy slot"),
        "{waiting:?}"
    );
    assert_eq!(while_waiting, "start-a\n");
    assert_eq!(slot_code, Some(0));
    assert_eq!(gate.status.code(), Some(0));
    assert_eq!(support::one_object(&gate)["status"], json!("CLEAR"));
    assert_eq!(read(&log), "start-a\nend-a\nstart-gate\nend-gate\n");
}

#[test]
fn exit_code_and_output_are_the_commands_own() {
    let Some(sb) = calm_sandbox("exit_code_and_output_are_the_commands_own") else {
        return;
    };

    let ok = sb.aaf(&["slot", "--", "true"]);
    assert_eq!(ok.status.code(), Some(0));
    assert!(ok.stdout.is_empty(), "{:?}", ok.stdout);

    let failed = sb.aaf(&["slot", "--", "sh", "-c", "echo out; echo err >&2; exit 3"]);
    assert_eq!(failed.status.code(), Some(3));
    // Nothing of aaf's own on stdout, and the command's stderr comes through.
    assert_eq!(failed.stdout, b"out\n");
    assert!(String::from_utf8_lossy(&failed.stderr).contains("err\n"));

    // SIGTERM is 15.
    let killed = sb.aaf(&["slot", "--", "sh", "-c", "kill -TERM $$"]);
    assert_eq!(killed.status.code(), Some(128 + 15));

    let missing = sb.aaf(&["slot", "--", "no-such-program-aaf-test"]);
    // Said by `nice` when the command runs at low priority, by aaf itself
    // otherwise; 127 either way, as a shell would.
    assert_eq!(missing.status.code(), Some(127), "{missing:?}");
    assert!(!missing.stderr.is_empty());
}

#[test]
fn timeout_while_waiting_is_exit_75_and_the_command_never_runs() {
    let Some(sb) = calm_sandbox("timeout_while_waiting_is_exit_75") else {
        return;
    };
    let marker = beside(&sb, "ran");
    let touch = [
        "slot",
        "--timeout",
        "0",
        "--",
        "touch",
        marker.to_str().unwrap(),
    ];
    let held = hold_slot_zero(&sb);

    let out = sb.aaf(&touch);
    assert_eq!(out.status.code(), Some(75));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no heavy slot within 0s; the command was not started"),
        "{stderr}"
    );
    assert!(out.stdout.is_empty());
    assert!(!marker.exists());

    // The same command line once the slot is free.
    held.unlock().unwrap();
    assert_eq!(sb.aaf(&touch).status.code(), Some(0));
    assert!(marker.exists());
}

#[test]
fn command_without_the_separator_is_a_usage_error_and_nothing_runs() {
    let sb = Sandbox::new();
    let marker = beside(&sb, "ran");
    let marker_arg = marker.to_str().unwrap();

    for args in [
        vec!["slot", "touch", marker_arg],
        vec!["slot", "--label", "x", "touch", marker_arg],
        vec!["slot"],
        vec!["slot", "--"],
    ] {
        let out = sb.aaf(&args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        assert!(!out.stderr.is_empty(), "{args:?}");
        assert!(!marker.exists(), "{args:?}");
    }
}

#[test]
fn json_flag_is_refused_and_belongs_to_the_command_after_the_separator() {
    let Some(sb) = calm_sandbox("json_flag_is_refused") else {
        return;
    };
    let marker = beside(&sb, "ran");

    for args in [
        ["slot", "--json", "--", "touch", marker.to_str().unwrap()],
        ["--json", "slot", "--", "touch", marker.to_str().unwrap()],
    ] {
        let out = sb.aaf(&args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let out = support::one_object(&out);
        assert_eq!(out["status"], json!("usage_error"));
        assert!(out["error"].as_str().unwrap().contains("--json"));
        assert!(!marker.exists());
    }

    // After `--` it is the command's own argument.
    let passed = sb.aaf(&["slot", "--", "sh", "-c", "echo \"$1\"", "sh", "--json"]);
    assert_eq!(passed.status.code(), Some(0));
    assert_eq!(passed.stdout, b"--json\n");
}

#[test]
fn works_outside_a_git_repository_and_inside_an_agent_run() {
    if !memory_is_calm("works_outside_a_git_repository") {
        return;
    }
    let sb = Sandbox::without_git();
    calm_machine(&sb, 1);

    let out = sb
        .command(&["slot", "--", "sh", "-c", "pwd; exit 4"])
        .env("AAF_RUN_ID", "run-1")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4), "{out:?}");
    // It ran where it was called.
    let cwd = String::from_utf8(out.stdout).unwrap();
    assert_eq!(Path::new(cwd.trim()), sb.repo);
    // It went through the queue: the slot file is there.
    assert!(aaf_engine::throttle::slots::slot_path(&sb.data, 0).exists());
}

#[test]
fn command_gets_the_daemon_switches_after_the_callers_gradle_opts() {
    let Some(sb) = calm_sandbox("command_gets_the_daemon_switches") else {
        return;
    };
    let out = sb
        .command(&["slot", "--", "sh", "-c", "printf %s \"$GRADLE_OPTS\""])
        .env("GRADLE_OPTS", "-Xmx1g")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("-Xmx1g {DAEMON_SWITCHES}")
    );
}

#[test]
fn command_below_a_slot_holder_runs_at_once_instead_of_waiting_for_its_own_ancestor() {
    let Some(sb) = calm_sandbox("command_below_a_slot_holder_runs_at_once") else {
        return;
    };
    let marker = beside(&sb, "ran");
    let touch = [
        "slot",
        "--timeout",
        "0",
        "--",
        "touch",
        marker.to_str().unwrap(),
    ];
    let held = hold_slot_zero(&sb);

    // What a command started by the holder of slot 0 has in its environment:
    // the slot and the token the holder wrote into the lock file.
    let nested = sb
        .command(&touch)
        .env("AAF_HEAVY_SLOT", format!("0:{HELD_TOKEN}"))
        .output()
        .unwrap();
    assert_eq!(nested.status.code(), Some(0), "{nested:?}");
    assert!(marker.exists());
    assert!(String::from_utf8_lossy(&nested.stderr).contains("already inside heavy slot 0"));
    fs::remove_file(&marker).unwrap();

    // The slot number alone, or with part of the token, excuses nothing.
    for value in [
        "0",
        "0:",
        "0:0123456789abcdef",
        " 0:0123456789abcdef0123456789abcdef",
    ] {
        let bare = sb
            .command(&touch)
            .env("AAF_HEAVY_SLOT", value)
            .output()
            .unwrap();
        assert_eq!(bare.status.code(), Some(75), "{value:?}");
        assert!(!marker.exists(), "{value:?}");
    }

    // A value that names no slot excuses nothing.
    let junk = sb
        .command(&touch)
        .env("AAF_HEAVY_SLOT", "x")
        .output()
        .unwrap();
    assert_eq!(junk.status.code(), Some(75));
    assert!(!marker.exists());

    // Nor does a variable left over when nobody holds a slot: the command
    // then takes its turn like any other.
    held.unlock().unwrap();
    let stale = sb
        .command(&touch)
        .env("AAF_HEAVY_SLOT", "0")
        .output()
        .unwrap();
    assert_eq!(stale.status.code(), Some(0));
    assert!(!String::from_utf8_lossy(&stale.stderr).contains("already inside"));
    assert!(marker.exists());
}
