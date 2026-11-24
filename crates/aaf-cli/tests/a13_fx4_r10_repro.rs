#![allow(clippy::unwrap_used, clippy::expect_used)]
//! R10-L8-1: `board add --repro-cmd` / `aaf repro` must not confirm a bug
//! whose command never ran (exit 126/127), must say why a run failed, and
//! must validate the cheap arguments before running the repro command.

#[path = "factory_support.rs"]
mod support;

use support::Fixture;

#[test]
fn missing_script_under_sh_is_unrunnable_and_creates_no_task() {
    let fx = Fixture::new();
    let out = fx.aaf(&[
        "board",
        "add",
        "--title",
        "typo bug",
        "--spec",
        "x",
        "--accept",
        "y",
        "--repro-cmd",
        "sh",
        "--repro-arg",
        "missing.sh",
    ]);
    assert_eq!(out.code, 2, "{}", out.stderr);
    assert_eq!(out.json["status"], "unrunnable", "{}", out.json);
    assert!(
        out.json["reason"].as_str().unwrap_or("").contains("127"),
        "{}",
        out.json
    );
    assert_eq!(fx.aaf(&["board", "list"]).json["count"], 0);
}

#[test]
fn confirmed_and_flaky_report_last_exit_code_and_stderr_tail() {
    let fx = Fixture::new();
    let out = fx.aaf(&[
        "repro",
        "--",
        "sh",
        "-c",
        "for i in 1 2 3 4 5 6 7 8 9 10 11 12; do echo line$i >&2; done; exit 3",
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["status"], "confirmed");
    assert_eq!(out.json["last_exit_code"], 3, "{}", out.json);
    let tail: Vec<&str> = out.json["stderr_tail"]
        .as_array()
        .unwrap_or_else(|| panic!("stderr_tail missing: {}", out.json))
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(tail.len(), 10, "{tail:?}");
    assert_eq!(tail.first(), Some(&"line3"));
    assert_eq!(tail.last(), Some(&"line12"));

    let counter = fx.tmp.path().join("flaky-counter");
    let script = format!(
        "if [ -f '{c}' ]; then exit 0; fi; : > '{c}'; echo flaky-err >&2; exit 4",
        c = counter.display()
    );
    let flaky = fx.aaf(&["repro", "--", "sh", "-c", &script]);
    assert_eq!(flaky.code, 2, "{}", flaky.stderr);
    assert_eq!(flaky.json["status"], "flaky");
    assert_eq!(flaky.json["last_exit_code"], 4, "{}", flaky.json);
    assert_eq!(flaky.json["stderr_tail"][0], "flaky-err", "{}", flaky.json);
}

#[test]
fn bad_blocked_by_fails_before_the_repro_command_runs() {
    let fx = Fixture::new();
    let marker = fx.tmp.path().join("repro-ran");
    let script = format!(": > '{}'; exit 1", marker.display());
    let out = fx.aaf(&[
        "board",
        "add",
        "--title",
        "blocked bug",
        "--spec",
        "x",
        "--accept",
        "y",
        "--blocked-by",
        "t-00000000",
        "--repro-cmd",
        "sh",
        "--repro-arg=-c",
        "--repro-arg",
        &script,
    ]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(
        !marker.exists(),
        "the repro command ran although --blocked-by was invalid"
    );
    assert_eq!(fx.aaf(&["board", "list"]).json["count"], 0);
}
