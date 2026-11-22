//! `aaf gate`, `aaf approve` and the `--json` contract through the built binary.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "cli_support.rs"]
mod support;

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};
use support::{Sandbox, write};

/// A committed repo whose scoped test command is `test_scoped`.
fn repo_with_test_command(test_scoped: &str) -> Sandbox {
    let sb = Sandbox::new();
    write(
        &sb.repo.join(".autofactory/config.toml"),
        &format!("[commands]\ntest_scoped = \"{test_scoped}\"\n"),
    );
    // Tier 1 of the child (`$HOME/.config/autofactory`): the heavy command
    // must not wait for the CPU load of the machine running the tests. The
    // memory-pressure wait has no switch and stays real.
    write(
        &sb.home.join(".config/autofactory/config.toml"),
        "[machine]\nmax_load_percent = 0\n",
    );
    sb.commit_all("config");
    sb
}

#[test]
fn gate_reports_a_wait_on_stderr_and_stdout_stays_one_json_object() {
    if !support::memory_is_calm("gate_reports_a_wait_on_stderr") {
        return;
    }
    let sb = repo_with_test_command("true");
    // Hold slot 0 the way another `aaf gate` would: the command has to wait.
    let slot = aaf_engine::throttle::slots::slot_path(&sb.data, 0);
    write(&slot, "");
    let held = File::open(&slot).unwrap();
    held.lock().unwrap();

    let mut child = sb
        .command(&["gate", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut first = String::new();
        stderr.read_line(&mut first).unwrap();
        tx.send(first).unwrap();
        let mut rest = String::new();
        stderr.read_to_string(&mut rest).unwrap();
        rest
    });
    // The ceiling only ends a run that never reports; a report arrives at once.
    let first = rx.recv_timeout(Duration::from_secs(60));
    held.unlock().unwrap();
    let out = child.wait_with_output().unwrap();
    reader.join().unwrap();

    let first = first.expect("no progress line on stderr while the slot was taken");
    assert!(
        first.starts_with("aaf gate: test_scoped: waiting"),
        "{first}"
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(support::one_object(&out)["status"], json!("CLEAR"));
}

#[test]
fn gate_is_clear_exit_0_and_reports_the_receipt() {
    if !support::memory_is_calm("gate_is_clear_exit_0") {
        return;
    }
    let sb = repo_with_test_command("true");

    let (code, out) = sb.json(&["gate"]);

    assert_eq!(code, 0, "{out}");
    assert_eq!(out["ok"], json!(true));
    assert_eq!(out["command"], json!("gate"));
    assert_eq!(out["status"], json!("CLEAR"));
    assert_eq!(out["mode"], json!("scoped"));
    assert_eq!(out["reasons"], json!([]));
    assert!(out["tree_fp"].as_str().unwrap().starts_with("git-tree:"));
    assert_eq!(out["findings"]["static_critical"], json!(0));
    assert_eq!(out["findings"]["store_critical"], json!(0));
    let commands = out["commands"].as_array().unwrap();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0]["name"], json!("test_scoped"));
    assert_eq!(commands[0]["exit_code"], json!(0));
    let receipt = Path::new(out["receipt_path"].as_str().unwrap());
    assert!(receipt.starts_with(&sb.data) && receipt.is_file());
}

#[test]
fn gate_is_blocked_exit_2_when_the_test_command_fails() {
    if !support::memory_is_calm("gate_is_blocked_exit_2") {
        return;
    }
    let sb = repo_with_test_command("false");

    let (code, out) = sb.json(&["gate", "--scoped"]);

    assert_eq!(code, 2, "{out}");
    assert_eq!(out["ok"], json!(false));
    assert_eq!(out["status"], json!("BLOCKED"));
    assert!(!out["reasons"].as_array().unwrap().is_empty());
}

#[test]
fn gate_is_unverified_exit_2_without_a_test_command_for_the_mode() {
    let sb = repo_with_test_command("true");

    // `test_full` is empty, so the full and release modes cannot be verified.
    for (flag, mode) in [("--full", "full"), ("--release", "release")] {
        let (code, out) = sb.json(&["gate", flag]);
        assert_eq!(code, 2, "{out}");
        assert_eq!(out["status"], json!("UNVERIFIED"));
        assert_eq!(out["mode"], json!(mode));
    }

    let text = sb.aaf(&["gate", "--full"]);
    assert_eq!(text.status.code(), Some(2));
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.contains("UNVERIFIED"), "{text}");
}

