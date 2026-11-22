//! `aaf doctor` must not say "ok" where `aaf run` or `aaf gate` will fail or
//! run unguarded: a hook registration that cannot block, a repository git
//! refuses to read, a git too old for the fingerprint.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "cli_support.rs"]
mod support;

use serde_json::{Value, json};
use support::{Sandbox, make_executable, write};

fn check<'a>(out: &'a Value, name: &str) -> &'a Value {
    out["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == json!(name))
        .unwrap_or_else(|| panic!("no check {name}: {out}"))
}

fn registered(sb: &Sandbox, command: &str) -> Value {
    json!({"hooks": {"PreToolUse": [{
        "matcher": "Bash|Write|Edit|MultiEdit|Read",
        "hooks": [{"type": "command", "command": command.replace("{hook}", sb.bin.join("aaf-hook").to_str().unwrap())}]
    }]}})
}

fn doctor_with_settings(settings: &Value) -> (i32, Value) {
    let sb = Sandbox::new();
    write(
        &sb.repo.join(".claude/settings.json"),
        &settings.to_string(),
    );
    sb.json(&["doctor"])
}

#[test]
fn doctor_accepts_a_plain_registration() {
    let sb = Sandbox::new();
    let settings = registered(&sb, "{hook}");
    write(
        &sb.repo.join(".claude/settings.json"),
        &settings.to_string(),
    );
    let (_, out) = sb.json(&["doctor"]);
    assert_eq!(check(&out, "hook_registered")["ok"], json!(true), "{out}");
    assert_eq!(check(&out, "hook_binary")["ok"], json!(true), "{out}");
}

#[test]
fn doctor_fails_a_registration_whose_command_discards_the_verdict() {
    let sb = Sandbox::new();
    let (code, out) = doctor_with_settings(&registered(&sb, "{hook} >/dev/null 2>&1 || true"));
    assert_eq!(code, 2, "{out}");
    let c = check(&out, "hook_registered");
    assert_eq!(c["ok"], json!(false), "{out}");
    assert!(
        c["detail"].as_str().unwrap().contains("only the program"),
        "{out}"
    );
}

#[test]
fn doctor_fails_when_all_hooks_are_disabled() {
    let sb = Sandbox::new();
    let mut settings = registered(&sb, "{hook}");
    settings["disableAllHooks"] = json!(true);
    let (code, out) = doctor_with_settings(&settings);
    assert_eq!(code, 2, "{out}");
    let c = check(&out, "hook_registered");
    assert_eq!(c["ok"], json!(false), "{out}");
    assert!(
        c["detail"].as_str().unwrap().contains("disableAllHooks"),
        "{out}"
    );
}

#[test]
fn doctor_fails_when_the_local_settings_file_overrides_the_hooks() {
    let sb = Sandbox::new();
    write(
        &sb.repo.join(".claude/settings.json"),
        &registered(&sb, "{hook}").to_string(),
    );
    let local = sb.repo.join(".claude/settings.local.json");
    write(&local, r#"{"disableAllHooks": true}"#);
    let (code, out) = sb.json(&["doctor"]);
    assert_eq!(code, 2, "{out}");
    let c = check(&out, "hook_registered");
    assert_eq!(c["ok"], json!(false), "{out}");
    assert!(
        c["detail"]
            .as_str()
            .unwrap()
            .contains("settings.local.json"),
        "{out}"
    );

    // A local file that leaves the hooks alone is the user's own business.
    write(&local, r#"{"permissions": {"allow": ["Bash(ls:*)"]}}"#);
    let (_, out) = sb.json(&["doctor"]);
    assert_eq!(check(&out, "hook_registered")["ok"], json!(true), "{out}");
}

#[test]
fn doctor_warns_when_the_repo_list_does_not_lock_the_agents_own_files() {
    let sb = Sandbox::new();
    // As an `aaf init` before these globs existed wrote it.
    write(
        &sb.repo.join(".autofactory/config.toml"),
        "[gate]\nhead_lock_globs = [\"**/tests/**\"]\n",
    );
    let (_, out) = sb.json(&["doctor"]);
    let c = check(&out, "head_lock_agent_files");
    assert_eq!(c["ok"], json!(false), "{out}");
    assert_eq!(c["required"], json!(false), "{out}");
    let detail = c["detail"].as_str().unwrap();
    assert!(detail.contains("**/.claude/**"), "{detail}");
    assert!(detail.contains("**/CLAUDE.md"), "{detail}");

    // The defaults (no key in the file) lock them.
    write(
        &sb.repo.join(".autofactory/config.toml"),
        "[gate]\ncommand_timeout_secs = 60\n",
    );
    let (_, out) = sb.json(&["doctor"]);
    assert_eq!(
        check(&out, "head_lock_agent_files")["ok"],
        json!(true),
        "{out}"
    );
}

/// A `git` first on PATH that answers `--version` and fails everything else
/// the way git does for a repository it refuses to read.
fn refusing_git(sb: &Sandbox, version: &str) {
    let git = sb.bin.join("git");
    write(
        &git,
        &format!(
            "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = --version ]; then echo 'git version {version}'; exit 0; fi\ndone\necho 'fatal: detected dubious ownership in repository' >&2\nexit 128\n"
        ),
    );
    make_executable(&git);
}

#[test]
fn doctor_fails_when_git_refuses_a_repository_that_is_there() {
    let sb = Sandbox::new();
    refusing_git(&sb, "2.50.1");

    let (code, out) = sb.json(&["doctor"]);

    // `aaf gate` and `aaf run` fail here; "not applicable, exit 0" was wrong.
    assert_eq!(code, 2, "{out}");
    let c = check(&out, "repo_root");
    assert_eq!(c["applicable"], json!(true), "{out}");
    assert_eq!(c["required"], json!(true), "{out}");
    assert_eq!(c["ok"], json!(false), "{out}");
    let detail = c["detail"].as_str().unwrap();
    assert!(detail.contains(".git"), "{detail}");
    assert!(detail.contains("safe.directory"), "{detail}");
}

#[test]
fn doctor_fails_on_a_broken_git_file() {
    let sb = Sandbox::without_git();
    write(&sb.repo.join(".git"), "gitdir: /nonexistent/aaf-test\n");

    let (code, out) = sb.json(&["doctor"]);

    assert_eq!(code, 2, "{out}");
    assert_eq!(check(&out, "repo_root")["ok"], json!(false), "{out}");
    assert_eq!(check(&out, "repo_root")["applicable"], json!(true), "{out}");
}

#[test]
fn doctor_fails_on_a_git_older_than_the_fingerprint_needs() {
    let sb = Sandbox::without_git();
    refusing_git(&sb, "2.33.9");

    let (code, out) = sb.json(&["doctor"]);

    assert_eq!(code, 2, "{out}");
    let c = check(&out, "git");
    assert_eq!(c["ok"], json!(false), "{out}");
    let detail = c["detail"].as_str().unwrap();
    assert!(detail.contains("2.33.9"), "{detail}");
    assert!(detail.contains("2.34"), "{detail}");
}

#[test]
fn doctor_accepts_a_git_that_is_new_enough_and_says_which() {
    let sb = Sandbox::without_git();
    refusing_git(&sb, "2.34.0 (Apple Git-1)");

    let (code, out) = sb.json(&["doctor"]);

    assert_eq!(code, 0, "{out}");
    let c = check(&out, "git");
    assert_eq!(c["ok"], json!(true), "{out}");
    assert!(c["detail"].as_str().unwrap().contains("2.34.0"), "{out}");
}
