#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Pillar 4: GitHub Projects v2 status column sync, collision guards, auto-add, and ambiguity checks.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use aaf_domain::config::GithubBoardConfig;
use aaf_domain::ports::{PortError, TaskStore};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::github_board::sync_board;
use aaf_engine::process::{CommandOutput, CommandRunner};
use serde_json::{Value, json};
use tempfile::TempDir;

struct ScriptedGh {
    responses: Mutex<VecDeque<CommandOutput>>,
    calls: Mutex<Vec<Vec<String>>>,
}

impl ScriptedGh {
    fn new(outputs: Vec<Value>) -> Self {
        Self::from_raw(outputs.into_iter().map(|v| v.to_string()).collect())
    }

    fn from_raw(outputs: Vec<String>) -> Self {
        let responses = outputs
            .into_iter()
            .map(|stdout| CommandOutput {
                exit_code: Some(0),
                stdout,
                stderr: String::new(),
            })
            .collect();
        Self {
            responses: Mutex::new(responses),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
}

impl CommandRunner for ScriptedGh {
    fn run(&self, program: &str, args: &[&str], _cwd: &Path) -> io::Result<CommandOutput> {
        let mut argv = vec![program.to_string()];
        argv.extend(args.iter().map(|s| (*s).to_string()));
        self.calls.lock().unwrap().push(argv);
        Ok(self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(CommandOutput {
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            }))
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

fn insert_issue_task(db: &Db, repo: &Path, id: &str, issue: u64, state: TaskState, now: i64) {
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
        now,
    );
    task.state = state;
    task.fingerprint = Some(format!("gh-issue:#{issue}"));
    db.insert_task(&task).unwrap();
}

fn project_v2_cfg(repo: &str) -> GithubBoardConfig {
    GithubBoardConfig {
        enabled: true,
        repo: repo.to_string(),
        project_owner: "ToanMobile".to_string(),
        project_number: Some(4),
        ..GithubBoardConfig::default()
    }
}

fn status_fields_json() -> Value {
    json!({
        "fields": [{
            "id": "PVTSSF_status",
            "name": "Status",
            "options": [
                {"id": "opt_ready", "name": "Ready"},
                {"id": "opt_qa", "name": "QA"}
            ]
        }]
    })
}

#[test]
fn sync_board_updates_github_projects_v2_status_columns_and_rejects_missing_items() {
    let (_tmp, repo, db) = setup_repo_db();
    insert_issue_task(&db, &repo, "t-pv2", 88, TaskState::Gating, 100);
    let cfg = project_v2_cfg("ToanMobile/demo");

    let dry_gh = ScriptedGh::new(vec![]);
    let dry = sync_board(&dry_gh, &db, &repo, &cfg, true).unwrap();
    assert_eq!(dry.mode, "project_v2");
    assert!(dry_gh.calls().is_empty());
    assert_eq!(dry.items[0].column, "QA");

    let gh = ScriptedGh::new(vec![
        json!({"id": "PVT_4"}),
        status_fields_json(),
        json!({
            "items": [{"id": "PVTI_88", "content": {"type": "Issue", "number": 88}}],
            "totalCount": 1
        }),
        json!({}),
    ]);
    let applied = sync_board(&gh, &db, &repo, &cfg, false).unwrap();
    assert_eq!(applied.mode, "project_v2");
    assert!(!applied.truncated_project_items);
    assert!(applied.items[0].synced && !applied.items[0].unchanged);
    let calls = gh.calls();
    assert_eq!(calls.len(), 4, "{calls:?}");
    assert_eq!(
        calls[3],
        [
            "gh",
            "project",
            "item-edit",
            "--project-id",
            "PVT_4",
            "--id",
            "PVTI_88",
            "--field-id",
            "PVTSSF_status",
            "--single-select-option-id",
            "opt_qa",
        ]
    );

    let bad_gh = ScriptedGh::new(vec![
        json!({"id": "PVT_4"}),
        json!({"fields": [{"id": "PVTSSF_status", "name": "Status", "options": [{"id": "opt_ready", "name": "Ready"}]}]}),
        json!({"items": [{"id": "PVTI_88", "content": {"number": 88}}]}),
    ]);
    assert!(matches!(
        sync_board(&bad_gh, &db, &repo, &cfg, false),
        Err(PortError::Invalid(_))
    ));
    assert_eq!(bad_gh.calls().len(), 3);

    let broken_json_gh = ScriptedGh::from_raw(vec!["not-json".to_string()]);
    assert!(matches!(
        sync_board(&broken_json_gh, &db, &repo, &cfg, false),
        Err(PortError::Invalid(_))
    ));
}

#[test]
fn sync_board_projects_v2_filters_collisions_skips_unchanged_and_flags_truncation() {
    let (_tmp, repo, db) = setup_repo_db();
    insert_issue_task(&db, &repo, "t-col", 88, TaskState::Gating, 100);
    let cfg = project_v2_cfg("ToanMobile/demo");

    // Multi-repo, PR, and missing-repository collisions on a multi-repo board are ignored.
    let gh = ScriptedGh::new(vec![
        json!({"id": "PVT_4"}),
        status_fields_json(),
        json!({
            "items": [
                {"id": "PVTI_other_repo", "status": "Ready", "content": {"type": "Issue", "number": 88, "repository": "other-org/other-repo"}},
                {"id": "PVTI_pr", "status": "Ready", "content": {"type": "PullRequest", "number": 88, "repository": "ToanMobile/demo"}},
                {"id": "PVTI_real", "status": "Ready", "content": {"type": "Issue", "number": 88, "repository": "toanmobile/demo"}},
                {"id": "PVTI_missing_repo", "status": "Ready", "content": {"type": "Issue", "number": 88}}
            ],
            "totalCount": 4
        }),
        json!({}),
    ]);
    let applied = sync_board(&gh, &db, &repo, &cfg, false).unwrap();
    assert!(applied.items[0].synced && !applied.truncated_project_items);
    assert!(gh.calls()[3].windows(2).any(|w| w == ["--id", "PVTI_real"]));

    let unchanged_gh = ScriptedGh::new(vec![
        json!({"id": "PVT_4"}),
        status_fields_json(),
        json!({
            "items": [
                {"id": "PVTI_real", "status": "QA", "content": {"type": "Issue", "number": 88, "repository": "ToanMobile/demo"}}
            ],
            "totalCount": 250
        }),
    ]);
    let skipped = sync_board(&unchanged_gh, &db, &repo, &cfg, false).unwrap();
    assert_eq!(unchanged_gh.calls().len(), 3);
    assert!(!skipped.items[0].synced && skipped.items[0].unchanged);
    assert!(skipped.items[0].error.is_none() && skipped.truncated_project_items);
}

#[test]
fn sync_board_projects_v2_auto_adds_missing_items_and_reports_empty_repo_errors() {
    let (_tmp, repo, db) = setup_repo_db();
    insert_issue_task(&db, &repo, "t-add", 88, TaskState::Gating, 100);
    let cfg = project_v2_cfg("ToanMobile/demo");

    let add_gh = ScriptedGh::new(vec![
        json!({"id": "PVT_4"}),
        status_fields_json(),
        json!({"items": [], "totalCount": 0}),
        json!({"id": "PVTI_added"}),
        json!({}),
    ]);
    let added = sync_board(&add_gh, &db, &repo, &cfg, false).unwrap();
    assert!(added.items[0].synced);
    let calls = add_gh.calls();
    assert_eq!(calls.len(), 5, "{calls:?}");
    assert_eq!(
        calls[3],
        [
            "gh",
            "project",
            "item-add",
            "4",
            "--owner",
            "ToanMobile",
            "--url",
            "https://github.com/ToanMobile/demo/issues/88",
            "--format",
            "json",
        ]
    );
    assert!(calls[4].windows(2).any(|w| w == ["--id", "PVTI_added"]));

    let no_repo_cfg = project_v2_cfg("");
    let empty_repo_gh = ScriptedGh::new(vec![
        json!({"id": "PVT_4"}),
        status_fields_json(),
        json!({"items": [], "totalCount": 0}),
    ]);
    assert!(matches!(
        sync_board(&empty_repo_gh, &db, &repo, &no_repo_cfg, false),
        Err(PortError::Invalid(_))
    ));
    assert_eq!(empty_repo_gh.calls().len(), 3);

    insert_issue_task(&db, &repo, "t-miss", 89, TaskState::Ready, 101);
    let partial_gh = ScriptedGh::new(vec![
        json!({"id": "PVT_4"}),
        status_fields_json(),
        json!({
            "items": [{"id": "PVTI_88", "status": "Ready", "content": {"type": "Issue", "number": 88}}],
            "totalCount": 1
        }),
        json!({}),
    ]);
    let partial = sync_board(&partial_gh, &db, &repo, &no_repo_cfg, false).unwrap();
    let by_num = |n| {
        partial
            .items
            .iter()
            .find(|i| i.issue_number == Some(n))
            .unwrap()
    };
    assert!(by_num(88).synced && by_num(88).error.is_none());
    assert!(!by_num(89).synced && by_num(89).error.is_some());
}

#[test]
fn sync_board_projects_v2_rejects_ambiguous_duplicate_numbers_when_repo_empty() {
    let (_tmp, repo, db) = setup_repo_db();
    insert_issue_task(&db, &repo, "t-ambig", 88, TaskState::Gating, 100);
    insert_issue_task(&db, &repo, "t-unique", 89, TaskState::Ready, 101);
    let no_repo_cfg = project_v2_cfg("");

    let gh = ScriptedGh::new(vec![
        json!({"id": "PVT_4"}),
        status_fields_json(),
        json!({
            "items": [
                {"id": "PVTI_a88", "status": "Ready", "content": {"type": "Issue", "number": 88, "repository": "org/repo-a"}},
                {"id": "PVTI_b88", "status": "Ready", "content": {"type": "Issue", "number": 88, "repository": "org/repo-b"}},
                {"id": "PVTI_u89", "status": "QA", "content": {"type": "Issue", "number": 89, "repository": "org/repo-a"}}
            ],
            "totalCount": 3
        }),
        json!({}),
    ]);

    let out = sync_board(&gh, &db, &repo, &no_repo_cfg, false).unwrap();
    let item88 = out
        .items
        .iter()
        .find(|i| i.issue_number == Some(88))
        .unwrap();
    let item89 = out
        .items
        .iter()
        .find(|i| i.issue_number == Some(89))
        .unwrap();
    assert!(!item88.synced);
    assert!(
        item88
            .error
            .as_deref()
            .is_some_and(|msg| msg.contains("[github].repo")),
        "{item88:?}"
    );
    assert!(item89.synced && item89.error.is_none());
    // Only #89 (PVTI_u89) was edited; #88 was never overwritten.
    assert_eq!(gh.calls().len(), 4);
    assert!(gh.calls()[3].windows(2).any(|w| w == ["--id", "PVTI_u89"]));
}