#[test]
fn gate_counts_a_critical_static_finding_and_lists_it() {
    let sb = repo_with_test_command("true");
    // Split so this file itself never trips the placeholder rule.
    let placeholder = concat!("fn f() { to", "do!() }\n");
    write(&sb.repo.join("src/lib.rs"), placeholder);

    let (code, out) = sb.json(&["gate"]);

    assert_eq!(code, 2, "{out}");
    assert_eq!(out["status"], json!("BLOCKED"));
    assert_eq!(out["findings"]["static_critical"], json!(1));
    assert_eq!(out["static_findings"][0]["rule"], json!("LAZY-001"));
}

#[test]
fn gate_config_error_is_exit_1_as_one_json_object() {
    let sb = Sandbox::new();
    write(
        &sb.repo.join(".autofactory/config.toml"),
        "[gate]\nno_such_key = 1\n",
    );

    let (code, out) = sb.json(&["gate"]);

    assert_eq!(code, 1, "{out}");
    assert_eq!(out["status"], json!("error"));
    assert!(out["error"].as_str().unwrap().contains("no_such_key"));
}

#[test]
fn two_gate_modes_at_once_is_a_usage_error() {
    let sb = Sandbox::new();

    let (code, out) = sb.json(&["gate", "--full", "--release"]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["status"], json!("usage_error"));
    assert!(out["error"].is_string());

    // Without --json clap's own message goes to stderr and stdout stays empty.
    let plain = sb.aaf(&["gate", "--full", "--release"]);
    assert_eq!(plain.status.code(), Some(2));
    assert!(plain.stdout.is_empty());
    assert!(!plain.stderr.is_empty());
}

#[test]
fn json_mode_covers_unknown_commands_and_help() {
    let sb = Sandbox::new();

    let (code, out) = sb.json(&["no-such-command"]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["status"], json!("usage_error"));

    let (code, out) = sb.json(&["--help"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["status"], json!("help"));
    assert!(out["text"].as_str().unwrap().contains("gate"));
}

fn approvals(sb: &Sandbox) -> bool {
    sb.data.join("approvals").exists()
}

/// A locked file (matches `**/tests/**`) modified after it was committed.
fn repo_with_locked_change() -> Sandbox {
    let sb = repo_with_test_command("true");
    write(&sb.repo.join("tests/it.rs"), "// v1\n");
    sb.commit_all("add test");
    write(&sb.repo.join("tests/it.rs"), "// v2\n");
    sb
}

#[test]
fn approve_refuses_when_stdin_is_not_a_terminal() {
    let sb = repo_with_locked_change();

    let (code, out) = sb.json(&["approve", "--reviewer", "alice"]);

    assert_eq!(code, 2, "{out}");
    assert_eq!(out["status"], json!("refused"));
    assert!(out["reason"].as_str().unwrap().contains("terminal"));
    assert!(!approvals(&sb));
    // And the gate still blocks on the unapproved locked change.
    let (code, gate) = sb.json(&["gate"]);
    assert_eq!(code, 2, "{gate}");
    assert_eq!(gate["head_lock"]["status"], json!("BLOCKED"));
}

#[test]
fn approve_refuses_inside_an_agent_run() {
    let sb = repo_with_locked_change();

    let out = sb
        .command(&["approve", "--reviewer", "alice", "--json"])
        .env("AAF_RUN_ID", "run-1")
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(2));
    let out: Value = support::one_object(&out);
    assert_eq!(out["status"], json!("refused"));
    assert!(out["reason"].as_str().unwrap().contains("AAF_RUN_ID"));
    assert!(!approvals(&sb));
}

#[test]
fn approve_needs_a_reviewer() {
    let sb = Sandbox::new();

    let (code, out) = sb.json(&["approve"]);

    assert_eq!(code, 2, "{out}");
    assert_eq!(out["status"], json!("usage_error"));
}

/// A verdict taken from a stored receipt ran no command now: the caller
/// (a hook, a CI step, a person) must be able to tell.
#[test]
fn gate_says_when_the_verdict_comes_from_a_reused_receipt() {
    if !support::memory_is_calm("gate_says_when_the_verdict_comes_from_a_reused_receipt") {
        return;
    }
    let sb = repo_with_test_command("echo ran >> ../runs.log");
    let runs = || std::fs::read_to_string(sb.repo.join("../runs.log")).unwrap_or_default();

    let (code, first) = sb.json(&["gate"]);
    assert_eq!(code, 0, "{first}");
    assert_eq!(first["reused_receipt"], json!(false), "{first}");
    assert_eq!(runs().lines().count(), 1);

    let (code, second) = sb.json(&["gate"]);
    assert_eq!(code, 0, "{second}");
    assert_eq!(second["reused_receipt"], json!(true), "{second}");
    assert_eq!(second["tree_fp"], first["tree_fp"]);
    // Nothing ran the second time.
    assert_eq!(runs().lines().count(), 1);
    let text = String::from_utf8(sb.aaf(&["gate"]).stdout).unwrap();
    assert!(text.contains("reused receipt"), "{text}");

    // A changed tree is gated again, and says so.
    write(&sb.repo.join("new.txt"), "x\n");
    let (_, third) = sb.json(&["gate"]);
    assert_eq!(third["reused_receipt"], json!(false), "{third}");
    assert_eq!(runs().lines().count(), 2);
}

