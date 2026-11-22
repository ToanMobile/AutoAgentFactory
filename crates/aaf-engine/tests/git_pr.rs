#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "git_support.rs"]
mod support;

use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use aaf_domain::gate::{
    CommandResult, GateMode, GateStatus, HeadLockReport, LockStatus, Receipt, Severity,
    StaticFinding, StoreComplianceReport,
};
use aaf_engine::git::pr::{self, PrOutcome, PrRequest};
use aaf_engine::git::{GitError, worktree};
use aaf_engine::process::{CommandOutput, CommandRunner, SystemRunner};
use support::{Fixture, commit_file, git};

fn finding(severity: Severity) -> StaticFinding {
    StaticFinding {
        rule: "SEC-001".to_string(),
        severity,
        file: "src/a.rs".to_string(),
        line: Some(3),
        message: "m".to_string(),
        fix: "f".to_string(),
    }
}

fn command(name: &str, line: &str, exit_code: Option<i32>, timed_out: bool) -> CommandResult {
    CommandResult {
        name: name.to_string(),
        command: line.to_string(),
        exit_code,
        timed_out,
        duration_ms: 12,
        log_tail: String::new(),
    }
}

fn receipt() -> Receipt {
    Receipt {
        schema_version: 1,
        repo_root: "/repo".to_string(),
        tree_fp: "git-tree:abc123".to_string(),
        head_sha: Some("deadbeef".to_string()),
        base_sha: None,
        mode: GateMode::Scoped,
        status: GateStatus::Blocked,
        reasons: vec!["test_scoped failed".to_string()],
        config_hash: "cfg".to_string(),
        commands: vec![
            command("lint", "cargo clippy", Some(0), false),
            command("test_scoped", "cargo test `x`", Some(101), false),
            command("build", "cargo build", None, true),
        ],
        static_findings: vec![
            finding(Severity::Critical),
            finding(Severity::Warn),
            finding(Severity::Warn),
        ],
        suppressions: 4,
        head_lock: HeadLockReport {
            status: LockStatus::Approved,
            lock_fp: "lockfp".to_string(),
            locked_changes: Vec::new(),
            approval: None,
        },
        store_compliance: StoreComplianceReport::not_applicable(),
        created_at: 0,
    }
}

type Call = (String, Vec<String>, PathBuf);

/// Records every call; answers `gh --version` and then `create` as told.
struct FakeRunner {
    calls: RefCell<Vec<Call>>,
    gh_installed: bool,
    create: CommandOutput,
}

impl FakeRunner {
    fn new(gh_installed: bool, code: i32, stdout: &str, stderr: &str) -> FakeRunner {
        FakeRunner {
            calls: RefCell::new(Vec::new()),
            gh_installed,
            create: CommandOutput {
                exit_code: Some(code),
                stdout: stdout.to_string(),
                stderr: stderr.to_string(),
            },
        }
    }
}

