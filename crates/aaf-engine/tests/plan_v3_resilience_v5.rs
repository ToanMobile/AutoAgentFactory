#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Plan v3 Wave 3 Slice 2 resilience tests:
//! - DB Schema v4 -> v5 migration & `Db::open_read_only` v3/v4/v5 compatibility
//! - `wake_due_blocked_tasks` (Infra/Quota only, cap 6, no `"unblocked:"` note)
//! - Per-repo freeze isolation, cooldown probe, probe cap, and `board_enrichment`
//! - `proposer.rs` 1-shot format-repair retry & harmless extra LLM JSON fields
//! - Throttle `Pressure::Warn` & `Busy::Orphaned` bounded waits
//! - `gc.rs` low-disk guard and lightweight periodic sweep

use std::collections::VecDeque;
use std::fs;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aaf_domain::blocked::BlockedReason;
use aaf_domain::budget::RootBudget;
use aaf_domain::config::{MachineConfig, UnblockerConfig};
use aaf_domain::event::EventKind;
use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, BudgetStore, Clock, EventSink, KvStore, PortResult,
    SandboxPolicy, TaskStore,
};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::{Db, SCHEMA_VERSION};
use aaf_engine::gc::{GcError, check_free_disk_bytes, lightweight_sweep};
use aaf_engine::throttle::{
    Ask, GroupControl, GroupState, LoadProbe, Pressure, PriorityTools, Sleeper, Throttle, paths,
};
use aaf_engine::unblocker::ladder::{
    MAX_REPO_PROBE_COUNT, REPO_FREEZE_COOLDOWN_SECS, is_repo_frozen, record_repo_root_result,
    repo_frozen_key, try_begin_repo_probe, unfreeze_repo,
};
use aaf_engine::unblocker::{AgentSubtaskProposer, SubtaskProposer, parse_plans};
use rusqlite::Connection;
use tempfile::TempDir;

const V3_SCHEMA: &str = "
    CREATE TABLE tasks (
        id TEXT PRIMARY KEY, repo_root TEXT NOT NULL, parent_id TEXT REFERENCES tasks(id),
        root_id TEXT NOT NULL, depth INTEGER NOT NULL, title TEXT NOT NULL, spec TEXT NOT NULL,
        context_notes TEXT NOT NULL, state TEXT NOT NULL, priority TEXT NOT NULL,
        fingerprint TEXT, blocked_reason TEXT, unblock_cycles INTEGER NOT NULL DEFAULT 0,
        created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
    CREATE TABLE root_budgets (
        root_id TEXT PRIMARY KEY, tokens_used INTEGER NOT NULL, token_limit INTEGER NOT NULL,
        wall_secs_used INTEGER NOT NULL, wall_secs_limit INTEGER NOT NULL,
        cycles_used INTEGER NOT NULL, cycles_limit INTEGER NOT NULL, frozen INTEGER NOT NULL);
    CREATE TABLE task_deps (
        task_id TEXT NOT NULL REFERENCES tasks(id), blocked_by TEXT NOT NULL REFERENCES tasks(id),
        PRIMARY KEY (task_id, blocked_by));
    CREATE TABLE runs (
        id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id), account_id TEXT,
        executor TEXT NOT NULL, worktree_path TEXT NOT NULL, branch TEXT NOT NULL,
        status TEXT NOT NULL, exit_code INTEGER, tokens_in INTEGER NOT NULL,
        tokens_out INTEGER NOT NULL, started_at INTEGER NOT NULL, ended_at INTEGER);
    CREATE TABLE accounts (
        id TEXT PRIMARY KEY, provider TEXT NOT NULL, config_dir TEXT NOT NULL,
        cooldown_until INTEGER, name TEXT NOT NULL DEFAULT '');
    CREATE TABLE events (
        id INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT, run_id TEXT, kind TEXT NOT NULL,
        payload_json TEXT NOT NULL, created_at INTEGER NOT NULL);
    CREATE TABLE closed_fingerprints (
        id INTEGER PRIMARY KEY, fingerprint TEXT NOT NULL, repo_root TEXT NOT NULL,
        task_id TEXT NOT NULL, closed_at INTEGER NOT NULL);
    CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
    PRAGMA user_version = 3;
