#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Edge-case tests for G7 — Manual CLI repro pre-flight.

#[path = "factory_support.rs"]
mod support;

use std::path::{Path, PathBuf};

use support::Fixture;

#[path = "../../aaf-fp/tests/exec_support.rs"]
mod exec_support;

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    exec_support::write_executable(&path, format!("#!/bin/sh\n{body}\n"));
    path
}

#[test]
fn flaky_repro_does_not_create_task() {
    let fx = Fixture::new();
    let c = fx.tmp.path().join("counter");
    let script = write_script(
        fx.tmp.path(),
        "flaky.sh",
        &format!(
            "n=0; [ -f '{c}' ] && n=$(cat '{c}'); n=$((n + 1)); printf '%s' \"$n\" > '{c}'; [ \"$n\" -le 1 ] && exit 1; exit 0",
            c = c.display()
        ),
    );
    let out = fx.aaf(&[
        "board",
        "add",
        "--title",
        "flaky bug",
        "--spec",
        "x",
        "--accept",
        "y",
        "--repro-cmd",
        script.to_str().unwrap(),
        "--repro-runs",
        "2",
    ]);
    assert_eq!(out.code, 2, "{}", out.stderr);
    assert_eq!(out.json["status"], "flaky");
    assert_eq!(fx.aaf(&["board", "list"]).json["count"], 0);
}

#[test]
fn aaf_run_id_refuses_board_add_repro_and_aaf_repro() {
    let fx = Fixture::new();
    let add = fx.aaf_in(
        &fx.repo,
        &[
            "board",
            "add",
            "--title",
            "agent bug",
            "--spec",
            "x",
            "--accept",
            "y",
            "--repro-cmd",
            "/usr/bin/false",
        ],
        &[("AAF_RUN_ID", "run-abc123")],
    );
    assert_eq!(add.code, 2, "{}", add.stderr);
    assert_eq!(add.json["status"], "refused");
    assert_eq!(fx.aaf(&["board", "list"]).json["count"], 0);

    let repro = fx.aaf_in(
        &fx.repo,
        &["repro", "--", "/usr/bin/false"],
        &[("AAF_RUN_ID", "run-xyz789")],
    );
    assert_eq!(repro.code, 2, "{}", repro.stderr);
    assert_eq!(repro.json["status"], "refused");
}

#[test]
fn repro_arg_passes_literally_without_shell_expansion() {
    let fx = Fixture::new();
    let script = write_script(
        fx.tmp.path(),
        "check_literal.sh",
        r#"if [ "$1" = '$HOME' ] && [ "$2" = 'a; exit 0' ]; then exit 1; fi; exit 0"#,
    );
    let out = fx.aaf(&[
        "board",
        "add",
        "--title",
        "literal args",
        "--spec",
        "x",
        "--accept",
        "y",
        "--repro-cmd",
        script.to_str().unwrap(),
        "--repro-arg",
        "$HOME",
        "--repro-arg",
        "a; exit 0",
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(fx.aaf(&["board", "list"]).json["count"], 1);
}

#[test]
fn repro_subcommand_json_outputs_for_confirmed_not_reproduced_and_unrunnable() {
    let fx = Fixture::new();
    let ok = fx.aaf(&["repro", "--", "/usr/bin/false"]);
    assert_eq!(ok.code, 0, "{}", ok.stderr);
    assert_eq!(ok.json["ok"], true);
    assert_eq!(ok.json["command"], "repro");
    assert_eq!(ok.json["status"], "confirmed");
    assert_eq!(ok.json["outcome"], "confirmed");
    assert_eq!(ok.json["runs"], 2);

    let nope = fx.aaf(&["repro", "--", "/usr/bin/true"]);
    assert_eq!(nope.code, 2, "{}", nope.stderr);
    assert_eq!(nope.json["ok"], false);
    assert_eq!(nope.json["status"], "not_reproduced");
    assert_eq!(nope.json["outcome"], "not_reproduced");

    let bad = fx.aaf(&["repro", "--", "/nonexistent/aaf-repro-binary-xyz"]);
    assert_eq!(bad.code, 2, "{}", bad.stderr);
    assert_eq!(bad.json["ok"], false);
    assert_eq!(bad.json["status"], "unrunnable");
    assert_eq!(bad.json["outcome"], "unrunnable");
}
