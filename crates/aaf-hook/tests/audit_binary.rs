#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The auditor's exact inputs against the built binary: each must exit 2.

use std::fs;
use std::io::Write;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

fn exit_code(base: &Path, payload: &Value) -> Option<i32> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_aaf-hook"))
        .env_clear()
        .env("AAF_NO_BUILTIN_DEFAULTS", "1")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("AAF_DATA_DIR", base.join("data"))
        .env("AAF_CONFIG_DIR", base.join("config"))
        .env("HOME", base.join("home"))
        .current_dir(base.join("work"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // The hook may answer before reading everything; a closed pipe is fine.
    let _ = child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes());
    child.wait().unwrap().code()
}

#[test]
fn every_audited_input_exits_2_and_its_neighbour_exits_0() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    for sub in ["data", "config", "home", "work"] {
        fs::create_dir(base.join(sub)).unwrap();
    }
    let work = base.join("work");
    fs::write(work.join(".env"), "x").unwrap();
    symlink(".env", work.join("env_link")).unwrap();
    symlink(base.join("data"), work.join("data_link")).unwrap();
    let data = base.join("data").to_string_lossy().into_owned();

    let bash = |cwd: &Path, command: &str| {
        json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": command},
            "cwd": cwd,
        })
    };
    let file = |tool: &str, path: &str| {
        json!({
            "hook_event_name": "PreToolUse",
            "tool_name": tool,
            "tool_input": {"file_path": path},
            "cwd": work,
        })
    };
    let dynamic_aaf = format!("$DIR/aaf cat {data}/receipt.key");
    let cases: Vec<(&str, Value, i32)> = vec![
        (
            "1 push --no-dry-run",
            bash(&work, "git push --dry-run --no-dry-run --force origin main"),
            2,
        ),
        (
            "2a --repo= +refspec",
            bash(&work, "git push --repo=origin +HEAD:main"),
            2,
        ),
        (
            "2b --repo :main",
            bash(&work, "git push --repo origin :main"),
            2,
        ),
        (
            "3 -c alias",
            bash(&work, "git -c alias.f='push --force' f origin main"),
            2,
        ),
        ("4a Write symlink", file("Write", "env_link"), 2),
        ("4b Edit symlink", file("Edit", "env_link"), 2),
        ("4c MultiEdit symlink", file("MultiEdit", "env_link"), 2),
        ("4d redirect symlink", bash(&work, "echo x > env_link"), 2),
        (
            "4e truncate symlink",
            bash(&work, "truncate -s 0 env_link"),
            2,
        ),
        ("4f rm symlink", bash(&work, "rm env_link"), 2),
        (
            "5 find -delete",
            bash(&work, "find . -name .env -delete"),
            2,
        ),
        ("6 relative data path", bash(&base, "cat data/aaf.db"), 2),
        ("7 data symlink", bash(&work, "cat ./data_link/aaf.db"), 2),
        (
            "8a /tmp/evil/aaf",
            bash(&work, "/tmp/evil/aaf cat $AAF_DATA_DIR/receipt.key"),
            2,
        ),
        (
            "8b aaf redirect",
            bash(&work, "aaf < $AAF_DATA_DIR/receipt.key"),
            2,
        ),
        ("9 dynamic aaf", bash(&work, &dynamic_aaf), 2),
        (
            "10 eas option before submit",
            bash(&work, "eas --profile prod submit"),
            2,
        ),
        // The neighbours that must stay allowed.
        (
            "1' dry run last",
            bash(&work, "git push --no-dry-run --dry-run --force origin main"),
            0,
        ),
        (
            "2' --repo feature",
            bash(&work, "git push --repo=origin feature/x"),
            0,
        ),
        (
            "3' harmless alias",
            bash(&work, "git -c alias.st=status st"),
            0,
        ),
        (
            "5' find tmp",
            bash(&work, "find . -name '*.tmp' -delete"),
            0,
        ),
        ("8' bare aaf", bash(&work, "aaf gate --release"), 0),
        ("10' eas build", bash(&work, "eas build --profile prod"), 0),
    ];
    let mut wrong = Vec::new();
    for (name, payload, expected) in &cases {
        let got = exit_code(&base, payload);
        println!("case {name}: exit {got:?}");
        if got != Some(*expected) {
            wrong.push(format!("{name}: expected exit {expected}, got {got:?}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