";

const V4_STEP: &str = "
    ALTER TABLE tasks ADD COLUMN scope_paths TEXT NOT NULL DEFAULT '[]';
    CREATE TABLE turn_snapshots (
        id INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT NOT NULL REFERENCES tasks(id),
        run_id TEXT NOT NULL, ref_name TEXT NOT NULL, commit_oid TEXT NOT NULL,
        created_at INTEGER NOT NULL);
    PRAGMA user_version = 4;
";

fn make_task(id: &str, repo: &str) -> Task {
    let mut t = Task::new_root(
        id,
        repo,
        format!("task {id}"),
        ImmutableSpec {
            description: "desc".to_string(),
            acceptance: vec!["ok".to_string()],
        },
        Priority::P1,
        100,
    );
    t.state = TaskState::Ready;
    t
}

#[test]
fn schema_v4_to_v5_migration_and_open_read_only_compat() {
    assert_eq!(SCHEMA_VERSION, 5);
    let tmp = TempDir::new().unwrap();

    // 1. Read-only compatibility on a v3 database (no scope_paths, retry_after, auto_recover_count).
    let v3_path = tmp.path().join("v3.db");
    {
        let conn = Connection::open(&v3_path).unwrap();
        conn.execute_batch(V3_SCHEMA).unwrap();
        conn.execute_batch(
            r#"INSERT INTO tasks VALUES ('t-v3', '/repo', NULL, 't-v3', 0, 'v3 task',
               '{"description":"d","acceptance":["a"]}', '[]', 'ready', 'P1', NULL, NULL, 0, 10, 10);"#,
        )
        .unwrap();
    }
    let ro_v3 = Db::open_read_only(&v3_path).unwrap();
    assert_eq!(ro_v3.schema_version(), 3);
    let t_v3 = ro_v3.get_task("t-v3").unwrap();
    assert!(t_v3.scope_paths.is_empty());
    assert_eq!(t_v3.retry_after, None);
    assert_eq!(t_v3.auto_recover_count, 0);
    drop(ro_v3);

    // 2. Read-only compatibility on a v4 database, then read-write migration v4 -> v5.
    let v4_path = tmp.path().join("v4.db");
    {
        let conn = Connection::open(&v4_path).unwrap();
        conn.execute_batch(V3_SCHEMA).unwrap();
        conn.execute_batch(V4_STEP).unwrap();
        conn.execute_batch(
            r#"INSERT INTO tasks VALUES ('t-v4', '/repo', NULL, 't-v4', 0, 'v4 task',
               '{"description":"d","acceptance":["a"]}', '[]', 'blocked', 'P0', NULL,
               '{"kind":"infra","detail":"net"}', 1, 20, 20, '["src/a.rs"]');"#,
        )
        .unwrap();
    }
    let ro_v4 = Db::open_read_only(&v4_path).unwrap();
    assert_eq!(ro_v4.schema_version(), 4);
    let t_v4 = ro_v4.get_task("t-v4").unwrap();
    assert_eq!(t_v4.scope_paths, vec!["src/a.rs"]);
    assert_eq!(t_v4.retry_after, None);
    assert_eq!(t_v4.auto_recover_count, 0);
    drop(ro_v4);

    let rw = Db::open(&v4_path).unwrap();
    assert_eq!(rw.schema_version(), 5);
    rw.set_retry_schedule("t-v4", Some(500), 2, 100).unwrap();
    let migrated = rw.get_task("t-v4").unwrap();
    assert_eq!(migrated.retry_after, Some(500));
    assert_eq!(migrated.auto_recover_count, 2);
    drop(rw);

    let ro_v5 = Db::open_read_only(&v4_path).unwrap();
    assert_eq!(ro_v5.schema_version(), 5);
    let read_back = ro_v5.get_task("t-v4").unwrap();
    assert_eq!(read_back.retry_after, Some(500));
    assert_eq!(read_back.auto_recover_count, 2);
}

