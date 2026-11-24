//! Tests for W5a: `aaf doctor` executor hook checks (`executor_hooks`,
//! `readonly_executor_hooks`, `multi_host_hooks`) and `aaf executors list`
//! hook support labels.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "cli_support.rs"]
mod support;

use serde_json::{Value, json};
use support::{Sandbox, write};

fn check<'a>(out: &'a Value, name: &str) -> &'a Value {
    out["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == json!(name))
        .unwrap_or_else(|| panic!("no check {name}: {out}"))
}

#[test]
fn opencode_writer_without_hook_fails_required_executor_hooks() {
    let sb = Sandbox::new();
    assert_eq!(sb.json(&["init"]).0, 0);
    write(
        &sb.repo.join(".autofactory/config.toml"),
        "[run]\nexecutor = \"opencode\"\n",
    );
    sb.commit_all("configure opencode writer without hook");

    let (code, out) = sb.json(&["doctor"]);
    assert_eq!(code, 2, "{out}");
    let c = check(&out, "executor_hooks");
    assert_eq!(c["ok"], json!(false), "{c}");
    assert_eq!(c["required"], json!(true), "{c}");
    assert_eq!(c["applicable"], json!(true), "{c}");
    let detail = c["detail"].as_str().unwrap();
    assert!(detail.contains("opencode"), "{detail}");
}

#[test]
fn uncommitted_opencode_hook_still_fails_executor_hooks() {
    let sb = Sandbox::new();
    assert_eq!(sb.json(&["init"]).0, 0);
    write(
        &sb.repo.join(".autofactory/config.toml"),
        "[run]\nexecutor = \"opencode\"\n",
    );
    sb.commit_all("configure opencode writer before adding hook");

    // Register `.opencode/hooks.json` in the working tree only (uncommitted).
    assert_eq!(sb.json(&["init", "--hook-host", "opencode"]).0, 0);
    assert!(sb.repo.join(".opencode/hooks.json").is_file());

    let (code, out) = sb.json(&["doctor"]);
    assert_eq!(code, 2, "{out}");
    let c = check(&out, "executor_hooks");
    assert_eq!(c["ok"], json!(false), "{c}");
    assert_eq!(c["required"], json!(true), "{c}");
    let detail = c["detail"].as_str().unwrap();
    assert!(detail.contains("opencode"), "{detail}");
}

#[test]
fn opencode_writer_with_committed_hook_passes() {
    let sb = Sandbox::new();
    assert_eq!(sb.json(&["init", "--hook-host", "opencode"]).0, 0);
    write(
        &sb.repo.join(".autofactory/config.toml"),
        "[run]\nexecutor = \"opencode\"\n",
    );
    sb.commit_all("configure opencode writer with hook");

    let (code, out) = sb.json(&["doctor"]);
    assert_eq!(code, 0, "{out}");
    let c = check(&out, "executor_hooks");
    assert_eq!(c["ok"], json!(true), "{c}");
    assert_eq!(c["required"], json!(true), "{c}");
    let detail = c["detail"].as_str().unwrap();
    assert!(detail.contains("opencode"), "{detail}");
}

#[test]
fn hook_host_all_reports_gemini_entry_and_opencode() {
    let sb = Sandbox::new();
    assert_eq!(sb.json(&["init", "--hook-host", "all"]).0, 0);

    let (code, out) = sb.json(&["doctor"]);
    assert_eq!(code, 0, "{out}");
    let c = check(&out, "multi_host_hooks");
    assert_eq!(c["ok"], json!(true), "{c}");
    assert_eq!(c["required"], json!(false), "{c}");
    let detail = c["detail"].as_str().unwrap();
    assert!(detail.contains("codex"), "{detail}");
    assert!(detail.contains("gemini"), "{detail}");
    assert!(detail.contains("opencode"), "{detail}");
}

#[test]
fn hook_host_antigravity_is_not_reported_invalid_for_gemini() {
    let sb = Sandbox::new();
    assert_eq!(sb.json(&["init", "--hook-host", "antigravity"]).0, 0);

    let (code, out) = sb.json(&["doctor"]);
    assert_eq!(code, 0, "{out}");
    let c = check(&out, "multi_host_hooks");
    assert_eq!(c["ok"], json!(true), "{c}");
    assert_eq!(c["required"], json!(false), "{c}");
    let detail = c["detail"].as_str().unwrap();
    assert!(detail.contains("antigravity"), "{detail}");
    assert!(detail.contains("covers gemini tools"), "{detail}");
}

