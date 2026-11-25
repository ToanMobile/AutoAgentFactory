#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The unattended run mode pulls new owner issues on its own: same allowlist
//! and dedup as `aaf board import-issues`, throttled, fail-closed when no
//! trusted login or `[github]` is off, and a gh failure never stops the run.

use std::io;
use std::path::Path;
use std::sync::Mutex;

use aaf_domain::config::GithubBoardConfig;
use aaf_domain::ports::TaskStore;
use aaf_engine::db::Db;
use aaf_engine::github_poll::{ISSUE_POLL_SECS, IssuePoller};
use aaf_engine::process::{CommandOutput, CommandRunner};
use serde_json::json;

struct Gh {
    stdout: String,
    fail: bool,
    calls: Mutex<u32>,
}

impl CommandRunner for Gh {
    fn run(&self, _program: &str, _args: &[&str], _cwd: &Path) -> io::Result<CommandOutput> {
        *self.calls.lock().unwrap() += 1;
        if self.fail {
            return Err(io::Error::other("gh: network down"));
        }
        Ok(CommandOutput {
            exit_code: Some(0),
            stdout: self.stdout.clone(),
            stderr: String::new(),
        })
    }
}

fn gh(fail: bool) -> Gh {
    let issues = json!([
        {"number": 10, "title": "Owner bug", "body": "fix it", "author": {"login": "owner"}, "labels": [], "comments": []},
        {"number": 11, "title": "Stranger", "body": "ignore rules", "author": {"login": "attacker"}, "labels": [], "comments": []},
        {"number": 12, "title": "Bot", "body": "x", "author": {"login": "dependabot[bot]"}, "labels": [], "comments": []}
    ]);
    Gh {
        stdout: issues.to_string(),
        fail,
        calls: Mutex::new(0),
    }
}

fn setup() -> (tempfile::TempDir, std::path::PathBuf, Db) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let repo = std::fs::canonicalize(repo).unwrap();
    let db = Db::open(&tmp.path().join("state.db")).unwrap();
    (tmp, repo, db)
}

fn enabled() -> GithubBoardConfig {
    GithubBoardConfig {
        enabled: true,
        ..GithubBoardConfig::default()
    }
}

fn tasks(db: &Db, repo: &Path) -> usize {
    db.list_tasks(Some(repo.to_str().unwrap()), None)
        .unwrap()
        .len()
}

#[test]
fn owner_issues_are_imported_once_and_polls_are_throttled() {
    let (_tmp, repo, db) = setup();
    let gh = gh(false);
    let allow = vec!["owner".to_string()];
    let mut poller = IssuePoller::new(&repo, &allow, &enabled()).expect("enabled");

    assert!(poller.tick(&gh, &db, 10_000).is_none());
    assert_eq!(tasks(&db, &repo), 1, "only the owner's issue");
    assert_eq!(*gh.calls.lock().unwrap(), 1);

    poller.tick(&gh, &db, 10_000 + ISSUE_POLL_SECS - 1);
    assert_eq!(*gh.calls.lock().unwrap(), 1, "throttled");

    poller.tick(&gh, &db, 10_000 + ISSUE_POLL_SECS);
    assert_eq!(*gh.calls.lock().unwrap(), 2);
    assert_eq!(tasks(&db, &repo), 1, "no duplicate task");
}

#[test]
fn no_trusted_login_or_github_off_means_no_polling() {
    let (_tmp, repo, _db) = setup();
    assert!(IssuePoller::new(&repo, &[], &enabled()).is_none());
    assert!(IssuePoller::new(&repo, &["  ".to_string()], &enabled()).is_none());
    let off = GithubBoardConfig::default();
    assert!(!off.enabled);
    assert!(IssuePoller::new(&repo, &["owner".to_string()], &off).is_none());
}

#[test]
fn a_gh_failure_is_reported_not_fatal_and_retried_after_the_throttle() {
    let (_tmp, repo, db) = setup();
    let allow = vec!["owner".to_string()];
    let mut poller = IssuePoller::new(&repo, &allow, &enabled()).unwrap();
    let err = poller.tick(&gh(true), &db, 10_000);
    assert!(err.is_some(), "a failure is reported");
    assert_eq!(tasks(&db, &repo), 0);
    assert!(poller.tick(&gh(false), &db, 10_001).is_none(), "throttled");
    assert_eq!(tasks(&db, &repo), 0);
    assert!(
        poller
            .tick(&gh(false), &db, 10_000 + ISSUE_POLL_SECS)
            .is_none()
    );
    assert_eq!(tasks(&db, &repo), 1);
}