#[test]
fn wake_due_blocked_tasks_filters_reasons_caps_at_6_and_preserves_ladder_markers() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    let repo = "/repo-wake";

    let cases: [(&str, BlockedReason, bool); 7] = [
        (
            "t-infra",
            BlockedReason::Infra {
                detail: "503".into(),
            },
            true,
        ),
        (
            "t-quota",
            BlockedReason::Quota {
                reset_at: Some(900),
            },
            true,
        ),
        (
            "t-human",
            BlockedReason::NeedsHuman {
                why: "review".into(),
            },
            false,
        ),
        (
            "t-budget",
            BlockedReason::BudgetExceeded {
                detail: "cap".into(),
            },
            false,
        ),
        (
            "t-conflict",
            BlockedReason::MergeConflict {
                detail: "a.rs".into(),
            },
            false,
        ),
        (
            "t-stuck",
            BlockedReason::Stuck {
                detail: "loop".into(),
            },
            false,
        ),
        (
            "t-gate",
            BlockedReason::GateFailed {
                summary: "err".into(),
            },
            false,
        ),
    ];

    for (id, reason, _) in &cases {
        let mut t = make_task(id, repo);
        t.context_notes
            .push("unblock[t3] digest=abc: prev".to_string());
        db.insert_task(&t).unwrap();
        db.set_state(id, TaskState::Blocked, Some(reason), 100)
            .unwrap();
        db.set_retry_schedule(id, Some(200), 0, 100).unwrap();
    }

    // Before deadline: nothing wakes.
    assert!(db.wake_due_blocked_tasks(repo, 199).unwrap().is_empty());

    // At deadline: only Infra and Quota wake.
    let woken = db.wake_due_blocked_tasks(repo, 200).unwrap();
    assert_eq!(woken, vec!["t-infra", "t-quota"]);

    for (id, _, should_wake) in &cases {
        let t = db.get_task(id).unwrap();
        if *should_wake {
            assert_eq!(t.state, TaskState::Ready, "{id}");
            assert_eq!(t.blocked_reason, None, "{id}");
            assert_eq!(t.retry_after, None, "{id}");
            assert_eq!(t.auto_recover_count, 1, "{id}");
            assert!(
                !t.context_notes.iter().any(|n| n.starts_with("unblocked:")),
                "must never append unblocked: note: {:?}",
                t.context_notes
            );
            let evs = db.events_for_task(id, 5).unwrap();
            assert_eq!(evs[0].kind, EventKind::TaskStateChanged);
            let payload: serde_json::Value = serde_json::from_str(&evs[0].payload_json).unwrap();
            assert_eq!(payload["to"], "ready");
            assert_eq!(payload["why"], "auto_retry_due");
        } else {
            assert_eq!(t.state, TaskState::Blocked, "{id} must stay blocked");
            assert_eq!(t.auto_recover_count, 0, "{id}");
        }
    }

    // Re-block t-infra and wake up to cap 6; 7th attempt must NOT wake.
    let infra = BlockedReason::Infra {
        detail: "net".into(),
    };
    for expected_count in 2..=6u32 {
        let ts = 200 + i64::from(expected_count) * 10;
        db.set_state("t-infra", TaskState::Blocked, Some(&infra), ts)
            .unwrap();
        // Keep existing auto_recover_count (expected_count - 1).
        let cur = db.get_task("t-infra").unwrap().auto_recover_count;
        assert_eq!(cur, expected_count - 1);
        db.set_retry_schedule("t-infra", Some(ts + 5), cur, ts)
            .unwrap();
        assert_eq!(
            db.wake_due_blocked_tasks(repo, ts + 5).unwrap(),
            vec!["t-infra"]
        );
        assert_eq!(
            db.get_task("t-infra").unwrap().auto_recover_count,
            expected_count
        );
    }

    // At auto_recover_count == 6, wake_due_blocked_tasks refuses to auto-wake again.
    db.set_state("t-infra", TaskState::Blocked, Some(&infra), 500)
        .unwrap();
    db.set_retry_schedule("t-infra", Some(505), 6, 500).unwrap();
    assert!(db.wake_due_blocked_tasks(repo, 999).unwrap().is_empty());
    assert_eq!(db.get_task("t-infra").unwrap().state, TaskState::Blocked);

    // Manual Blocked -> Ready clears retry_after while preserving auto_recover_count;
    // transitioning to Done/Review/Cancelled resets auto_recover_count to 0.
    db.set_state("t-infra", TaskState::Ready, None, 600)
        .unwrap();
    let ready = db.get_task("t-infra").unwrap();
    assert_eq!(ready.retry_after, None);
    assert_eq!(ready.auto_recover_count, 6);
    db.set_state("t-infra", TaskState::Cancelled, None, 601)
        .unwrap();
    assert_eq!(db.get_task("t-infra").unwrap().auto_recover_count, 0);
}