/// A person's own checkout: a file only `info/exclude` hides is theirs. The
/// gate runs, keeps the file and says it was there.
#[test]
fn gate_is_clear_with_a_warning_beside_a_file_only_info_exclude_hides() {
    if !support::memory_is_calm("gate_is_clear_with_a_warning") {
        return;
    }
    let sb = repo_with_test_command("true");
    let exclude = sb.repo.join(".git/info/exclude");
    let mut rules = std::fs::read_to_string(&exclude).unwrap_or_default();
    rules.push_str("conftest.py\n");
    write(&exclude, &rules);
    write(&sb.repo.join("conftest.py"), "collect_ignore = ['.']\n");

    let (code, out) = sb.json(&["gate"]);

    assert_eq!(code, 0, "{out}");
    assert_eq!(out["status"], json!("CLEAR"), "{out}");
    let reasons: Vec<&str> = out["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .filter(|r| !r.starts_with("throttle: "))
        .collect();
    assert_eq!(reasons.len(), 1, "{out}");
    let warning = reasons[0];
    assert!(warning.starts_with("warning: work tree: "), "{warning}");
    assert!(
        warning.contains("conftest.py") && warning.contains("info/exclude"),
        "{warning}"
    );
    assert!(sb.repo.join("conftest.py").exists());
    // The person reading the terminal sees it too.
    let text = String::from_utf8(sb.aaf(&["gate"]).stdout).unwrap();
    assert!(text.contains("warning: work tree: "), "{text}");
}

/// A tracked test behind an index flag is on disk for the test command and
/// missing from the fingerprint. No memory check: nothing heavy may start.
#[test]
fn gate_is_blocked_exit_2_and_runs_nothing_on_a_tracked_file_git_was_told_to_skip() {
    let sb = repo_with_test_command("echo ran >> ../runs.log");
    write(&sb.repo.join("test_x.py"), "assert 1 == 2\n");
    sb.commit_all("a test");
    sb.git(&["update-index", "--skip-worktree", "test_x.py"]);
    write(&sb.repo.join("test_x.py"), "assert True\n");

    let (code, out) = sb.json(&["gate"]);

    assert_eq!(code, 2, "{out}");
    assert_eq!(out["status"], json!("BLOCKED"), "{out}");
    let reasons = out["reasons"].to_string();
    assert!(reasons.contains("test_x.py"), "{reasons}");
    assert!(!sb.repo.join("../runs.log").exists());
}

mod skip_rule {
    use aaf_engine::throttle::Pressure;

    use super::support::calm_or_skip;

    fn decide(pressure: Option<Pressure>, require_real: bool) -> (bool, String) {
        let mut notice = Vec::new();
        let calm = calm_or_skip("some_test", pressure, require_real, &mut notice);
        (calm, String::from_utf8(notice).unwrap())
    }

    #[test]
    fn calm_or_unreadable_machine_runs_the_test_without_a_word() {
        for require_real in [false, true] {
            for pressure in [Some(Pressure::Normal), None] {
                assert_eq!(decide(pressure, require_real), (true, String::new()));
            }
        }
    }

    #[test]
    fn busy_machine_skips_with_a_line_that_names_the_test_and_the_switch() {
        for pressure in [Pressure::Warn, Pressure::Critical] {
            let (calm, notice) = decide(Some(pressure), false);
            assert!(!calm);
            assert!(notice.starts_with("SKIPPED some_test: "), "{notice}");
            assert!(notice.contains("AAF_REQUIRE_REAL=1"), "{notice}");
            assert!(notice.ends_with('\n'), "{notice}");
        }
    }

    #[test]
    #[should_panic(expected = "AAF_REQUIRE_REAL=1: some_test cannot run for real")]
    fn busy_machine_fails_the_test_when_real_runs_are_required() {
        decide(Some(Pressure::Warn), true);
    }
}
