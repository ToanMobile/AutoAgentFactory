#![allow(clippy::unwrap_used, clippy::expect_used)]
//! SEC-071 verification: `main_branch_guard` must block interactive `Write`
//! targeting a not-yet-created subdirectory of a protected primary checkout
//! even when invoked from a linked worktree (`cwd = wt-t1`).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;

struct RepoPair {
    _dir: TempDir,
    primary: PathBuf,
    wt: PathBuf,
    data: PathBuf,
    home: PathBuf,
}

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?} failed: {out:?}");
}

impl RepoPair {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let primary = base.join("primary");
        let wt = base.join("wt-t1");
        let data = base.join("data");
        let home = base.join("home");
        for path in [&primary, &data, &home] {
            fs::create_dir_all(path).unwrap();
        }
        git(&primary, &["init", "-q", "-b", "main"]);
        fs::create_dir_all(primary.join(".autofactory")).unwrap();
        fs::write(
            primary.join(".autofactory/config.toml"),
            "[project]\nname = \"p\"\n",
        )
        .unwrap();
        fs::write(primary.join("lib.rs"), "pub fn a() {}\n").unwrap();
        git(&primary, &["add", "-A"]);
        git(&primary, &["commit", "-q", "-m", "init"]);
        git(
            &primary,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "aaf/t1",
                wt.to_str().unwrap(),
            ],
        );
        assert!(primary.join(".git").is_dir(), "primary must have .git dir");
        assert!(
            wt.join(".git").is_file(),
            "linked worktree must have .git file"
        );
        Self {
            _dir: dir,
            primary,
            wt,
            data,
            home,
        }
    }

    fn run_write(&self, file_path: &Path) -> (i32, Value, String) {
        let payload = json!({
            "tool_name": "Write",
            "tool_input": { "file_path": file_path },
            "cwd": self.wt,
        });
        let mut child = Command::new(env!("CARGO_BIN_EXE_aaf-hook"))
            .current_dir(&self.wt)
            .env_clear()
            .env("AAF_NO_BUILTIN_DEFAULTS", "1")
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("AAF_DATA_DIR", &self.data)
            .env("HOME", &self.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        let code = output.status.code().unwrap_or(-1);
        let stdout: Value = serde_json::from_slice(&output.stdout).unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        (code, stdout, stderr)
    }
}

#[test]
fn write_into_missing_dir_of_main_checkout_from_worktree_is_denied() {
    let fx = RepoPair::new();
    let target = fx.primary.join("notes/2026-11-27/a.md");
    assert!(
        !target.parent().unwrap().exists(),
        "parent dir must not exist before hook runs"
    );

    let (code, stdout, stderr) = fx.run_write(&target);
    assert_eq!(code, 2, "expected deny: stdout={stdout}, stderr={stderr}");
    assert!(stderr.contains("main_branch_guard"), "stderr={stderr}");
    assert_eq!(stdout["hookSpecificOutput"]["permissionDecision"], "deny");
    let reason = stdout["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap_or("");
    assert!(reason.contains("main_branch_guard"), "stdout={stdout}");
}

#[test]
fn write_into_existing_dir_of_main_checkout_from_worktree_is_denied() {
    let fx = RepoPair::new();
    let target = fx.primary.join("lib.rs");

    let (code, stdout, stderr) = fx.run_write(&target);
    assert_eq!(code, 2, "expected deny: stdout={stdout}, stderr={stderr}");
    assert!(stderr.contains("main_branch_guard"), "stderr={stderr}");
    assert_eq!(stdout["hookSpecificOutput"]["permissionDecision"], "deny");
    let reason = stdout["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap_or("");
    assert!(reason.contains("main_branch_guard"), "stdout={stdout}");
}

#[test]
fn write_into_missing_dir_of_own_worktree_is_allowed() {
    let fx = RepoPair::new();
    let target = fx.wt.join("notes/new/a.md");
    assert!(
        !target.parent().unwrap().exists(),
        "parent dir must not exist before hook runs"
    );

    let (code, stdout, stderr) = fx.run_write(&target);
    assert_eq!(code, 0, "expected allow: stdout={stdout}, stderr={stderr}");
    assert_eq!(stdout, json!({}));
}