#[test]
fn per_repo_freeze_isolates_repos_and_caps_half_open_probes() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    let repo_a = "/repos/alpha";
    let repo_b = "/repos/beta";

    // Three consecutive root failures freeze alpha only.
    assert!(!record_repo_root_result(&db, repo_a, "r1", false, 1_000).unwrap());
    assert!(!record_repo_root_result(&db, repo_a, "r2", false, 1_001).unwrap());
    assert!(record_repo_root_result(&db, repo_a, "r3", false, 1_002).unwrap());

    assert!(is_repo_frozen(&db, repo_a, 1_002).unwrap());
    assert!(!is_repo_frozen(&db, repo_b, 1_002).unwrap());
    assert!(db.board_enrichment(Some(repo_a)).unwrap().frozen);
    assert!(!db.board_enrichment(Some(repo_b)).unwrap().frozen);

    // Still frozen just before cooldown expires.
    let probe_at = 1_002 + REPO_FREEZE_COOLDOWN_SECS;
    assert!(is_repo_frozen(&db, repo_a, probe_at - 1).unwrap());

    // Once cooldown expires, half-open probe opens (capped at MAX_REPO_PROBE_COUNT).
    let mut now = probe_at;
    for _ in 0..MAX_REPO_PROBE_COUNT {
        assert!(!is_repo_frozen(&db, repo_a, now).unwrap());
        assert!(try_begin_repo_probe(&db, repo_a, now).unwrap());
        assert_eq!(
            db.get_meta(&repo_frozen_key(repo_a)).unwrap().as_deref(),
            Some("probe")
        );
        assert!(!db.board_enrichment(Some(repo_a)).unwrap().frozen);
        // Probe fails -> re-freezes and increments probe count.
        assert!(record_repo_root_result(&db, repo_a, "rp", false, now).unwrap());
        now += REPO_FREEZE_COOLDOWN_SECS;
    }

    // After MAX_REPO_PROBE_COUNT failed probes, no more automatic probing occurs.
    assert!(is_repo_frozen(&db, repo_a, now + 100_000).unwrap());
    assert!(!try_begin_repo_probe(&db, repo_a, now + 100_000).unwrap());

    // Explicit unfreeze clears repo freeze keys.
    unfreeze_repo(&db, repo_a).unwrap();
    assert!(!is_repo_frozen(&db, repo_a, now + 100_000).unwrap());

    // Probe success automatically unfreezes repo_b.
    for i in 0..3 {
        let _ = record_repo_root_result(&db, repo_b, "rb", false, 2_000 + i).unwrap();
    }
    assert!(is_repo_frozen(&db, repo_b, 2_005).unwrap());
    let recover_at = 2_002 + REPO_FREEZE_COOLDOWN_SECS;
    assert!(!record_repo_root_result(&db, repo_b, "rb", true, recover_at).unwrap());
    assert!(!is_repo_frozen(&db, repo_b, recover_at).unwrap());
}

struct SeqRunner {
    outcomes: Mutex<VecDeque<AgentOutcome>>,
    run_ids: Mutex<Vec<String>>,
}

