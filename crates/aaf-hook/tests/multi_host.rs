#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;

use aaf_hook::host::{
    HookHost, ToolAction, decide_for_host, extract_apply_patch_paths, parse_cli_host,
    parse_payload, render_for_host, run_for_host,
};
use aaf_hook::{Decision, HookEnv, Outcome};
use common::{Fixture, rule};
use serde_json::{Value, json};

fn with_env<T>(fixture: &Fixture, work: impl FnOnce(&HookEnv) -> T) -> T {
    let root = fixture.root.clone();
    let lookup = move |_: &Path| Some(root.clone());
    let config = |_: &Path| fixture.git_config.borrow().clone();
    let env = HookEnv {
        repo_root: &lookup,
        git_config: &config,
        data_dir: Some(fixture.data.clone()),
        config_dir: Some(fixture.config.clone()),
        home: Some(fixture.home.clone()),
        cwd: None,
        in_factory_run: fixture.in_factory_run,
        trusted_aaf_bins: fixture.trusted_aaf_bins.clone(),
    };
    work(&env)
}

fn judge_host(fixture: &Fixture, payload: &Value, host: HookHost) -> (Decision, HookHost) {
    with_env(fixture, |env| {
        decide_for_host(payload.to_string().as_bytes(), env, host)
    })
}

fn run_host_raw(fixture: &Fixture, raw: &[u8], host: HookHost) -> Outcome {
    with_env(fixture, |env| run_for_host(raw, env, host))
}

fn call(f: &Fixture, tool: &str, input: Value) -> Value {
    json!({ "tool_name": tool, "tool_input": input, "cwd": f.root })
}