#[test]
fn readonly_gemini_auditor_without_hook_is_advisory_only() {
    let sb = Sandbox::new();
    assert_eq!(sb.json(&["init"]).0, 0);
    write(
        &sb.repo.join(".autofactory/config.toml"),
        "[run]\nexecutor = \"claude\"\n[cross_audit]\nexecutor = \"gemini\"\n",
    );
    sb.commit_all("configure explicit gemini cross_audit");

    let (code, out) = sb.json(&["doctor"]);
    assert_eq!(code, 0, "{out}");
    let c = check(&out, "readonly_executor_hooks");
    assert_eq!(c["ok"], json!(false), "{c}");
    assert_eq!(c["required"], json!(false), "{c}");
    assert_eq!(c["applicable"], json!(true), "{c}");
    let detail = c["detail"].as_str().unwrap();
    assert!(detail.contains("gemini"), "{detail}");
    assert!(detail.contains("aaf init --hook-host gemini"), "{detail}");
    assert!(detail.contains("not enforced by aaf run yet"), "{detail}");
}

#[test]
fn claude_only_config_reports_no_non_claude_writer() {
    let sb = Sandbox::new();
    assert_eq!(sb.json(&["init"]).0, 0);
    sb.commit_all("default claude config");

    let (code, out) = sb.json(&["doctor"]);
    assert_eq!(code, 0, "{out}");

    let exec_hooks = check(&out, "executor_hooks");
    assert_eq!(exec_hooks["ok"], json!(true), "{exec_hooks}");
    assert_eq!(exec_hooks["required"], json!(true), "{exec_hooks}");
    assert!(
        exec_hooks["detail"]
            .as_str()
            .unwrap()
            .contains("no non-claude code-writing executor"),
        "{exec_hooks}"
    );

    let ro_hooks = check(&out, "readonly_executor_hooks");
    assert_eq!(ro_hooks["ok"], json!(true), "{ro_hooks}");
    assert_eq!(ro_hooks["required"], json!(false), "{ro_hooks}");
    assert!(
        ro_hooks["detail"]
            .as_str()
            .unwrap()
            .contains("explicit read-only executors only; auto/fallback not inspected"),
        "{ro_hooks}"
    );

    let multi = check(&out, "multi_host_hooks");
    assert_eq!(multi["ok"], json!(true), "{multi}");
    let multi_detail = multi["detail"].as_str().unwrap();
    assert!(multi_detail.contains("opencode"), "{multi_detail}");
    assert!(
        multi_detail.contains("--hook-host antigravity"),
        "{multi_detail}"
    );
}

#[test]
fn outside_git_repo_marks_executor_hook_checks_not_applicable() {
    let sb = Sandbox::without_git();
    let (code, out) = sb.json(&["doctor"]);
    assert_eq!(code, 0, "{out}");
    for name in [
        "multi_host_hooks",
        "executor_hooks",
        "readonly_executor_hooks",
    ] {
        let c = check(&out, name);
        assert_eq!(c["applicable"], json!(false), "{name}: {c}");
        assert_eq!(c["required"], json!(false), "{name}: {c}");
    }
}

#[test]
fn executors_list_shows_new_hook_labels() {
    let sb = Sandbox::without_git();

    let text_out = sb.aaf(&["executors", "list"]);
    assert_eq!(text_out.status.code(), Some(0));
    let stdout = String::from_utf8(text_out.stdout).unwrap();
    assert!(stdout.contains("antigravity_before_tool"), "{stdout}");
    assert!(stdout.contains("opencode_pre_tool_use"), "{stdout}");

    let (code, out) = sb.json(&["executors", "list"]);
    assert_eq!(code, 0, "{out}");
    let list = out["executors"].as_array().unwrap();
    let hook_of = |id: &str| {
        list.iter()
            .find(|e| e["id"] == json!(id))
            .unwrap_or_else(|| panic!("missing executor {id}: {out}"))["hook_support"]
            .clone()
    };
    assert_eq!(hook_of("antigravity"), json!("AntigravityBeforeTool"));
    assert_eq!(hook_of("opencode"), json!("OpencodePreToolUse"));
}