impl AgentRunner for SeqRunner {
    fn run(&self, request: &AgentRequest) -> PortResult<AgentOutcome> {
        self.run_ids.lock().unwrap().push(request.run_id.clone());
        Ok(self.outcomes.lock().unwrap().pop_front().unwrap())
    }
}

#[test]
fn proposer_ignores_harmless_extra_keys_and_recovers_via_format_repair() {
    // Harmless extra LLM fields are allowed; smuggled task attributes stay rejected.
    let with_extra = r#"[{"title":"Step 1","description":"Refactor parser","acceptance":["tests pass"],"rationale":"clean split","files":["src/p.rs"]}]"#;
    let parsed = parse_plans(with_extra);
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].title, "Step 1");

    let smuggled = r#"[{"title":"Step 1","description":"d","acceptance":["a"],"state":"done"}]"#;
    assert!(parse_plans(smuggled).is_empty());

    // 1-shot format-repair recovers when attempt 1 returns prose instead of JSON.
    let tmp = TempDir::new().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    let task = make_task("t-prop", "/repo");
    db.insert_task(&task).unwrap();
    db.put_budget(&RootBudget::new("t-prop", &UnblockerConfig::default()))
        .unwrap();

    let runner = SeqRunner {
        outcomes: Mutex::new(VecDeque::from([
            AgentOutcome {
                exit_code: Some(0),
                timed_out: false,
                final_text: "I recommend splitting into two parts: parser and tests.".into(),
                tokens_in: 300,
                tokens_out: 100,
                limit_hit: None,
                infra: None,
                log_tail: String::new(),
            },
            AgentOutcome {
                exit_code: Some(0),
                timed_out: false,
                final_text: with_extra.into(),
                tokens_in: 200,
                tokens_out: 50,
                limit_hit: None,
                infra: None,
                log_tail: String::new(),
            },
        ])),
        run_ids: Mutex::new(Vec::new()),
    };

    let proposer = AgentSubtaskProposer {
        runner: &runner,
        budgets: &db,
        base: AgentRequest {
            run_id: "prop-1".into(),
            prompt: String::new(),
            cwd: "/repo".into(),
            account: None,
            timeout_secs: 60,
            sandbox: SandboxPolicy::default(),
            json_schema: None,
        },
    };
    let plans = proposer
        .propose(&task, &BlockedReason::Stuck { detail: "x".into() }, "dig1")
        .unwrap();
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].title, "Step 1");
    assert_eq!(db.get_budget("t-prop").unwrap().tokens_used, 650);
    let ids = runner.run_ids.lock().unwrap().clone();
    assert_eq!(ids, vec!["prop-1", "prop-1-repair"]);
}

struct FakeClock(AtomicI64);
impl Clock for FakeClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct AdvSleeper {
    clock: Arc<FakeClock>,
}
impl Sleeper for AdvSleeper {
    fn sleep(&self, duration: Duration) {
        let secs = i64::try_from(duration.as_secs().max(1)).unwrap_or(1);
        self.clock.0.fetch_add(secs, Ordering::SeqCst);
    }
}

struct FixedProbe(Pressure);
impl LoadProbe for FixedProbe {
    fn load_percent(&self) -> Option<u32> {
        Some(10)
    }
    fn pressure(&self) -> Option<Pressure> {
        Some(self.0)
    }
    fn free_memory_bytes(&self) -> Option<u64> {
        Some(16 * 1024 * 1024 * 1024)
    }
}

struct OrphanGroup(Option<u32>);
impl GroupControl for OrphanGroup {
    fn rss_bytes(&self, _: u32) -> Option<u64> {
        None
    }
    fn stop(&self, _: u32) -> bool {
        true
    }
    fn resume(&self, _: u32) -> bool {
        true
    }
    fn group_state(&self, pgid: u32) -> Option<GroupState> {
        (self.0 == Some(pgid)).then(|| GroupState {
            leader_started: Some("Sun Nov 22 11:00:00 2026".into()),
            stopped: false,
        })
    }
}