impl CommandRunner for FakeRunner {
    fn run(&self, program: &str, args: &[&str], cwd: &Path) -> io::Result<CommandOutput> {
        self.calls.borrow_mut().push((
            program.to_string(),
            args.iter().map(|a| (*a).to_string()).collect(),
            cwd.to_path_buf(),
        ));
        if !self.gh_installed {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        if args == ["--version"] {
            return Ok(CommandOutput {
                exit_code: Some(0),
                stdout: "gh version 2.0.0\n".to_string(),
                stderr: String::new(),
            });
        }
        Ok(self.create.clone())
    }
}

fn request(fx: &Fixture, title: &str) -> PrRequest {
    let wt = worktree::create(&fx.repo, &fx.worktrees, "t1", "main").unwrap();
    commit_file(&wt.path, "a.txt", "a\n", "task work");
    PrRequest {
        repo_root: fx.repo.clone(),
        worktree: wt,
        base: "main".to_string(),
        remote: "origin".to_string(),
        title: title.to_string(),
        receipt: receipt(),
    }
}

fn remote_has_branch(fx: &Fixture) -> bool {
    let remote = fx.remote.as_ref().unwrap();
    !git(remote, &["branch", "--list", "aaf/t1"]).is_empty()
}

#[test]
fn body_summarises_the_receipt() {
    let body = pr::pr_body(&receipt());
    for expected in [
        "Status: **BLOCKED** (mode: scoped)",
        "Tree: `git-tree:abc123`",
        "Head: `deadbeef`",
        "Head lock: APPROVED (0 locked change(s))",
        "Static findings: 1 critical, 2 warn, 0 info (4 suppressed)",
        "Store compliance: NOT_APPLICABLE (0 critical, 0 warnings)",
        "- lint: exit 0 (`cargo clippy`)",
        "- test_scoped: exit 101 (`cargo test 'x'`)",
        "- build: timed out (`cargo build`)",
        "- test_scoped failed",
    ] {
        assert!(body.contains(expected), "missing {expected:?} in:\n{body}");
    }
}

#[test]
fn opens_a_pr_after_pushing_the_branch() {
    let fx = Fixture::with_remote();
    let title = "fix: $(touch pwned); \"quoted\" `tick` --body x";
    let req = request(&fx, title);
    let runner = FakeRunner::new(
        true,
        0,
        "Creating pull request\nhttps://example.test/pr/7\n",
        "",
    );

    let outcome = pr::open_pr(&req, &runner).unwrap();
    assert_eq!(
        outcome,
        PrOutcome::Opened {
            url: "https://example.test/pr/7".to_string()
        }
    );
    let remote = fx.remote.as_ref().unwrap();
    assert_eq!(
        fx.head(remote, "aaf/t1"),
        fx.head(&req.worktree.path, "HEAD")
    );

    let calls = runner.calls.borrow();
    assert_eq!(calls.len(), 2);
    let (program, args, cwd) = &calls[1];
    assert_eq!(program, "gh");
    assert_eq!(cwd, &req.worktree.path);
    let expected = [
        "pr",
        "create",
        "--title",
        title,
        "--body",
        &pr::pr_body(&req.receipt),
        "--base",
        "main",
        "--head",
        "aaf/t1",
    ];
    assert_eq!(args, &expected);
    assert!(!fx.repo.join("pwned").exists());
}

#[test]
fn skips_without_a_remote_and_never_calls_gh() {
    let fx = Fixture::new();
    let req = request(&fx, "title");
    let runner = FakeRunner::new(true, 0, "https://example.test/pr/1\n", "");

    let outcome = pr::open_pr(&req, &runner).unwrap();
    assert!(matches!(outcome, PrOutcome::Skipped { ref reason } if reason.contains("remote")));
    assert!(runner.calls.borrow().is_empty());
}

#[test]
fn skips_without_gh_and_pushes_nothing() {
    let fx = Fixture::with_remote();
    let req = request(&fx, "title");
    let runner = FakeRunner::new(false, 0, "", "");

    let outcome = pr::open_pr(&req, &runner).unwrap();
    assert!(matches!(outcome, PrOutcome::Skipped { ref reason } if reason.contains("gh")));
    assert_eq!(runner.calls.borrow().len(), 1);
    assert!(!remote_has_branch(&fx));
}

#[test]
fn a_failing_gh_is_an_error_carrying_its_stderr() {
    let fx = Fixture::with_remote();
    let req = request(&fx, "title");
    let runner = FakeRunner::new(true, 1, "", "GraphQL: not authorised\n");

    match pr::open_pr(&req, &runner).unwrap_err() {
        GitError::Command { code, stderr, .. } => {
            assert_eq!(code, Some(1));
            assert_eq!(stderr, "GraphQL: not authorised");
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn gh_success_without_a_url_is_an_error() {
    let fx = Fixture::with_remote();
    let req = request(&fx, "title");
    let runner = FakeRunner::new(true, 0, "done\n", "");
    assert!(pr::open_pr(&req, &runner).is_err());
}

#[test]
fn system_runner_passes_arguments_verbatim_and_reports_a_missing_program() {
    let dir = tempfile::tempdir().unwrap();
    let runner = SystemRunner {
        timeout: Duration::from_secs(10),
    };
    let out = runner
        .run("/bin/echo", &["$(id); a b", "--x"], dir.path())
        .unwrap();
    assert_eq!(out.stdout, "$(id); a b --x\n");

    let err = runner
        .run("aaf-no-such-program-xyz", &[], dir.path())
        .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
}
