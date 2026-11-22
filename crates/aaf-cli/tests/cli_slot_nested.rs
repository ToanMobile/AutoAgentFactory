//! `aaf slot` through the built binary: who may run below a slot holder
//! without waiting, and build tools under a path with a space. Light
//! commands and fake build tools only.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "cli_support.rs"]
mod support;

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use aaf_engine::throttle::slots::try_acquire;
use aaf_engine::throttle::token::slot_env_value;
use aaf_engine::throttle::{Admission, HeavySample, SlotGuard, ThreadSleeper, stats};
use support::{Sandbox, calm_machine, make_executable, memory_is_calm, write};

const DAEMON_SWITCHES: &str = "-Dorg.gradle.daemon=false \
     -Dorg.gradle.project.kotlin.compiler.execution.strategy=in-process";

/// A sandbox with this many heavy slots. `None` when the machine is under
/// memory pressure and the test has to be skipped.
fn calm_sandbox(test: &str, slots: u32) -> Option<Sandbox> {
    if !memory_is_calm(test) {
        return None;
    }
    let sb = Sandbox::new();
    calm_machine(&sb, slots);
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

/// `<slot>:<32 hex digits>`, as the throttle sets it on its command.
fn is_slot_value(value: &str, slot: &str) -> bool {
    value.split_once(':').is_some_and(|(index, token)| {
        index == slot
            && token.len() == 32
            && token
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    })
}

/// `aaf slot` running a command that writes its `AAF_HEAVY_SLOT` to `seen`
/// followed by a line `started`, then stays until `go` exists (a minute at
/// most, so a failed test leaves nothing behind).
fn holder(sb: &Sandbox, seen: &Path, go: &Path) -> Child {
    let script = format!(
        "printf '%s\\nstarted\\n' \"$AAF_HEAVY_SLOT\" > '{seen}'; n=0; \
         until [ -e '{go}' ] || [ $n -ge 1200 ]; do sleep 0.05; n=$((n+1)); done",
        seen = seen.display(),
        go = go.display()
    );
    sb.command(&["slot", "--label", "holder", "--", "sh", "-c", &script])
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn first_line(path: &Path) -> String {
    read(path).lines().next().unwrap_or_default().to_string()
}

#[test]
fn slot_variable_opens_nothing_without_the_token_of_the_run_that_holds_the_slot() {
    let Some(sb) = calm_sandbox("slot_variable_opens_nothing_without_the_token", 1) else {
        return;
    };
    let (earlier, seen, go) = (
        beside(&sb, "earlier"),
        beside(&sb, "seen"),
        beside(&sb, "go"),
    );
    let marker = beside(&sb, "ran");
    let touch = [
        "slot",
        "--timeout",
        "1",
        "--",
        "touch",
        marker.to_str().unwrap(),
    ];

    // A run that is over by the time the holder below starts.
    let print = format!("printf %s \"$AAF_HEAVY_SLOT\" > '{}'", earlier.display());
    assert_eq!(
        sb.aaf(&["slot", "--", "sh", "-c", &print]).status.code(),
        Some(0)
    );
    let finished = read(&earlier);

    let mut holding = holder(&sb, &seen, &go);
    let started = appears(&seen, "started");
    let current = first_line(&seen);

    let wrong_token = format!("0:{}", "deadbeef".repeat(4));
    let mut jumped = Vec::new();
    for value in ["0", "0:", wrong_token.as_str(), finished.as_str()] {
        let out = sb
            .command(&touch)
            .env("AAF_HEAVY_SLOT", value)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if out.status.code() != Some(75) || marker.exists() || stderr.contains("already inside") {
            jumped.push((value, out.status.code(), stderr));
        }
        let _ = fs::remove_file(&marker);
    }

    // What a real child of the holder inherits does open the slot.
    let inherited = sb
        .command(&touch)
        .env("AAF_HEAVY_SLOT", &current)
        .output()
        .unwrap();
    let ran_inside = marker.exists();
    let _ = fs::remove_file(&marker);

    write(&go, "");
    let holder_code = holding.wait().unwrap().code();

    // Once that run is over its token opens nothing either: with the slot
    // taken again by somebody else the command waits.
    let mut next = holder(&sb, &beside(&sb, "seen-next"), &beside(&sb, "go-next"));
    let next_started = appears(&beside(&sb, "seen-next"), "started");
    let stale = sb
        .command(&touch)
        .env("AAF_HEAVY_SLOT", &current)
        .output()
        .unwrap();
    let ran_stale = marker.exists();
    write(&beside(&sb, "go-next"), "");
    let next_code = next.wait().unwrap().code();

    assert!(started && next_started, "a holder never started");
    assert_eq!(jumped, Vec::new(), "these values jumped the queue");
    assert!(is_slot_value(&finished, "0"), "{finished:?}");
    assert!(is_slot_value(&current, "0"), "{current:?}");
    assert_ne!(current, finished, "every run gets a token of its own");
    assert_eq!(inherited.status.code(), Some(0), "{inherited:?}");
    assert!(ran_inside);
    assert!(
        String::from_utf8_lossy(&inherited.stderr).contains("already inside heavy slot 0"),
        "{inherited:?}"
    );
    assert_eq!(stale.status.code(), Some(75), "{stale:?}");
    assert!(!ran_stale);
    assert_eq!((holder_code, next_code), (Some(0), Some(0)));
}

#[test]
fn child_of_a_slot_holder_runs_its_own_slot_command_at_once_with_the_daemon_switches() {
    let Some(sb) = calm_sandbox("child_of_a_slot_holder_runs_its_own_slot_command", 1) else {
        return;
    };
    let seen = beside(&sb, "seen");
    let inner = beside(&sb, "inner.sh");
    write(
        &inner,
        &format!(
            "printf '%s' \"$GRADLE_OPTS\" > '{}'\nexit 7\n",
            seen.display()
        ),
    );
    // The build script in the middle drops GRADLE_OPTS: the nested command
    // must get the switches from the nested `aaf slot` itself. One slot and
    // a timeout: if the nested call queued, it would end with 75.
    let outer = format!(
        "unset GRADLE_OPTS; '{aaf}' slot --timeout 1 -- sh '{inner}'",
        aaf = env!("CARGO_BIN_EXE_aaf"),
        inner = inner.display()
    );
    let out = sb.aaf(&["slot", "--", "sh", "-c", &outer]);

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(7), "{stderr}");
    assert!(stderr.contains("already inside heavy slot 0"), "{stderr}");
    assert_eq!(read(&seen), DAEMON_SWITCHES);
}

/// Slot 0 of a machine with `slots` slots, held by this test process the
/// way another `aaf` holds it: locked, with a token of its own in the lock
/// file. The tests below need no calm machine: a command that waits for a
/// slot, or runs below a holder, never gets as far as the wait for memory.
fn hold(sb: &Sandbox, slots: usize, admission: Admission) -> SlotGuard {
    try_acquire(&sb.data, slots, admission, &|| None)
        .unwrap()
        .unwrap()
}

#[test]
fn second_process_with_a_made_up_slot_variable_waits_and_exits_75() {
    let sb = Sandbox::new();
    calm_machine(&sb, 1);
    let marker = beside(&sb, "ran");
    let touch = [
        "slot",
        "--timeout",
        "1",
        "--",
        "touch",
        marker.to_str().unwrap(),
    ];

    // A run that is over before the present holder takes the slot.
    let earlier = hold(&sb, 1, Admission::Unmeasured);
    let finished = slot_env_value(0, earlier.token());
    drop(earlier);
    let held = hold(&sb, 1, Admission::Unmeasured);
    let current = slot_env_value(0, held.token());
    assert_ne!(current, finished);

    let wrong_token = format!("0:{}", "deadbeef".repeat(4));
    for value in ["0", "0:", wrong_token.as_str(), finished.as_str()] {
        let out = sb
            .command(&touch)
            .env("AAF_HEAVY_SLOT", value)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(75), "{value:?}: {stderr}");
        assert!(
            stderr.contains("waiting for a heavy slot") && !stderr.contains("already inside"),
            "{value:?}: {stderr}"
        );
        assert!(!marker.exists(), "{value:?} ran the command");
    }

    // The value a child of the holder inherits lets the same command run.
    let inside = sb
        .command(&touch)
        .env("AAF_HEAVY_SLOT", &current)
        .output()
        .unwrap();
    assert_eq!(inside.status.code(), Some(0), "{inside:?}");
    assert!(marker.exists());
    fs::remove_file(&marker).unwrap();

    // Until that run is over and the slot belongs to the next one.
    drop(held);
    let _next = hold(&sb, 1, Admission::Unmeasured);
    let stale = sb
        .command(&touch)
        .env("AAF_HEAVY_SLOT", &current)
        .output()
        .unwrap();
    assert_eq!(stale.status.code(), Some(75), "{stale:?}");
    assert!(!marker.exists());
}