#[test]
fn throttle_admit_bounds_pressure_warn_and_reaps_orphaned_slots() {
    let data = TempDir::new().unwrap();
    let machine = MachineConfig {
        max_heavy_parallel: 1,
        heavy_wait_secs: 10,
        ..MachineConfig::default()
    };

    // 1. Sustained Pressure::Warn falls back to degraded once heavy_wait_secs elapses.
    let clock = Arc::new(FakeClock(AtomicI64::new(1_000)));
    let sleeper = AdvSleeper {
        clock: Arc::clone(&clock),
    };
    let probe = FixedProbe(Pressure::Warn);
    let control = OrphanGroup(None);
    let throttle = Throttle {
        machine: &machine,
        probe: &probe,
        sleeper: &sleeper,
        control: &control,
        progress: &|_| {},
        cpus: 4,
        tools: PriorityTools::default(),
        jitter_seed: 7,
        sample_every: Duration::from_secs(2),
    };
    let ask = Ask {
        repo_id: "r",
        label: "test_full",
        tool: None,
        cmd_family: None,
        exclusive: false,
        wait_limit_secs: None,
    };
    let admitted = throttle
        .admit_ask(data.path(), &*clock, &ask)
        .unwrap()
        .unwrap();
    assert!(admitted.degraded);
    assert!(admitted.waited_secs >= 10);
    drop(admitted);

    // 2. Sustained Busy::Orphaned is reaped after heavy_wait_secs instead of hanging forever.
    let slot_file = paths::slot_path(data.path(), 0);
    let tok_file = paths::token_path(data.path(), 0);
    fs::create_dir_all(slot_file.parent().unwrap()).unwrap();
    fs::create_dir_all(tok_file.parent().unwrap()).unwrap();
    fs::write(&tok_file, "token=0123456789abcdef0123456789abcdef\n").unwrap();
    fs::write(
        &slot_file,
        "reserved=unknown\npgid=4242\nlstart=Sun Nov 22 11:00:00 2026\n",
    )
    .unwrap();

    let clock2 = Arc::new(FakeClock(AtomicI64::new(2_000)));
    let sleeper2 = AdvSleeper {
        clock: Arc::clone(&clock2),
    };
    let calm_probe = FixedProbe(Pressure::Normal);
    let orphan_ctrl = OrphanGroup(Some(4242));
    let throttle2 = Throttle {
        machine: &machine,
        probe: &calm_probe,
        sleeper: &sleeper2,
        control: &orphan_ctrl,
        progress: &|_| {},
        cpus: 4,
        tools: PriorityTools::default(),
        jitter_seed: 7,
        sample_every: Duration::from_secs(2),
    };
    let admitted2 = throttle2
        .admit_ask(data.path(), &*clock2, &ask)
        .unwrap()
        .unwrap();
    assert_eq!(admitted2.slot.index(), 0);
    assert!(admitted2.waited_secs >= 10);
}

#[test]
fn gc_low_disk_check_and_lightweight_sweep() {
    let tmp = TempDir::new().unwrap();
    assert!(check_free_disk_bytes(tmp.path(), 1).is_ok());
    assert!(matches!(
        check_free_disk_bytes(tmp.path(), u64::MAX),
        Err(GcError::LowDisk { .. })
    ));

    let data_dir = tmp.path().join("data");
    let wt_dir = tmp.path().join("worktrees");
    let streams_dir = data_dir.join("streams");
    fs::create_dir_all(&streams_dir).unwrap();
    fs::create_dir_all(&wt_dir).unwrap();
    let old_stream = streams_dir.join("old-run.jsonl");
    fs::write(&old_stream, "{}\n").unwrap();

    // Passing a `now` far in the future expires the freshly written stream file.
    let far_future = 4_000_000_000i64;
    let report = lightweight_sweep(tmp.path(), &data_dir, &wt_dir, far_future);
    assert!(!old_stream.exists());
    assert_eq!(report.removed_files.len(), 1);
}
