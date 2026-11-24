#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(target_os = "macos")]
//! Wave 3, seen with a real account: profile directories link their session
//! state (`projects`, `history.jsonl`, …) into one shared `~/.claude`. The
//! sandbox opened the profile directory only, so the agent's transcript was
//! silently never written. The real `sandbox-exec` with a stand-in `claude`.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::ports::{AgentRequest, AgentRunner, SandboxPolicy};
use aaf_engine::executors::ExecEnv;
use aaf_engine::executors::claude::ClaudeCodeRunner;

#[path = "../../aaf-fp/tests/exec_support.rs"]
mod exec_support;
use exec_support::write_executable;

#[test]
fn linked_session_state_is_writable_and_linked_settings_are_not() {
    if !Path::new("/usr/bin/sandbox-exec").exists() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let (shared, account, worktree) = (root.join("shared"), root.join("acct"), root.join("wt"));
    for d in [
        shared.join("projects"),
        shared.join("hooks"),
        account.clone(),
        worktree.clone(),
    ] {
        fs::create_dir_all(d).unwrap();
    }
    fs::write(shared.join("history.jsonl"), "").unwrap();
    fs::write(shared.join("settings.json"), "{}").unwrap();
    for name in ["projects", "history.jsonl", "settings.json", "hooks"] {
        symlink(shared.join(name), account.join(name)).unwrap();
    }

    // Stands in for Claude Code: writes where the real one keeps its state.
    let claude = root.join("claude");
    let script = "#!/bin/sh\n\
        echo line > \"$CLAUDE_CONFIG_DIR/projects/session.jsonl\" && echo transcript-ok\n\
        echo line >> \"$CLAUDE_CONFIG_DIR/history.jsonl\" && echo history-ok\n\
        echo own > \"$CLAUDE_CONFIG_DIR/.claude.json\" && echo own-ok\n\
        echo evil > \"$CLAUDE_CONFIG_DIR/settings.json\" || echo settings-denied\n\
        echo evil > \"$CLAUDE_CONFIG_DIR/hooks/h.sh\" || echo hooks-denied\n";
    write_executable(&claude, script);

    let env = ExecEnv {
        home: Some(root.join("home").to_string_lossy().into_owned()),
        // Nothing writable by default: the temp dir holds this whole tree.
        temp_dirs: Vec::new(),
        ..ExecEnv::default()
    };
    let outcome = ClaudeCodeRunner::new(claude.to_string_lossy(), env)
        .run(&AgentRequest {
            run_id: format!("test-{}", uuid::Uuid::new_v4()),
            prompt: "unused".to_string(),
            cwd: worktree.to_string_lossy().into_owned(),
            account: Some(AccountProfile {
                id: "claude_code-test".to_string(),
                provider: ProviderKind::ClaudeCode,
                config_dir: account.to_string_lossy().into_owned(),
                name: "test".to_string(),
            }),
            timeout_secs: 60,
            sandbox: SandboxPolicy {
                enabled: true,
                ..SandboxPolicy::default()
            },
            json_schema: None,
        })
        .unwrap();

    let log = outcome.log_tail;
    for marker in [
        "transcript-ok",
        "history-ok",
        "own-ok",
        "settings-denied",
        "hooks-denied",
    ] {
        assert!(log.contains(marker), "missing {marker}: {log}");
    }
    assert!(shared.join("projects/session.jsonl").exists());
    assert_eq!(
        fs::read_to_string(shared.join("settings.json")).unwrap(),
        "{}"
    );
    assert!(!shared.join("hooks/h.sh").exists());
}
