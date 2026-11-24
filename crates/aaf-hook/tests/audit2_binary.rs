#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The round-2 auditor's exact command lines against the built binary: each
//! must exit 2, and the harmless call next to it must exit 0.

use std::fs;
use std::io::Write;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

/// Runs the hook from `base/work`, as Claude Code does from the project.
fn exit_code(base: &Path, payload: &Value) -> Option<i32> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_aaf-hook"))
        .env_clear()
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
fn every_round_2_input_exits_2_and_its_neighbour_exits_0() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    for sub in ["data", "config", "home", "work"] {
        fs::create_dir(base.join(sub)).unwrap();
    }
    let work = base.join("work");
    fs::write(work.join(".env"), "x").unwrap();
    fs::write(base.join("home/notes.txt"), "x").unwrap();
    symlink(base.join("data"), work.join("data_link")).unwrap();
    symlink(base.join("home"), work.join("home_link")).unwrap();

    let bash = |command: &str| {
        json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": command},
            "cwd": work,
        })
    };
    // No `cwd` field at all: the hook falls back to its own directory.
    let bash_no_cwd = |command: &str| {
        json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": command},
        })
    };
    let file_no_cwd = |tool: &str, path: &str| {
        json!({
            "hook_event_name": "PreToolUse",
            "tool_name": tool,
            "tool_input": {"file_path": path},
        })
    };
    let cases: Vec<(&str, Value, i32)> = vec![
        (
            "1 glued -c alias",
            bash("git -calias.f='push --force' f origin main"),
            2,
        ),
        (
            "2a -c include.path",
            bash("git -c include.path=/tmp/evil.gitconfig push origin main"),
            2,
        ),
        (
            "2b GIT_CONFIG_GLOBAL prefix",
            bash("GIT_CONFIG_GLOBAL=/tmp/evil.gitconfig git push origin main"),
            2,
        ),
        (
            "2c GIT_CONFIG_SYSTEM prefix",
            bash("GIT_CONFIG_SYSTEM=/tmp/evil.gitconfig git push origin main"),
            2,
        ),
        (
            "2d env GIT_CONFIG_GLOBAL",
            bash("env GIT_CONFIG_GLOBAL=/tmp/evil.gitconfig git push origin main"),
            2,
        ),
        (
            "2e export GIT_CONFIG_GLOBAL",
            bash("export GIT_CONFIG_GLOBAL=/tmp/evil.gitconfig; git push origin main"),
            2,
        ),
        (
            "2f export GIT_CONFIG_SYSTEM",
            bash("export GIT_CONFIG_SYSTEM=/tmp/evil.gitconfig; git push origin main"),
            2,
        ),
        (
            "2g glued -c core.hooksPath",
            bash("git -ccore.hooksPath=/dev/null commit -m x"),
            2,
        ),
        (
            "3 find -exec env rm",
            bash("find . -name .env -exec env rm {} +"),
            2,
        ),
        ("4a tar glued -C", bash("tar -C./data_link -cf - aaf.db"), 2),
        (
            "4b tar spaced -C",
            bash("tar -C ./data_link -cf - aaf.db"),
            2,
        ),
        (
            "4c tar -C parent, operand into data",
            bash("tar -C .. -cf - data/aaf.db"),
            2,
        ),
        ("5a no cwd, bash", bash_no_cwd("cat ../data/aaf.db"), 2),
        ("5b no cwd, Read", file_no_cwd("Read", "../data/aaf.db"), 2),
        ("5c no cwd, rm", bash_no_cwd("rm ../work/.env"), 2),
        ("5d no cwd, Write", file_no_cwd("Write", ".env"), 2),
        ("6a aaf approve", bash("aaf approve"), 2),
        ("6b aaf board unblock", bash("aaf board unblock T-1"), 2),
        ("6c aaf board unfreeze", bash("aaf board unfreeze"), 2),
        ("7 find | xargs rm", bash("find . -name .env | xargs rm"), 2),
        ("8a gradlew task", bash("./gradlew :app:lintDebug"), 2),
        ("8b xcodebuild", bash("xcodebuild -scheme App build"), 2),
        ("8c Unity batch", bash("Unity -batchmode -runTests"), 2),
        (
            "8d slot hides nothing else",
            bash("aaf slot -- aaf approve"),
            2,
        ),
        // The neighbours that must stay allowed.
        ("8a' gradlew --version", bash("./gradlew --version"), 0),
        ("8a'' gradlew --stop", bash("./gradlew --stop"), 0),
        (
            "8a''' gradlew in the slot",
            bash("aaf slot -- ./gradlew :app:lintDebug"),
            0,
        ),
        ("8' aaf gate --full", bash("aaf gate --full"), 0),
        ("8b' xcodebuild -version", bash("xcodebuild -version"), 0),
        ("8c' Unity -version", bash("Unity -version"), 0),
        (
            "1' harmless glued alias",
            bash("git -calias.st=status st"),
            0,
        ),
        ("2' ordinary -c", bash("git -c user.name=x commit -m y"), 0),
        (
            "2'' config from /dev/null",
            bash("GIT_CONFIG_GLOBAL=/dev/null git status"),
            0,
        ),
        (
            "3' find -exec env rm tmp",
            bash("find . -name '*.tmp' -exec env rm {} +"),
            0,
        ),
        (
            "4' tar glued -C elsewhere",
            bash("tar -C./home_link -cf - notes.txt"),
            0,
        ),
        ("5a' no cwd, bash", bash_no_cwd("cat ../home/notes.txt"), 0),
        (
            "5b' no cwd, Read",
            file_no_cwd("Read", "../home/notes.txt"),
            0,
        ),
        ("5c' no cwd, rm", bash_no_cwd("rm ../home/notes.txt"), 0),
        ("6a' aaf gate", bash("aaf gate"), 0),
        ("6b' aaf board list", bash("aaf board list"), 0),
        ("6c' aaf doctor", bash("aaf doctor"), 0),
        (
            "7' find tmp | xargs rm",
            bash("find . -name '*.tmp' | xargs rm"),
            0,
        ),
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
