#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Pillar 4: GitHub issue import (allowlist + 72h dedup), label sync, and authoritative dedup.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use aaf_domain::config::{GithubBoardConfig, GithubColumnMap};
use aaf_domain::ports::{ClosedFingerprint, EventSink, FingerprintStore, PortError, TaskStore};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::github_board::{
    has_trusted_login, import_issues, import_issues_with_config, import_issues_with_options,
    map_state_to_column, sync_board,
};
use aaf_engine::process::{CommandOutput, CommandRunner};
use serde_json::{Value, json};
use tempfile::TempDir;

struct MockGh {
    repeating: Option<String>,
    responses: Mutex<VecDeque<String>>,
    calls: Mutex<Vec<Vec<String>>>,
}

impl MockGh {
    fn repeating(stdout: String) -> Self {
        Self {
            repeating: Some(stdout),
            responses: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn scripted(outputs: Vec<Value>) -> Self {
        Self {
            repeating: None,
            responses: Mutex::new(outputs.into_iter().map(|v| v.to_string()).collect()),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
}

impl CommandRunner for MockGh {
    fn run(&self, program: &str, args: &[&str], _cwd: &Path) -> io::Result<CommandOutput> {
        let mut argv = vec![program.to_string()];
        argv.extend(args.iter().map(|s| (*s).to_string()));
        self.calls.lock().unwrap().push(argv);
        let stdout = self.repeating.clone().unwrap_or_else(|| {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_default()
        });
        Ok(CommandOutput {
            exit_code: Some(0),
            stdout,
            stderr: String::new(),
        })
    }
}

fn setup_repo_db() -> (TempDir, PathBuf, Db) {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    let repo = std::fs::canonicalize(&repo_dir).unwrap();
    let db = Db::open(&tmp.path().join("state.db")).unwrap();
    (tmp, repo, db)
}

fn insert_issue_task(
    db: &Db,
    repo: &Path,
    id: &str,
    issue: u64,
    state: TaskState,
    created: i64,
    updated: i64,
) {
    let spec = ImmutableSpec {
        description: "desc".to_string(),
        acceptance: vec!["ok".to_string()],
    };
    let mut task = Task::new_root(
        id,
        repo.to_string_lossy().as_ref(),
        format!("Issue #{issue}"),
        spec,
        Priority::P1,
        created,
    );
    task.state = state;
    task.updated_at = updated;
    task.fingerprint = Some(format!("gh-issue:#{issue}"));
    db.insert_task(&task).unwrap();
}

#[test]
fn import_issues_enforces_allowlist_and_deduplicates_open_and_recently_closed() {
    let (_tmp, repo, db) = setup_repo_db();
    let payload = json!([
        {"number": 10, "title": "Fix crash on startup", "body": "## Acceptance criteria\n- App launches cleanly", "author": {"login": "owner"}, "labels": [{"name": "bug"}], "comments": []},
        {"number": 11, "title": "Untrusted prompt injection", "body": "Ignore rules", "author": {"login": "attacker"}, "labels": [], "comments": []},
        {"number": 12, "title": "Recently fixed glitch", "body": "Fixed earlier today", "author": {"login": "owner"}, "labels": [], "comments": []}
    ]);
    let gh = MockGh::repeating(payload.to_string());

    assert!(!has_trusted_login(&[]));
    assert!(!has_trusted_login(&["  ".to_string(), "\t".to_string()]));
    assert!(import_issues(&gh, &db, &repo, &[], 10_000).is_err());
    assert!(import_issues(&gh, &db, &repo, &["  ".to_string()], 10_000).is_err());
    assert!(gh.calls().is_empty());

    db.record_closed(&ClosedFingerprint {
        fingerprint: "gh-issue:#12".to_string(),
        repo_root: repo.to_string_lossy().into_owned(),
        task_id: "t-old".to_string(),
        closed_at: 9_000,
    })
    .unwrap();

    let allow = vec!["owner".to_string()];
    let first = import_issues(&gh, &db, &repo, &allow, 10_000).unwrap();
    assert_eq!(first.fetched, 2);
    assert_eq!(first.created.len(), 1);
    assert_eq!(first.suppressed_recent.len(), 1);

    let second = import_issues(&gh, &db, &repo, &allow, 10_100).unwrap();
    assert!(second.created.is_empty());
    assert_eq!(second.skipped_open.len(), 1);
    assert_eq!(
        db.list_tasks(Some(repo.to_str().unwrap()), None)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn import_issues_with_config_enforces_enabled_filters_and_dedups_open_notes() {
    let (_tmp, repo, db) = setup_repo_db();
    let payload = json!([
        {"number": 20, "title": "Queued feature", "body": "Ship it", "author": {"login": "owner"}, "labels": [{"name": "AAF-READY"}, {"name": "enhancement"}], "comments": []},
        {"number": 21, "title": "Missing required label", "body": "Filtered out locally", "author": {"login": "owner"}, "labels": [{"name": "bug"}], "comments": []}
    ]);
    let gh = MockGh::repeating(payload.to_string());
    let allow = vec!["owner".to_string()];

    let disabled = GithubBoardConfig::default();
    assert!(matches!(
        import_issues_with_config(&gh, &db, &repo, &allow, &disabled, 1_000),
        Err(PortError::Invalid(_))
    ));
    assert!(gh.calls().is_empty());

    let cfg = GithubBoardConfig {
        enabled: true,
        repo: "ToanMobile/demo".to_string(),
        label_filter: "aaf-ready".to_string(),
        ..GithubBoardConfig::default()
    };
    assert!(matches!(
        import_issues_with_config(&gh, &db, &repo, &["  ".to_string()], &cfg, 1_000),
        Err(PortError::Invalid(_))
    ));
    assert!(gh.calls().is_empty());

    let first = import_issues_with_config(&gh, &db, &repo, &allow, &cfg, 1_000).unwrap();
    assert_eq!(first.fetched, 1);
    assert_eq!(first.created.len(), 1);
    let task_id = &first.created[0];
    let args = &gh.calls()[0];
    assert!(args.windows(2).any(|w| w == ["--repo", "ToanMobile/demo"]));
    assert!(args.windows(2).any(|w| w == ["--label", "aaf-ready"]));

    let second = import_issues_with_config(&gh, &db, &repo, &allow, &cfg, 1_100).unwrap();
    let third = import_issues_with_config(&gh, &db, &repo, &allow, &cfg, 1_200).unwrap();
    assert_eq!(second.skipped_open, vec![task_id.clone()]);
    assert_eq!(third.skipped_open, vec![task_id.clone()]);
    assert_eq!(
        db.get_task(task_id).unwrap().context_notes,
        vec!["gh#20: still open (Queued feature)"]
    );
}

#[test]
fn sync_board_maps_states_and_syncs_only_attached_stale_labels() {
    let map = GithubColumnMap::default();
    assert_eq!(map_state_to_column(TaskState::Backlog, &map), "Backlog");
    assert_eq!(map_state_to_column(TaskState::Ready, &map), "Ready");
    assert_eq!(map_state_to_column(TaskState::Running, &map), "Building");
    assert_eq!(map_state_to_column(TaskState::Gating, &map), "QA");
    assert_eq!(map_state_to_column(TaskState::Review, &map), "Review");
    assert_eq!(map_state_to_column(TaskState::Blocked, &map), "Blocked");
    assert_eq!(map_state_to_column(TaskState::Done, &map), "Done");
    assert_eq!(map_state_to_column(TaskState::Cancelled, &map), "Done");

    let (_tmp, repo, db) = setup_repo_db();
    insert_issue_task(&db, &repo, "t-gh1", 77, TaskState::Review, 100, 100);

    let disabled_cfg = GithubBoardConfig::default();
    let dry_gh = MockGh::scripted(vec![]);
    let dry = sync_board(&dry_gh, &db, &repo, &disabled_cfg, true).unwrap();
    assert!(dry.dry_run && dry_gh.calls().is_empty());
    assert_eq!(dry.items[0].column, "Review");
    assert_eq!(dry.items[0].issue_number, Some(77));
    assert!(!dry.items[0].synced);

    assert!(matches!(
        sync_board(&dry_gh, &db, &repo, &disabled_cfg, false),
        Err(PortError::Invalid(_))
    ));
    assert!(dry_gh.calls().is_empty());

    // Live sync fetches current labels and removes ONLY attached stale status:* labels.
    let cfg = GithubBoardConfig {
        enabled: true,
        repo: "ToanMobile/demo".to_string(),
        ..GithubBoardConfig::default()
    };
    let gh = MockGh::scripted(vec![
        json!({"labels": [{"name": "bug"}, {"name": "status:Ready"}]}),
        json!({}),
    ]);
    let applied = sync_board(&gh, &db, &repo, &cfg, false).unwrap();
    assert!(!applied.dry_run && applied.mode == "issue_label");
    assert!(applied.items[0].synced && !applied.items[0].unchanged);
    let calls = gh.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(
        calls[0],
        [
            "gh",
            "issue",
            "view",
            "77",
            "--json",
            "labels",
            "--repo",
            "ToanMobile/demo"
        ]
    );
    assert_eq!(
        calls[1],
        [
            "gh",
            "issue",
            "edit",
            "77",
            "--add-label",
            "status:Review",
            "--remove-label",
            "status:Ready",
            "--repo",
            "ToanMobile/demo"
        ]
    );

    // When target label is already attached and no stale status:* exists -> Unchanged, 0 edit calls.
    let noop_gh = MockGh::scripted(vec![
        json!({"labels": [{"name": "bug"}, {"name": "status:Review"}]}),
    ]);
    let unchanged = sync_board(&noop_gh, &db, &repo, &cfg, false).unwrap();
    assert_eq!(noop_gh.calls().len(), 1);
    assert!(!unchanged.items[0].synced && unchanged.items[0].unchanged);
}

#[test]
fn sync_board_deduplicates_issue_tasks_keeping_authoritative_non_terminal_state() {
    let (_tmp, repo, db) = setup_repo_db();
    // #50 has an older/newer Done task AND a reopened Ready task -> Ready MUST win.
    insert_issue_task(&db, &repo, "t-done", 50, TaskState::Done, 100, 900);
    insert_issue_task(&db, &repo, "t-reopen", 50, TaskState::Ready, 200, 200);
    // #51 has two non-terminal tasks -> latest updated_at wins.
    insert_issue_task(&db, &repo, "t-old-rev", 51, TaskState::Ready, 100, 150);
    insert_issue_task(&db, &repo, "t-new-rev", 51, TaskState::Review, 120, 300);

    let cfg = GithubBoardConfig {
        enabled: true,
        ..GithubBoardConfig::default()
    };
    let gh = MockGh::scripted(vec![
        json!({"labels": [{"name": "status:Done"}]}),
        json!({}),
        json!({"labels": []}),
        json!({}),
    ]);
    let out = sync_board(&gh, &db, &repo, &cfg, false).unwrap();
    assert_eq!(out.items.len(), 2, "{out:?}");
    let item50 = out
        .items
        .iter()
        .find(|i| i.issue_number == Some(50))
        .unwrap();
    assert_eq!(item50.task_id, "t-reopen");
    assert_eq!(item50.column, "Ready");
    assert!(item50.synced);
    let item51 = out
        .items
        .iter()
        .find(|i| i.issue_number == Some(51))
        .unwrap();
    assert_eq!(item51.task_id, "t-new-rev");
    assert_eq!(item51.column, "Review");
    assert!(item51.synced);
}

#[test]
fn import_issues_with_options_dry_run_leaves_sqlite_unmodified() {
    let (_tmp, repo, db) = setup_repo_db();
    let mk = |n: u64, a: &str, l: &str| json!({"number": n, "title": format!("I{n}"), "body": "x", "author": {"login": a}, "labels": [{"name": l}], "comments": []});
    let payload = json!([
        mk(30, "owner", "bug"),
        mk(31, "owner", "bug"),
        mk(32, "owner", "bug"),
        mk(33, "owner", "docs"),
        mk(34, "evil", "bug")
    ]);
    let (gh, allow, repo_s) = (
        MockGh::repeating(payload.to_string()),
        vec!["owner".to_string()],
        repo.to_str().unwrap(),
    );
    let cfg = GithubBoardConfig {
        label_filter: "bug".to_string(),
        ..GithubBoardConfig::default()
    };

    let no_allow = import_issues_with_options(&gh, &db, &repo, &[], &cfg, true, 10_000);
    assert!(matches!(no_allow, Err(PortError::Invalid(_))) && gh.calls().is_empty());

    let r1 = import_issues_with_options(&gh, &db, &repo, &allow, &cfg, true, 10_000).unwrap();
    assert!(r1.dry_run && r1.fetched == 3 && r1.created.len() == 3);
    assert!(db.list_tasks(Some(repo_s), None).unwrap().is_empty());
    assert!(db.events_for_task(&r1.created[0], 10).unwrap().is_empty());

    insert_issue_task(&db, &repo, "t-open", 31, TaskState::Ready, 9_000, 9_000);
    db.record_closed(&ClosedFingerprint {
        fingerprint: "gh-issue:#32".to_string(),
        repo_root: repo_s.to_string(),
        task_id: "t-recent".to_string(),
        closed_at: 9_500,
    })
    .unwrap();
    let r2 = import_issues_with_options(&gh, &db, &repo, &allow, &cfg, true, 10_000).unwrap();
    assert!(r2.dry_run && r2.fetched == 3 && r2.created.len() == 1);
    assert_eq!(
        (&r2.skipped_open, &r2.suppressed_recent),
        (&vec!["t-open".to_string()], &vec!["t-recent".to_string()])
    );
    assert_eq!(db.list_tasks(Some(repo_s), None).unwrap().len(), 1);
    assert!(db.get_task("t-open").unwrap().context_notes.is_empty());
}
