#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Integration tests for v3.3 Phase 2:
//! - Safe `docs_fast_path` in `--scoped` mode (still running static secret scan)
//! - `commands.lint_full` only running in `--full` / `--release`
//! - Verified `AAF_CHANGED_FILES_FILE` / `AAF_DELETED_FILES_FILE` env vars
//! - Cross-worktree content-addressed receipt reuse

use std::fs;
use std::path::Path;
use std::process::Command;

use aaf_domain::config::{MachineConfig, RepoConfig};
use aaf_domain::gate::{GateMode, GateStatus};
use aaf_domain::ports::Clock;
use aaf_engine::gate::docs_fast::DOCS_FAST_PATH_NOTE;
use aaf_engine::gate::run_gate_observed_reporting;
use tempfile::TempDir;

struct FixedClock(i64);
impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_repo() -> TempDir {
    let dir = TempDir::new().unwrap();
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["config", "user.email", "test@example.com"]);
    git(dir.path(), &["config", "user.name", "Test"]);
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::create_dir_all(dir.path().join("docs")).unwrap();
    fs::write(dir.path().join("src/lib.rs"), "pub fn ok() -> i32 { 1 }\n").unwrap();
    fs::write(dir.path().join("docs/guide.md"), "# Guide\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-m", "init"]);
    git(dir.path(), &["checkout", "-b", "feat/work"]);
    dir
}

fn cfg_with_commands(
    lint: &str,
    lint_full: &str,
    test_scoped: &str,
    test_full: &str,
) -> RepoConfig {
    let mut cfg = RepoConfig::default();
    cfg.gate.base_ref = "main".to_string();
    cfg.commands.lint = lint.to_string();
    cfg.commands.lint_full = lint_full.to_string();
    cfg.commands.test_scoped = test_scoped.to_string();
    cfg.commands.test_full = test_full.to_string();
    cfg
}

#[test]
fn scoped_docs_fast_path_skips_commands_but_still_blocks_secrets_and_runs_full() {
    let repo = init_repo();
    let data = TempDir::new().unwrap();
    let marker = repo.path().join(".ran_cmd");
    // Write a failing test command — docs fast path should skip it cleanly in --scoped.
    let fail_cmd = format!("touch {} && exit 1", marker.display());
    let cfg = cfg_with_commands(&fail_cmd, "echo full_lint", &fail_cmd, "echo full_test");

    // 1. Safe markdown change -> scoped takes docs fast path -> Clear, 0 commands run.
    fs::write(
        repo.path().join("docs/guide.md"),
        "# Guide\nUpdated notes.\n",
    )
    .unwrap();
    let machine = MachineConfig::default();
    let noop = |_: &aaf_engine::throttle::Progress<'_>| {};
    let run = run_gate_observed_reporting(
        data.path(),
        repo.path(),
        &cfg,
        GateMode::Scoped,
        &FixedClock(100),
        &machine,
        &noop,
    )
    .unwrap();
    assert_eq!(
        run.receipt.status,
        GateStatus::Clear,
        "{:?}",
        run.receipt.reasons
    );
    assert!(run.receipt.commands.is_empty());
    assert!(
        run.receipt
            .reasons
            .iter()
            .any(|r| r.starts_with(DOCS_FAST_PATH_NOTE)),
        "{:?}",
        run.receipt.reasons
    );
    assert!(
        !marker.exists(),
        "scoped docs fast path must not execute test_scoped"
    );

    // 2. Same docs change in --full mode runs lint_full and test_full (never skips).
    let cfg_full_ok = cfg_with_commands("true", "echo full_lint", "true", "echo full_test");
    let full = run_gate_observed_reporting(
        data.path(),
        repo.path(),
        &cfg_full_ok,
        GateMode::Full,
        &FixedClock(101),
        &machine,
        &noop,
    )
    .unwrap();
    assert_eq!(full.receipt.status, GateStatus::Clear);
    let names: Vec<&str> = full
        .receipt
        .commands
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(names, vec!["lint", "lint_full", "test_full"]);

    // 3. Adding a secret inside markdown STILL blocks via static checks (SEC-001).
    let fake_aws = format!("{}AKIAIOSFODNN7EXAMPLE", "key=");
    fs::write(
        repo.path().join("docs/guide.md"),
        format!("# Guide\n{fake_aws}\n"),
    )
    .unwrap();
    let blocked = run_gate_observed_reporting(
        data.path(),
        repo.path(),
        &cfg,
        GateMode::Scoped,
        &FixedClock(102),
        &machine,
        &noop,
    )
    .unwrap();
    assert_eq!(blocked.receipt.status, GateStatus::Blocked);
    assert!(
        blocked
            .receipt
            .static_findings
            .iter()
            .any(|f| f.rule == "SEC-001"),
        "{:?}",
        blocked.receipt.static_findings
    );
}

#[test]
fn gate_passes_verified_changed_files_env_and_reuses_receipt_across_worktrees() {
    let repo = init_repo();
    let data = TempDir::new().unwrap();
    let capture_file = data.path().join("captured_env.txt");
    let wt_parent = TempDir::new().unwrap();
    let wt_path = wt_parent.path().join("sibling-wt");

    // Modify a code file and commit so both worktrees can check out the exact same commit.
    fs::write(repo.path().join("src/lib.rs"), "pub fn ok() -> i32 { 2 }\n").unwrap();
    git(repo.path(), &["add", "src/lib.rs"]);
    git(repo.path(), &["commit", "-m", "update lib"]);
    git(
        repo.path(),
        &[
            "worktree",
            "add",
            "--detach",
            wt_path.to_str().unwrap(),
            "HEAD",
        ],
    );

    let scoped_cmd = format!(
        "echo \"MODE=$AAF_GATE_MODE\" > {0} && echo \"BASE=$AAF_BASE_SHA\" >> {0} && cat \"$AAF_CHANGED_FILES_FILE\" >> {0}",
        capture_file.display()
    );
    let cfg = cfg_with_commands("", "", &scoped_cmd, "true");
    let machine = MachineConfig::default();
    let noop = |_: &aaf_engine::throttle::Progress<'_>| {};

    // First run in primary worktree executes test_scoped and sees src/lib.rs in AAF_CHANGED_FILES_FILE.
    let first = run_gate_observed_reporting(
        data.path(),
        repo.path(),
        &cfg,
        GateMode::Scoped,
        &FixedClock(200),
        &machine,
        &noop,
    )
    .unwrap();
    assert_eq!(first.receipt.status, GateStatus::Clear);
    assert!(!first.reused);
    let captured = fs::read_to_string(&capture_file).unwrap();
    assert!(captured.contains("MODE=scoped"), "{captured}");
    assert!(captured.contains("src/lib.rs"), "{captured}");

    // Second run in sibling worktree at identical tree+base+config reuses common receipt!
    fs::remove_file(&capture_file).unwrap();
    let second = run_gate_observed_reporting(
        data.path(),
        &wt_path,
        &cfg,
        GateMode::Scoped,
        &FixedClock(201),
        &machine,
        &noop,
    )
    .unwrap();
    assert_eq!(second.receipt.status, GateStatus::Clear);
    assert!(
        second.reused,
        "sibling worktree should reuse cross-worktree receipt"
    );
    assert!(
        !capture_file.exists(),
        "reused receipt must not re-run test_scoped"
    );
}
