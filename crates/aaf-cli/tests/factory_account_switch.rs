#![allow(clippy::unwrap_used, clippy::expect_used)]
//! End-to-end binary test for Claude account switching on rate limit:
//! a fake `claude` binary on PATH reads `$CLAUDE_CONFIG_DIR`; account A emits a
//! `rate_limit_event` (`status: "rejected"`), so `aaf run --once --unsandboxed`
//! switches to account B and drives the task all the way to `review`.

#[path = "factory_support.rs"]
mod support;

use std::fs;
use std::path::Path;

use serde_json::json;
use support::{Fixture, git};

const CLAUDE_HOOK_REGISTERED: &str = r#"{"hooks":{"PreToolUse":[{"matcher":"Bash|Write|Edit|MultiEdit|Read","hooks":[{"type":"command","command":"aaf-hook"}]}]}}"#;

#[cfg(unix)]
fn write_executable(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, body).unwrap();
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

#[cfg(unix)]
fn make_claude_account_dir(home: &Path, name: &str) {
    let dir = home.join(name);
    fs::create_dir_all(dir.join("projects")).unwrap();
    fs::write(dir.join(".claude.json"), "{}\n").unwrap();
}

#[cfg(unix)]
#[test]
fn binary_switches_claude_account_when_first_account_hits_rate_limit() {
    let fx = Fixture::new();
    let home = fx.root().join("home");
    make_claude_account_dir(&home, ".claude-a");
    make_claude_account_dir(&home, ".claude-b");

    // Put fake `claude` and fake `aaf-hook` at the front of PATH.
    let bin_dir = fx.root().join("fake-bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let calls_log = fx.root().join("claude-calls.log");
    let fake_claude = format!(
        r#"#!/bin/sh
set -eu
cfg="${{CLAUDE_CONFIG_DIR:-none}}"
printf '%s\n' "$cfg" >> {log}
case "$cfg" in
  */.claude-a)
    printf '%s\n' '{{"type":"rate_limit_event","rate_limit_info":{{"status":"rejected","resetsAt":4102444800}}}}'
    printf '%s\n' '{{"type":"result","result":"rate limit reached","is_error":true}}'
    exit 1
    ;;
  */.claude-b)
    printf 'switched\n' > switched.txt
    printf '%s\n' '{{"type":"result","result":"done","is_error":false,"usage":{{"input_tokens":12,"output_tokens":6}}}}'
    exit 0
    ;;
  *)
    printf '%s\n' '{{"type":"result","result":"unexpected account","is_error":true}}'
    exit 2
    ;;
esac
"#,
        log = calls_log.display()
    );
    write_executable(&bin_dir.join("claude"), &fake_claude);
    write_executable(&bin_dir.join("aaf-hook"), "#!/bin/sh\nexit 0\n");

    // Configure repo for Claude executor + registered hook.
    fs::create_dir_all(fx.repo.join(".autofactory")).unwrap();
    fs::create_dir_all(fx.repo.join(".claude")).unwrap();
    fs::write(
        fx.repo.join(".claude/settings.json"),
        CLAUDE_HOOK_REGISTERED,
    )
    .unwrap();
    fs::write(
        fx.repo.join(".autofactory/config.toml"),
        "[commands]\ntest_scoped = \"test -f switched.txt\"\n\n\
         [run]\nexecutor = \"claude\"\nrun_timeout_secs = 120\n",
    )
    .unwrap();
    git(&fx.repo, &["add", "-A"]);
    git(&fx.repo, &["commit", "--quiet", "-m", "configure claude"]);

    let base_path = std::env::var("PATH").unwrap_or_default();
    let test_path = format!("{}:{base_path}", bin_dir.display());
    let env = [("PATH", test_path.as_str())];

    let id = fx.add("switch claude account on rate limit");
    let run_out = fx.aaf_in(&fx.repo, &["run", "--once", "--unsandboxed"], &env);
    assert_eq!(run_out.code, 0, "aaf run failed: {}", run_out.stderr);
    assert_eq!(fx.state(&id), "review", "task did not reach review");

    let shown = fx.show(&id);
    let runs = shown["runs"].as_array().unwrap();
    assert!(
        runs.len() >= 2,
        "expected >= 2 runs (rate-limited + switched), got: {shown}"
    );
    assert!(
        runs.iter().any(|r| r["status"] == json!("succeeded")),
        "expected a succeeded run after switch: {shown}"
    );
    let accounts: std::collections::HashSet<&str> = runs
        .iter()
        .filter_map(|r| r["account_id"].as_str())
        .collect();
    assert!(
        accounts.len() >= 2,
        "expected >= 2 distinct account_ids across runs, got {accounts:?}: {shown}"
    );

    let log_text = fs::read_to_string(&calls_log).unwrap_or_default();
    assert!(
        log_text.contains(".claude-a") && log_text.contains(".claude-b"),
        "expected both accounts to be invoked, log:\n{log_text}"
    );
    assert_eq!(
        git(&fx.repo, &["show", &format!("aaf/{id}:switched.txt")]),
        "switched"
    );
}
