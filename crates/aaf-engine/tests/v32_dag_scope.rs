#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Phase 1 v3.2 tests: DB v3 -> v4 migration, read-only v3 compatibility,
//! `TaskStore::{remove_dep, all_deps}`, `schedule_order` helpers, and `split` scope wiring.

use std::path::Path;

use aaf_domain::blocked::BlockedReason;
use aaf_domain::config::UnblockerConfig;
use aaf_domain::ports::TaskStore;
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::{Db, DbError, SCHEMA_VERSION};
use aaf_engine::factory::schedule_order::{
    blocked_upstream_tasks, normalize_scope, scopes_overlap,
    sort_ready_by_priority_and_critical_path,
};
use aaf_engine::split::{SplitSubtask, apply_split};
use rusqlite::Connection;

/// Frozen v3 schema: v1 tables + v2 indexes/triggers + v3 `accounts.name`,
/// intentionally WITHOUT v4's `tasks.scope_paths` or `turn_snapshots`.
const V3_SCHEMA: &str = "
    CREATE TABLE tasks (
        id TEXT PRIMARY KEY, repo_root TEXT NOT NULL, parent_id TEXT REFERENCES tasks(id),
        root_id TEXT NOT NULL, depth INTEGER NOT NULL, title TEXT NOT NULL, spec TEXT NOT NULL,
        context_notes TEXT NOT NULL, state TEXT NOT NULL, priority TEXT NOT NULL,
        fingerprint TEXT, blocked_reason TEXT, unblock_cycles INTEGER NOT NULL DEFAULT 0,
        created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
    );
    CREATE TABLE root_budgets (
        root_id TEXT PRIMARY KEY, tokens_used INTEGER NOT NULL, token_limit INTEGER NOT NULL,
        wall_secs_used INTEGER NOT NULL, wall_secs_limit INTEGER NOT NULL,
        cycles_used INTEGER NOT NULL, cycles_limit INTEGER NOT NULL, frozen INTEGER NOT NULL
    );
    CREATE TABLE task_deps (
        task_id TEXT NOT NULL REFERENCES tasks(id), blocked_by TEXT NOT NULL REFERENCES tasks(id),
        PRIMARY KEY (task_id, blocked_by)
    );
    CREATE TABLE runs (
        id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id), account_id TEXT,
        executor TEXT NOT NULL, worktree_path TEXT NOT NULL, branch TEXT NOT NULL,
        status TEXT NOT NULL, exit_code INTEGER, tokens_in INTEGER NOT NULL,
        tokens_out INTEGER NOT NULL, started_at INTEGER NOT NULL, ended_at INTEGER
    );
    CREATE TABLE accounts (
        id TEXT PRIMARY KEY, provider TEXT NOT NULL, config_dir TEXT NOT NULL,
        cooldown_until INTEGER, name TEXT NOT NULL DEFAULT ''
    );
    CREATE TABLE events (
        id INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT, run_id TEXT, kind TEXT NOT NULL,
        payload_json TEXT NOT NULL, created_at INTEGER NOT NULL
    );
    CREATE TABLE closed_fingerprints (
        id INTEGER PRIMARY KEY, fingerprint TEXT NOT NULL, repo_root TEXT NOT NULL,
        task_id TEXT NOT NULL, closed_at INTEGER NOT NULL
    );
    CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
    CREATE INDEX idx_tasks_state_priority ON tasks(state, priority, created_at);
    CREATE INDEX idx_tasks_repo_state_priority ON tasks(repo_root, state, priority, created_at);
    CREATE INDEX idx_tasks_fingerprint ON tasks(fingerprint, repo_root);
    CREATE INDEX idx_tasks_parent ON tasks(parent_id);
    CREATE INDEX idx_runs_account_started ON runs(account_id, started_at);
    CREATE INDEX idx_runs_task ON runs(task_id, started_at);
    CREATE INDEX idx_events_task ON events(task_id, id);
    CREATE INDEX idx_closed_fingerprints ON closed_fingerprints(fingerprint, repo_root, closed_at);
    CREATE TRIGGER events_no_update BEFORE UPDATE ON events
        BEGIN SELECT RAISE(ABORT, 'events is append-only'); END;
    CREATE TRIGGER events_no_delete BEFORE DELETE ON events
        BEGIN SELECT RAISE(ABORT, 'events is append-only'); END;
    PRAGMA user_version = 3;
