//! Docs fast path against configured commands that read docs (audit a13
//! round 3, R3-L3-2): a `lint` or `test_scoped` string that names a changed
//! doc's extension, file or directory runs instead of being skipped.
#![allow(clippy::unwrap_used, clippy::expect_used)]

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
    assert!(out.status.success(), "git {args:?} failed");
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

/// Scoped gate over a `docs/guide.md` edit with the given commands: (status,
/// names of the commands that ran, whether the fast-path note was added).
fn scoped(lint: &str, test_scoped: &str, doc_body: &str) -> (GateStatus, Vec<String>, bool) {
    let repo = init_repo();
    let data = TempDir::new().unwrap();
    fs::write(repo.path().join("docs/guide.md"), doc_body).unwrap();
    let mut cfg = RepoConfig::default();
    cfg.gate.base_ref = "main".to_string();
    cfg.commands.lint = lint.to_string();
    cfg.commands.test_scoped = test_scoped.to_string();
    let noop = |_: &aaf_engine::throttle::Progress<'_>| {};
    let run = run_gate_observed_reporting(
        data.path(),
        repo.path(),
        &cfg,
        GateMode::Scoped,
        &FixedClock(100),
        // The CPU-load wait is off: the host running the tests may be busy.
        &MachineConfig {
            max_load_percent: 0,
            ..MachineConfig::default()
        },
        &noop,
    )
    .unwrap();
    let names = run
        .receipt
        .commands
        .iter()
        .map(|c| c.name.clone())
        .collect();
    let fast = run
        .receipt
        .reasons
        .iter()
        .any(|r| r.starts_with(DOCS_FAST_PATH_NOTE));
    (run.receipt.status, names, fast)
}

#[test]
fn test_scoped_that_greps_the_docs_dir_runs_and_blocks() {
    let (status, names, fast) = scoped("", "! grep -rq TODO docs", "# Guide\nTODO\n");
    assert_ne!(status, GateStatus::Clear, "ran: {names:?}");
    assert_eq!(names, vec!["test_scoped".to_string()]);
    assert!(!fast);
}

#[test]
fn lint_that_names_the_doc_extension_runs() {
    let (status, names, fast) = scoped(": '**/*.md'", "true", "# Guide\nmore\n");
    assert_eq!(status, GateStatus::Clear);
    assert_eq!(names, vec!["lint".to_string(), "test_scoped".to_string()]);
    assert!(!fast);
}

#[test]
fn lint_that_names_the_doc_file_case_insensitively_runs() {
    let (_, names, fast) = scoped("test -s Docs/GUIDE.MD || true", "true", "# Guide\nx\n");
    assert_eq!(names, vec!["lint".to_string(), "test_scoped".to_string()]);
    assert!(!fast);
}

#[test]
fn commands_that_name_nothing_of_the_doc_stay_fast() {
    // `mydocs` and `docsite` are not whole-word hits.
    let (status, names, fast) = scoped("echo mydocs docsite", "exit 1", "# Guide\nok\n");
    assert_eq!(status, GateStatus::Clear);
    assert!(names.is_empty(), "{names:?}");
    assert!(fast);
}