fn before_tool(f: &Fixture, tool: &str, input: Value) -> Value {
    json!({ "hook_event_name": "BeforeTool", "tool_name": tool, "tool_input": input, "cwd": f.root })
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn host_arg_and_cli_parsing() {
    assert_eq!(HookHost::parse_arg("auto"), Some(HookHost::Auto));
    assert_eq!(HookHost::parse_arg("CLAUDE"), Some(HookHost::Claude));
    assert_eq!(HookHost::parse_arg(" codex "), Some(HookHost::Codex));
    assert_eq!(HookHost::parse_arg("Gemini"), Some(HookHost::Gemini));
    assert_eq!(HookHost::parse_arg("unknown"), None);
    assert_eq!(HookHost::Auto.as_str(), "auto");
    assert_eq!(HookHost::Claude.as_str(), "claude");
    assert_eq!(HookHost::Codex.as_str(), "codex");
    assert_eq!(HookHost::Gemini.as_str(), "gemini");

    for (raw, expected) in [
        (args(&[]), HookHost::Auto),
        (args(&["--host", "claude"]), HookHost::Claude),
        (args(&["--host", "codex"]), HookHost::Codex),
        (args(&["--host", "gemini"]), HookHost::Gemini),
        (args(&["--host=codex"]), HookHost::Codex),
        (args(&["--host=auto"]), HookHost::Auto),
    ] {
        assert_eq!(parse_cli_host(&raw), Ok(expected));
    }

    for bad in [
        args(&["--host"]),
        args(&["--host", "invalid"]),
        args(&["--host=invalid"]),
        args(&["--host="]),
        args(&["--unknown"]),
        args(&["--host", "codex", "extra"]),
    ] {
        assert!(parse_cli_host(&bad).is_err(), "expected Err for {bad:?}");
    }
}

#[test]
fn auto_detection_and_cwd_extraction() {
    let parsed = parse_payload(
        json!({ "hook_event_name": "BeforeTool", "tool_name": "custom_tool", "workspace_root": "/tmp/ws" })
            .to_string()
            .as_bytes(),
        HookHost::Auto,
    )
    .unwrap();
    assert_eq!(parsed.resolved_host, HookHost::Gemini);
    assert_eq!(parsed.cwd.as_deref(), Some("/tmp/ws"));
    assert_eq!(parsed.action, ToolAction::Unknown);

    let parsed = parse_payload(
        json!({
            "tool_name": "run_shell_command",
            "tool_input": { "command": "echo hi" },
            "cwd": "",
            "workspace_root": "/tmp/ws2"
        })
        .to_string()
        .as_bytes(),
        HookHost::Auto,
    )
    .unwrap();
    assert_eq!(parsed.resolved_host, HookHost::Gemini);
    assert_eq!(parsed.cwd.as_deref(), Some("/tmp/ws2"));
    assert_eq!(parsed.action, ToolAction::Shell("echo hi".into()));

    let parsed = parse_payload(
        json!({ "tool_name": "exec_command", "tool_input": { "cmd": "git status" }, "cwd": "/tmp/repo" })
            .to_string()
            .as_bytes(),
        HookHost::Auto,
    )
    .unwrap();
    assert_eq!(parsed.resolved_host, HookHost::Codex);
    assert_eq!(parsed.cwd.as_deref(), Some("/tmp/repo"));
    assert_eq!(parsed.action, ToolAction::Shell("git status".into()));

    let parsed = parse_payload(
        json!({ "tool_name": "Bash", "tool_input": { "command": "cargo check" }, "cwd": "/tmp/repo" })
            .to_string()
            .as_bytes(),
        HookHost::Auto,
    )
    .unwrap();
    assert_eq!(parsed.resolved_host, HookHost::Claude);
    assert_eq!(parsed.action, ToolAction::Shell("cargo check".into()));

    // Explicit host overrides auto-detection for shared tool names.
    let parsed = parse_payload(
        json!({ "tool_name": "write_file", "tool_input": { "path": "src/main.rs" } })
            .to_string()
            .as_bytes(),
        HookHost::Codex,
    )
    .unwrap();
    assert_eq!(parsed.resolved_host, HookHost::Codex);
    assert_eq!(
        parsed.action,
        ToolAction::FileWrite(vec!["src/main.rs".into()])
    );
}

#[test]
fn claude_code_allow_and_deny() {
    let f = Fixture::new();

    let (dec, host) = judge_host(
        &f,
        &call(&f, "Bash", json!({"command": "git status"})),
        HookHost::Auto,
    );
    assert_eq!((dec, host), (Decision::Allow, HookHost::Claude));

    let (dec, host) = judge_host(
        &f,
        &call(
            &f,
            "Bash",
            json!({"command": "git push --force origin main"}),
        ),
        HookHost::Claude,
    );
    assert_eq!(host, HookHost::Claude);
    assert_eq!(rule(&dec), Some("git_guard"));

    let (dec, _) = judge_host(
        &f,
        &call(&f, "Read", json!({"file_path": ".env"})),
        HookHost::Claude,
    );
    assert_eq!(rule(&dec), Some("secret_guard"));

    let (dec, _) = judge_host(
        &f,
        &call(&f, "Write", json!({"file_path": "src/lib.rs"})),
        HookHost::Claude,
    );
    assert_eq!(dec, Decision::Allow);

    let (dec, _) = judge_host(
        &f,
        &call(&f, "Edit", json!({"file_path": ".git/config"})),
        HookHost::Claude,
    );
    assert_eq!(rule(&dec), Some("git_guard"));
}

#[test]
fn codex_shell_and_argv_array_allow_and_deny() {
    let f = Fixture::new();

    let (dec, host) = judge_host(
        &f,
        &call(&f, "shell", json!({"command": "cargo test --lib"})),
        HookHost::Auto,
    );
    assert_eq!((dec, host), (Decision::Allow, HookHost::Codex));

    let (dec, host) = judge_host(
        &f,
        &call(&f, "shell_command", json!({"command": "cat .env"})),
        HookHost::Auto,
    );
    assert_eq!(host, HookHost::Codex);
    assert_eq!(rule(&dec), Some("secret_guard"));

    let (dec, host) = judge_host(
        &f,
        &call(&f, "exec_command", json!({"cmd": "git push --force"})),
        HookHost::Auto,
    );
    assert_eq!(host, HookHost::Codex);
    assert_eq!(rule(&dec), Some("git_guard"));

    // Argv array forms: allow safe, deny wrapped `bash -lc`, deny secret read, fail-closed bad arrays.
    let (dec, _) = judge_host(
        &f,
        &call(
            &f,
            "shell",
            json!({"command": ["git", "status", "--short"]}),
        ),
        HookHost::Codex,
    );
    assert_eq!(dec, Decision::Allow);

    let (dec, _) = judge_host(
        &f,
        &call(
            &f,
            "shell",
            json!({"command": ["bash", "-lc", "git push --force"]}),
        ),
        HookHost::Codex,
    );
    assert_eq!(rule(&dec), Some("git_guard"));

    let (dec, _) = judge_host(
        &f,
        &call(&f, "shell", json!({"command": ["cat", ".env"]})),
        HookHost::Codex,
    );
    assert_eq!(rule(&dec), Some("secret_guard"));

    for bad in [json!({"command": []}), json!({"command": ["echo", 42]})] {
        let (dec, _) = judge_host(&f, &call(&f, "shell", bad), HookHost::Codex);
        assert_eq!(rule(&dec), Some("hook_input"));
    }
}

#[test]
fn codex_apply_patch_and_file_tools() {
    let f = Fixture::new();

    let patch = "*** Begin Patch\r\n\
        *** Update File: src/a.rs\r\n@@\r\n-old\r\n+new\r\n\
        *** Move to: src/b.rs\r\n\
        *** Add File: src/c.rs\r\n+hello\r\n\
        *** Delete File: src/d.rs\r\n\
        *** End Patch\r\n";
    assert_eq!(
        extract_apply_patch_paths(patch),
        vec!["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"]
    );

    let (dec, host) = judge_host(
        &f,
        &call(&f, "apply_patch", json!({"patch": patch})),
        HookHost::Auto,
    );
    assert_eq!((dec, host), (Decision::Allow, HookHost::Codex));

    let data_patch = format!(
        "*** Begin Patch\n*** Add File: {}/evil.txt\n+x\n*** End Patch\n",
        f.data.display()
    );
    let cases = [
        (
            json!({"input": "*** Begin Patch\n*** Update File: src/a.rs\n+ok\n*** Update File: .env\n+X=1\n*** End Patch\n"}),
            "delete_guard",
        ),
        (
            json!("*** Begin Patch\n*** Delete File: .env\n*** End Patch\n"),
            "delete_guard",
        ),
        (
            json!({"patch": "*** Begin Patch\n*** Update File: src/a.rs\n*** Move to: .git/config\n*** End Patch\n"}),
            "git_guard",
        ),
        (json!({"patch": data_patch}), "data_dir_guard"),
        (json!({}), "hook_input"),
        (
            json!({"patch": "--- a/.env\n+++ b/.env\n+X=1\n"}),
            "hook_input",
        ),
    ];
    for (input, expected) in cases {
        let (dec, _) = judge_host(&f, &call(&f, "apply_patch", input), HookHost::Codex);
        assert_eq!(rule(&dec), Some(expected));
    }

    let (dec, _) = judge_host(
        &f,
        &call(&f, "read_file", json!({"path": ".env"})),
        HookHost::Codex,
    );
    assert_eq!(rule(&dec), Some("secret_guard"));

    let (dec, _) = judge_host(
        &f,
        &call(&f, "write_file", json!({"path": "src/ok.rs"})),
        HookHost::Codex,
    );
    assert_eq!(dec, Decision::Allow);
}

#[test]
fn gemini_before_tool_allow_and_deny() {
    let f = Fixture::new();
    f.write(".env", "SECRET=1\n");

    let (dec, host) = judge_host(
        &f,
        &before_tool(&f, "run_shell_command", json!({"command": "git status"})),
        HookHost::Auto,
    );
    assert_eq!((dec, host), (Decision::Allow, HookHost::Gemini));

    let (dec, host) = judge_host(
        &f,
        &before_tool(
            &f,
            "run_shell_command",
            json!({"command": "git push --force"}),
        ),
        HookHost::Auto,
    );
    assert_eq!(host, HookHost::Gemini);
    assert_eq!(rule(&dec), Some("git_guard"));

    let (dec, _) = judge_host(
        &f,
        &before_tool(&f, "replace", json!({"file_path": "src/lib.rs"})),
        HookHost::Gemini,
    );
    assert_eq!(dec, Decision::Allow);

    let (dec, _) = judge_host(
        &f,
        &before_tool(&f, "write_file", json!({"file_path": ".git/config"})),
        HookHost::Gemini,
    );
    assert_eq!(rule(&dec), Some("git_guard"));

    let (dec, _) = judge_host(
        &f,
        &before_tool(&f, "read_file", json!({"path": "src/lib.rs"})),
        HookHost::Gemini,
    );
    assert_eq!(dec, Decision::Allow);

    let (dec, _) = judge_host(
        &f,
        &before_tool(&f, "read_file", json!({"path": ".env"})),
        HookHost::Gemini,
    );
    assert_eq!(rule(&dec), Some("secret_guard"));

    // `read_many_files` allow & deny (secret file, AAF_DATA_DIR, glob, include, missing paths).
    let (dec, host) = judge_host(
        &f,
        &before_tool(
            &f,
            "read_many_files",
            json!({"paths": ["src/a.rs", "src/b.rs"]}),
        ),
        HookHost::Auto,
    );
    assert_eq!((dec, host), (Decision::Allow, HookHost::Gemini));

    let data_db = format!("{}/aaf.db", f.data.display());
    for (input, expected) in [
        (json!({"paths": ["src/a.rs", ".env"]}), "secret_guard"),
        (json!({"paths": ["src/a.rs", data_db]}), "data_dir_guard"),
        (json!({"paths": [".env*"]}), "secret_guard"),
        (
            json!({"paths": ["src/a.rs"], "include": [".env"]}),
            "secret_guard",
        ),
        (json!({}), "hook_input"),
    ] {
        let (dec, _) = judge_host(
            &f,
            &before_tool(&f, "read_many_files", input),
            HookHost::Gemini,
        );
        assert_eq!(rule(&dec), Some(expected));
    }
}

#[test]
fn render_schemas_and_fail_closed_malformed_json() {
    let f = Fixture::new();

    for host in [
        HookHost::Auto,
        HookHost::Claude,
        HookHost::Codex,
        HookHost::Gemini,
    ] {
        let out = render_for_host(&Decision::Allow, host);
        assert_eq!(
            (out.code, out.stdout.as_str(), out.stderr.as_str()),
            (0, "{}", "")
        );
    }

    let codex_deny = run_host_raw(
        &f,
        call(&f, "shell", json!({"command": "git push --force"}))
            .to_string()
            .as_bytes(),
        HookHost::Auto,
    );
    assert_eq!(codex_deny.code, 2);
    let body: Value = serde_json::from_str(&codex_deny.stdout).unwrap();
    assert!(body.get("decision").is_none());
    assert_eq!(body["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    assert_eq!(body["hookSpecificOutput"]["permissionDecision"], "deny");
    assert_eq!(
        body["hookSpecificOutput"]["permissionDecisionReason"],
        codex_deny.stderr
    );

    let gemini_deny = run_host_raw(
        &f,
        before_tool(
            &f,
            "run_shell_command",
            json!({"command": "git push --force"}),
        )
        .to_string()
        .as_bytes(),
        HookHost::Auto,
    );
    assert_eq!(gemini_deny.code, 2);
    let body: Value = serde_json::from_str(&gemini_deny.stdout).unwrap();
    assert_eq!(body["decision"], "deny");
    assert_eq!(body["reason"], gemini_deny.stderr);
    assert_eq!(body["hookSpecificOutput"]["hookEventName"], "BeforeTool");
    assert_eq!(body["hookSpecificOutput"]["permissionDecision"], "deny");
    assert_eq!(
        body["hookSpecificOutput"]["permissionDecisionReason"],
        gemini_deny.stderr
    );

    for raw in [
        b"not json".as_slice(),
        b"[1, 2, 3]".as_slice(),
        b"".as_slice(),
    ] {
        let out = run_host_raw(&f, raw, HookHost::Gemini);
        assert_eq!(out.code, 2);
        let body: Value = serde_json::from_str(&out.stdout).unwrap();
        assert_eq!(body["decision"], "deny");
        assert_eq!(body["hookSpecificOutput"]["hookEventName"], "BeforeTool");
    }
}
