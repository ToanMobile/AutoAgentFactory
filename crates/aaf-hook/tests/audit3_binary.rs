#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The round-3 auditor's command lines against the built binary: each must
//! exit 2, and the harmless call next to it must exit 0.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

/// Runs the hook from `base/work`, as Claude Code does from the project.
fn exit_code(base: &Path, payload: &Value) -> Option<i32> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_aaf-hook"))
        .env_clear()
            .env("AAF_NO_BUILTIN_DEFAULTS", "1")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("AAF_RUN_ID", "test-run")
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
fn every_round_3_input_exits_2_and_its_neighbour_exits_0() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    for sub in ["data", "config", "home", "work"] {
        fs::create_dir(base.join(sub)).unwrap();
    }
    let work = base.join("work");
    fs::write(
        work.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/aaf-cli\"]\n",
    )
    .unwrap();
    fs::write(base.join("config/adb-allowlist"), "OK1\n").unwrap();

    let cases: &[(&str, &str, i32)] = &[
        (
            "1a slot variable as a prefix",
            "AAF_HEAVY_SLOT=0 aaf slot -- ./gradlew assembleDebug",
            2,
        ),
        (
            "1b export",
            "export AAF_HEAVY_SLOT=0; aaf slot -- ./gradlew assembleDebug",
            2,
        ),
        (
            "1c env",
            "env AAF_HEAVY_SLOT=0 aaf slot -- ./gradlew assembleDebug",
            2,
        ),
        ("1d declare", "declare -x AAF_HEAVY_SLOT=0:feed", 2),
        (
            "1e nested shell",
            "sh -c 'AAF_HEAVY_SLOT=0 aaf slot -- true'",
            2,
        ),
        (
            "1f substitution",
            "echo $(AAF_HEAVY_SLOT=0 aaf slot -- true)",
            2,
        ),
        ("1g env -S", "env -S 'AAF_HEAVY_SLOT=0 aaf slot -- true'", 2),
        (
            "1h name built at run time",
            "export \"$V\"=0; aaf slot -- ./gradlew assembleDebug",
            2,
        ),
        (
            "2a forged aaf, absolute",
            "/tmp/evil/aaf slot -- ./gradlew assembleDebug",
            2,
        ),
        (
            "2b forged aaf, relative",
            "./aaf slot -- ./gradlew assembleDebug",
            2,
        ),
        ("4a gradlew tasks", "./gradlew tasks", 2),
        ("4b gradlew projects", "./gradlew projects", 2),
        ("4c gradlew properties", "./gradlew properties", 2),
        ("4d gradlew help", "./gradlew help", 2),
        (
            "5a cargo run -- approve",
            "cargo run -- approve --reviewer bob",
            2,
        ),
        (
            "5b cargo run -- board unblock",
            "cargo run -- board unblock T-1",
            2,
        ),
        (
            "5c cargo run -- board unfreeze",
            "cargo run -- board unfreeze",
            2,
        ),
        (
            "7a glued serial off the allowlist",
            "adb -sFAKE123 uninstall com.app",
            2,
        ),
        ("7b fastboot wipe in a cluster", "fastboot -sOK1 -vw", 2),
        // The neighbours that must stay allowed.
        (
            "1' the queue itself",
            "aaf slot -- ./gradlew assembleDebug",
            0,
        ),
        ("1'' reading the variable", "echo $AAF_HEAVY_SLOT", 0),
        (
            "1''' another variable",
            "FOO=1 aaf slot -- ./gradlew assembleDebug",
            0,
        ),
        ("1h' run-time name, no slot", "export \"$V\"=0", 0),
        (
            "2' aaf by path, nothing heavy",
            "./target/debug/aaf doctor",
            0,
        ),
        ("4' gradlew --version", "./gradlew --version", 0),
        ("4'' gradlew --help", "./gradlew --help", 0),
        ("4''' tasks in the slot", "aaf slot -- ./gradlew tasks", 0),
        (
            "5a' another crate",
            "cargo run -p other-crate -- approve",
            0,
        ),
        ("5b' cargo run -- gate", "cargo run -- gate", 0),
        (
            "7a' glued serial on the allowlist",
            "adb -sOK1 uninstall com.app",
            0,
        ),
        ("7b' fastboot verbose", "fastboot -sOK1 -v getvar all", 0),
    ];
    let mut wrong = Vec::new();
    for (name, command, expected) in cases {
        let payload = json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": command},
            "cwd": work,
        });
        let got = exit_code(&base, &payload);
        println!("case {name}: exit {got:?}");
        if got != Some(*expected) {
            wrong.push(format!("{name}: expected exit {expected}, got {got:?}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