";

const V3_ROWS: &str = r#"
    INSERT INTO tasks VALUES ('t-root', '/repo', NULL, 't-root', 0, 'legacy root',
        '{"description":"v3 task","acceptance":["a1"]}', '["n1"]', 'ready', 'P1', NULL,
        NULL, 0, 100, 100);
    INSERT INTO tasks VALUES ('t-child', '/repo', 't-root', 't-root', 1, 'legacy child',
        '{"description":"child","acceptance":["c1"]}', '[]', 'ready', 'P1', NULL,
        NULL, 0, 101, 101);
    INSERT INTO task_deps VALUES ('t-root', 't-child');
"#;

fn user_version(path: &Path) -> i64 {
    let conn = Connection::open(path).unwrap();
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap()
}

fn table_exists(path: &Path, table: &str) -> bool {
    let conn = Connection::open(path).unwrap();
    conn.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1")
        .unwrap()
        .exists([table])
        .unwrap()
}

fn task_columns(path: &Path) -> Vec<String> {
    let conn = Connection::open(path).unwrap();
    let mut stmt = conn
        .prepare("SELECT name FROM pragma_table_info('tasks')")
        .unwrap();
    stmt.query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn make_task(id: &str, priority: Priority, created_at: i64, state: TaskState) -> Task {
    let spec = ImmutableSpec {
        description: format!("spec {id}"),
        acceptance: vec!["ok".to_string()],
    };
    let mut task = Task::new_root(
        id,
        "/repo",
        format!("title {id}"),
        spec,
        priority,
        created_at,
    );
    task.state = state;
    if state == TaskState::Blocked {
        task.blocked_reason = Some(BlockedReason::NeedsHuman {
            why: "blocked".to_string(),
        });
    }
    task
}

#[test]
fn read_only_opens_v3_db_without_migrating_and_rw_upgrades_to_v4() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aaf.db");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(V3_SCHEMA).unwrap();
        conn.execute_batch(V3_ROWS).unwrap();
    }
    assert_eq!(user_version(&path), 3);
    assert!(!table_exists(&path, "turn_snapshots"));
    assert!(!task_columns(&path).iter().any(|c| c == "scope_paths"));

    // Read-only open on unmigrated v3 DB must read tasks cleanly with empty scope_paths.
    {
        let ro = Db::open_read_only(&path).unwrap();
        assert_eq!(ro.schema_version(), 3);
        let root = ro.get_task("t-root").unwrap();
        assert_eq!(root.title, "legacy root");
        assert!(root.scope_paths.is_empty());
        let all = ro.list_tasks(Some("/repo"), None).unwrap();
        assert_eq!(all.len(), 2);
        let children = ro.children_of("t-root").unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].id, "t-child");
        assert_eq!(
            ro.all_deps().unwrap(),
            vec![("t-root".to_string(), "t-child".to_string())]
        );
    }
    // Must not have migrated or modified user_version on disk.
    assert_eq!(user_version(&path), 3);
    assert!(!table_exists(&path, "turn_snapshots"));

    // Read-write open upgrades v3 -> v4, keeping all rows and adding scope_paths + turn_snapshots.
    {
        let rw = Db::open(&path).unwrap();
        assert_eq!(rw.schema_version(), SCHEMA_VERSION);
        const { assert!(SCHEMA_VERSION >= 4) };
        let root = rw.get_task("t-root").unwrap();
        assert_eq!(root.title, "legacy root");
        assert!(root.scope_paths.is_empty());

        let mut scoped = make_task("t-scoped", Priority::P1, 200, TaskState::Ready);
        scoped.scope_paths = vec!["src/api".to_string(), "src/db".to_string()];
        rw.insert_task(&scoped).unwrap();
        let loaded = rw.get_task("t-scoped").unwrap();
        assert_eq!(loaded.scope_paths, vec!["src/api", "src/db"]);

        // Test remove_dep and all_deps.
        assert!(rw.remove_dep("t-root", "t-child").unwrap());
        assert!(!rw.remove_dep("t-root", "t-child").unwrap());
        assert!(rw.all_deps().unwrap().is_empty());
        rw.add_dep("t-scoped", "t-root").unwrap();
        assert_eq!(
            rw.all_deps().unwrap(),
            vec![("t-scoped".to_string(), "t-root".to_string())]
        );
    }

    assert_eq!(user_version(&path), SCHEMA_VERSION);
    assert!(table_exists(&path, "turn_snapshots"));
    assert!(task_columns(&path).iter().any(|c| c == "scope_paths"));

    // Read-only open on migrated v4 DB reads scope_paths accurately.
    let ro_v4 = Db::open_read_only(&path).unwrap();
    assert_eq!(ro_v4.schema_version(), SCHEMA_VERSION);
    assert_eq!(
        ro_v4.get_task("t-scoped").unwrap().scope_paths,
        vec!["src/api", "src/db"]
    );
}

