#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Runs the built binary: stdin JSON in, stdout/stderr/exit code out.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Sandbox {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["data", "config", "home", "work"] {
            std::fs::create_dir(dir.path().join(sub)).unwrap();
        }
        Sandbox { dir }
    }

    fn path(&self, sub: &str) -> std::path::PathBuf {
        self.dir.path().join(sub)
    }

    fn run(&self, stdin: &[u8]) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_aaf-hook"))
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("AAF_DATA_DIR", self.path("data"))
            .env("AAF_CONFIG_DIR", self.path("config"))
            .env("HOME", self.path("home"))
            .current_dir(self.path("work"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // The hook may answer before reading everything; a closed pipe is fine.
        let _ = child.stdin.take().unwrap().write_all(stdin);
        child.wait_with_output().unwrap()
    }

    fn bash(&self, command: &str) -> Output {
        let payload = json!({
            "session_id": "s",
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": command},
            "cwd": self.path("work"),
        });
        self.run(payload.to_string().as_bytes())
    }
}

fn assert_allowed(output: &Output) {
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "{}");
    assert!(output.stderr.is_empty(), "{output:?}");
}

/// Checks the whole deny contract and returns the reason.
fn assert_denied(output: &Output) -> String {
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let body: Value = serde_json::from_slice(&output.stdout).unwrap();
    let hook = &body["hookSpecificOutput"];
    assert_eq!(hook["hookEventName"], "PreToolUse");
    assert_eq!(hook["permissionDecision"], "deny");
    let reason = hook["permissionDecisionReason"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(String::from_utf8_lossy(&output.stderr).trim_end(), reason);
    let (rule, rest) = reason.split_once(": ").unwrap();
    assert!(
        !rule.is_empty() && !rule.contains(' '),
        "rule in {reason:?}"
    );
    let (why, fix) = rest.split_once(" — fix: ").unwrap();
    assert!(!why.is_empty() && !fix.is_empty(), "{reason:?}");
    reason
}

#[test]
fn allow_prints_empty_object_and_exits_zero() {
    let sandbox = Sandbox::new();
    assert_allowed(&sandbox.bash("cargo test -p aaf-hook"));
    assert_allowed(&sandbox.bash("git status"));
}

#[test]
fn deny_prints_the_decision_on_stdout_and_stderr_and_exits_two() {
    let sandbox = Sandbox::new();
    let reason = assert_denied(&sandbox.bash("git push --force origin main"));
    assert!(reason.starts_with("git_guard: "), "{reason}");
    let reason = assert_denied(&sandbox.bash("adb reboot bootloader"));
    assert!(reason.starts_with("adb_guard: "), "{reason}");
}

#[test]
fn unknown_tool_and_extra_fields_are_allowed() {
    let sandbox = Sandbox::new();
    for payload in [
        json!({"tool_name": "WebFetch", "tool_input": {"url": "https://example.invalid"}}),
        json!({"tool_name": "Glob", "tool_input": {"pattern": "**/.env"}, "cwd": "/"}),
        json!({"tool_name": "mcp__x__y", "tool_input": {"command": "git push --force"}}),
        json!({}),
    ] {
        assert_allowed(&sandbox.run(payload.to_string().as_bytes()));
    }
}

#[test]
fn data_dir_comes_from_the_environment_of_the_hook() {
    let sandbox = Sandbox::new();
    let key = sandbox.path("data").join("receipt.key");
    let payload = |tool: &str, path: &Path| {
        json!({"tool_name": tool, "tool_input": {"file_path": path}, "cwd": sandbox.path("work")})
            .to_string()
    };
    for tool in ["Read", "Write", "Edit", "MultiEdit"] {
        let reason = assert_denied(&sandbox.run(payload(tool, &key).as_bytes()));
        assert!(reason.starts_with("data_dir_guard: "), "{reason}");
    }
    let other = sandbox.path("work").join("notes.txt");
    assert_allowed(&sandbox.run(payload("Write", &other).as_bytes()));
    let command = format!("cat {}", sandbox.path("data").join("aaf.db").display());
    assert_denied(&sandbox.bash(&command));
}

#[test]
fn global_adb_lists_come_from_the_config_dir() {
    let sandbox = Sandbox::new();
    assert_allowed(&sandbox.bash("adb shell ls"));
    std::fs::write(sandbox.path("config").join("adb-denylist"), "BAD1\n").unwrap();
    let reason = assert_denied(&sandbox.bash("adb -s BAD1 shell ls"));
    assert!(reason.contains("denylist"), "{reason}");
    assert_denied(&sandbox.bash("adb shell ls"));
    assert_allowed(&sandbox.bash("adb -s GOOD1 shell ls"));
}

#[test]
fn store_submit_outside_a_repository_is_denied_with_the_gate_fix() {
    let sandbox = Sandbox::new();
    let reason = assert_denied(&sandbox.bash("fastlane deliver"));
    assert!(reason.starts_with("store_submit_guard: "), "{reason}");
    assert!(reason.ends_with("fix: aaf gate --release"), "{reason}");
}

#[test]
fn bad_stdin_exits_two() {
    let sandbox = Sandbox::new();
    let cases: [&[u8]; 9] = [
        b"",
        b"not json",
        b"{\"tool_name\": \"Bash\"",
        &[0xff, 0xfe, 0x00, 0x80, 0xc3, 0x28],
        b"[1, 2, 3]",
        b"\"Bash\"",
        b"null",
        // Bash without a command, and a command that is not a string.
        b"{\"tool_name\":\"Bash\",\"tool_input\":{}}",
        b"{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":[\"ls\"]}}",
    ];
    for case in cases {
        let reason = assert_denied(&sandbox.run(case));
        assert!(reason.starts_with("hook_input: "), "{reason}");
    }
}

/// Inputs built to crash a recursive parser. Whatever happens inside, the
/// process must answer with exit 2 (or allow harmless ones with 0), never a
/// panic code or a signal.
#[test]
fn hostile_input_never_escapes_the_exit_code_contract() {
    let sandbox = Sandbox::new();

    // JSON nested far past any recursion limit.
    let nested_json = format!("{}{}", "[".repeat(200_000), "]".repeat(200_000));
    assert_denied(&sandbox.run(nested_json.as_bytes()));

    // Substitutions, quotes and parentheses nested 50 000 deep around a
    // dangerous command.
    let deep = |open: &str, close: &str| {
        format!(
            "{}git push --force{}",
            open.repeat(50_000),
            close.repeat(50_000)
        )
    };
    for (open, close) in [
        ("$(", ")"),
        ("\"$(", ")\""),
        ("(", ")"),
        ("`", "`"),
        ("{ ", "; }"),
    ] {
        let output = sandbox.bash(&deep(open, close));
        assert_eq!(output.status.code(), Some(2), "nesting with {open:?}");
        assert_denied(&output);
    }
    assert_denied(&sandbox.bash(&format!("{}git push --force", "eval ".repeat(50_000))));

    // Garbage that is valid JSON but not valid shell.
    for command in [
        "\u{0}\u{1}\u{7f}\\",
        "$'\\U99999999\\x' \\",
        "<<<<<<<< >>>>>>> &&&& |||| ;;;;",
        "<<",
        "$( ` \" ' ${",
        "🦀 --force \u{202e}tig",
    ] {
        let code = sandbox.bash(command).status.code();
        assert!(
            matches!(code, Some(0) | Some(2)),
            "{command:?} exited {code:?}"
        );
    }

    // A megabyte of harmless words still gets an answer.
    assert_allowed(&sandbox.bash(&"echo ok; ".repeat(120_000)));
}

#[test]
fn a_panic_inside_the_guard_becomes_exit_two() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let outcome = aaf_hook::guarded(|| panic!("boom"));
    std::panic::set_hook(previous);
    assert_eq!(outcome.code, 2);
    assert!(outcome.stderr.starts_with("hook_internal: "));
    let body: Value = serde_json::from_str(&outcome.stdout).unwrap();
    assert_eq!(body["hookSpecificOutput"]["permissionDecision"], "deny");
}