#[test]
fn command_below_a_holder_gets_the_daemon_switches_from_the_nested_slot() {
    let sb = Sandbox::new();
    calm_machine(&sb, 1);
    let held = hold(&sb, 1, Admission::Unmeasured);
    let print = ["slot", "--", "sh", "-c", "printf %s \"$GRADLE_OPTS\""];

    // Whatever stands between the holder and this process dropped the
    // variable, or set one of its own.
    let bare = sb
        .command(&print)
        .env("AAF_HEAVY_SLOT", slot_env_value(0, held.token()))
        .output()
        .unwrap();
    assert_eq!(bare.status.code(), Some(0), "{bare:?}");
    assert!(String::from_utf8_lossy(&bare.stderr).contains("already inside heavy slot 0"));
    assert_eq!(String::from_utf8(bare.stdout).unwrap(), DAEMON_SWITCHES);

    let own = sb
        .command(&print)
        .env("AAF_HEAVY_SLOT", slot_env_value(0, held.token()))
        .env("GRADLE_OPTS", "-Xmx1g")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(own.stdout).unwrap(),
        format!("-Xmx1g {DAEMON_SWITCHES}")
    );
}

#[test]
fn build_tool_under_a_path_with_a_space_waits_for_the_machine_to_itself() {
    let sb = Sandbox::new();
    calm_machine(&sb, 2);
    let sample = HeavySample {
        peak_rss_bytes: 1024,
        duration_ms: 1,
    };
    let repo_id = aaf_engine::gate::stats_repo_id(&sb.repo);
    assert!(stats::record(&sb.data, &repo_id, "g", sample, &ThreadSleeper).unwrap());
    let marker = beside(&sb, "ran");
    let gradlew = sb.home.join("my app/gradlew");
    write(
        &gradlew,
        &format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    );
    make_executable(&gradlew);

    // A small measured command in slot 0; slot 1 is free, and a command
    // measured as tiny as this one would be let into it.
    let _other = hold(&sb, 2, Admission::Measured { peak_rss_bytes: 1 });
    let out = sb.aaf(&[
        "slot",
        "--label",
        "g",
        "--timeout",
        "1",
        "--",
        gradlew.to_str().unwrap(),
        "lint",
    ]);

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(75), "{stderr}");
    assert!(
        stderr.starts_with("aaf slot: g: waiting for a heavy slot"),
        "{stderr}"
    );
    assert!(!marker.exists());
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
fn build_tool_under_a_path_with_a_space_still_runs_alone() {
    // Two slots and a command measured as tiny: only its being a build
    // tool keeps the second run out while the first is going.
    let Some(sb) = calm_sandbox("build_tool_under_a_path_with_a_space", 2) else {
        return;
    };
    let sample = HeavySample {
        peak_rss_bytes: 1024,
        duration_ms: 1,
    };
    let repo_id = aaf_engine::gate::stats_repo_id(&sb.repo);
    assert!(stats::record(&sb.data, &repo_id, "g", sample, &ThreadSleeper).unwrap());

    let (log, go) = (beside(&sb, "log"), beside(&sb, "go"));
    let gradlew = sb.home.join("my app/gradlew");
    // `a` stays until `go` exists (a minute at most); `b` does not wait.
    write(
        &gradlew,
        &format!(
            "#!/bin/sh\necho \"start-$1\" >> '{log}'\nn=0\n\
             if [ \"$1\" = a ]; then\n\
             until [ -e '{go}' ] || [ $n -ge 1200 ]; do sleep 0.05; n=$((n+1)); done\n\
             fi\necho \"end-$1\" >> '{log}'\n",
            log = log.display(),
            go = go.display()
        ),
    );
    make_executable(&gradlew);
    let gradlew = gradlew.to_str().unwrap();

    let mut first = sb
        .command(&["slot", "--label", "g", "--", gradlew, "a"])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let first_started = appears(&log, "start-a");
    let mut second = sb
        .command(&["slot", "--label", "g", "--", gradlew, "b"])
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
        waiting.starts_with("aaf slot: g: waiting for a heavy slot"),
        "{waiting:?}"
    );
    assert_eq!(while_waiting, "start-a\n");
    assert_eq!(codes, (Some(0), Some(0)));
    assert_eq!(read(&log), "start-a\nend-a\nstart-b\nend-b\n");
}