#[test]
fn open_read_only_rejects_user_version_99() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("future99.db");
    {
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
    }
    let err = Db::open_read_only(&path).unwrap_err();
    assert!(
        matches!(
            err,
            DbError::UnsupportedSchema {
                found: 99,
                supported: SCHEMA_VERSION,
            }
        ),
        "unexpected error: {err:?}"
    );
}

#[test]
fn normalize_scope_validates_and_normalizes_paths() {
    assert_eq!(normalize_scope("src/api/").unwrap(), "src/api");
    assert_eq!(normalize_scope("./src/api/v1/").unwrap(), "src/api/v1");
    assert_eq!(
        normalize_scope(r"src\api\mod.rs").unwrap(),
        "src/api/mod.rs"
    );
    assert_eq!(normalize_scope("Src/A").unwrap(), "Src/A");

    for bad in [
        "",
        "   ",
        ".",
        "./",
        "/etc/passwd",
        r"\Windows\System32",
        r"C:\repo\src",
        "src/../secret",
        "..",
        "src/*.rs",
        "src/a?.rs",
        "src/[a].rs",
        "src/foo\0bar",
        "src/foo\nbar",
    ] {
        assert!(normalize_scope(bad).is_err(), "expected err for {bad:?}");
    }
}

#[test]
fn scopes_overlap_apfs_case_prefix_boundary_and_unscoped() {
    let s = |vals: &[&str]| vals.iter().map(|v| (*v).to_string()).collect::<Vec<_>>();

    // Unscoped never blocks scoped or unscoped.
    assert!(!scopes_overlap(&[], &s(&["src/a"])));
    assert!(!scopes_overlap(&s(&["src/a"]), &[]));
    assert!(!scopes_overlap(&[], &[]));

    // APFS ASCII case-folding + directory-segment prefix.
    assert!(scopes_overlap(&s(&["Src/A"]), &s(&["src/a/x"])));
    assert!(scopes_overlap(&s(&["src/a/x"]), &s(&["SRC/A"])));
    assert!(scopes_overlap(&s(&["Src/A"]), &s(&["src/a"])));

    // Segment boundary: src/a must NOT overlap src/ab.
    assert!(!scopes_overlap(&s(&["src/a"]), &s(&["src/ab"])));
    assert!(!scopes_overlap(&s(&["src/ab"]), &s(&["src/a"])));
    assert!(!scopes_overlap(&s(&["src/a"]), &s(&["src/b"])));
}

#[test]
fn sort_ready_priority_beats_critical_path_and_depth_breaks_ties() {
    // Chain: c1 blocked by b1, b1 blocked by a1 (a1 has downstream depth 2).
    // Independent p0_solo has P0 and depth 0 -> must still beat P1 a1!
    // Independent p1_older has P1, depth 0, earlier created_at -> loses to P1 a1 (depth 2).
    let deps = vec![
        ("c1".to_string(), "b1".to_string()),
        ("b1".to_string(), "a1".to_string()),
    ];
    let mut tasks = vec![
        make_task("p1_older", Priority::P1, 1, TaskState::Ready),
        make_task("a1", Priority::P1, 50, TaskState::Ready),
        make_task("p0_solo", Priority::P0, 100, TaskState::Ready),
        make_task("p1_newer", Priority::P1, 20, TaskState::Ready),
    ];

    sort_ready_by_priority_and_critical_path(&mut tasks, &deps);
    let ids: Vec<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, vec!["p0_solo", "a1", "p1_older", "p1_newer"]);

    // Cycle guard: corrupt cyclic deps must terminate safely.
    let cyclic = vec![
        ("x".to_string(), "y".to_string()),
        ("y".to_string(), "x".to_string()),
    ];
    let mut cyc_tasks = vec![
        make_task("y", Priority::P2, 1, TaskState::Ready),
        make_task("x", Priority::P2, 1, TaskState::Ready),
    ];
    sort_ready_by_priority_and_critical_path(&mut cyc_tasks, &cyclic);
    assert_eq!(cyc_tasks.len(), 2);
}

#[test]
fn blocked_upstream_tasks_detects_direct_and_transitive_blockers() {
    let a = make_task("a", Priority::P1, 1, TaskState::Blocked);
    let b = make_task("b", Priority::P1, 2, TaskState::Ready);
    let c = make_task("c", Priority::P1, 3, TaskState::Ready);
    let done_mid = make_task("done_mid", Priority::P1, 4, TaskState::Done);
    let after_done = make_task("after_done", Priority::P1, 5, TaskState::Ready);
    let solo = make_task("solo", Priority::P1, 6, TaskState::Ready);

    let all = vec![
        a,
        b.clone(),
        c.clone(),
        done_mid,
        after_done.clone(),
        solo.clone(),
    ];
    let ready = vec![b, c, after_done, solo];
    let deps = vec![
        ("b".to_string(), "a".to_string()),
        ("c".to_string(), "b".to_string()),
        ("done_mid".to_string(), "a".to_string()),
        ("after_done".to_string(), "done_mid".to_string()),
    ];

    let blocked = blocked_upstream_tasks(&ready, &all, &deps);
    assert_eq!(
        blocked,
        vec![
            ("b".to_string(), "a".to_string()),
            ("c".to_string(), "a".to_string()),
        ]
    );
}

#[test]
fn apply_split_persists_owned_paths_into_subtask_scope_paths() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("aaf.db")).unwrap();
    let parent = make_task("t-parent", Priority::P1, 10, TaskState::Ready);
    db.insert_task(&parent).unwrap();

    let plans = vec![
        SplitSubtask {
            key: "api".to_string(),
            title: "Update API".to_string(),
            description: "API changes".to_string(),
            acceptance: vec!["api tests pass".to_string()],
            owned_paths: vec!["./src/api/".to_string()],
            depends_on: Vec::new(),
        },
        SplitSubtask {
            key: "ui".to_string(),
            title: "Update UI".to_string(),
            description: "UI changes".to_string(),
            acceptance: vec!["ui tests pass".to_string()],
            owned_paths: vec!["src/ui".to_string()],
            depends_on: Vec::new(),
        },
    ];

    let out = apply_split(&db, &parent, &plans, &UnblockerConfig::default(), 20, false).unwrap();
    assert!(out.split);
    assert_eq!(out.subtasks.len(), 2);
    let first = db.get_task(&out.subtasks[0].id).unwrap();
    let second = db.get_task(&out.subtasks[1].id).unwrap();
    assert_eq!(first.scope_paths, vec!["src/api"]);
    assert_eq!(second.scope_paths, vec!["src/ui"]);
}
