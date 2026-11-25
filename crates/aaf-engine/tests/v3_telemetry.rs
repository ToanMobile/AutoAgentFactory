#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Pillar 2 (OpenAgentd essence): read-only telemetry, timeline trace, and plan prompt.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::blocked::BlockedReason;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{
    AccountStore, EventSink, KvStore, META_CONSECUTIVE_ROOT_FAILS, META_FACTORY_FROZEN, RunStore,
    TaskStore,
};
use aaf_domain::run::{RunFinish, RunRecord, RunStatus};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::plan_mode::build_plan_prompt;
use aaf_engine::telemetry::{
    CostBasis, CostPolicy, CostRates, StatusQuery, collect_status, collect_status_from_db,
    collect_trace, collect_trace_at, inspect_throttle,
};

#[test]
fn telemetry_status_and_trace_aggregate_costs_runs_and_events() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("state.db")).unwrap();

    db.upsert_account(&AccountProfile {
        id: "work".to_string(),
        provider: ProviderKind::ClaudeCode,
        config_dir: dir
            .path()
            .join("claude-work")
            .to_string_lossy()
            .into_owned(),
        name: "work".to_string(),
    })
    .unwrap();
    db.set_meta(META_CONSECUTIVE_ROOT_FAILS, "2").unwrap();
    db.set_meta(META_FACTORY_FROZEN, "0").unwrap();

    let spec = ImmutableSpec {
        description: "Add OAuth login".to_string(),
        acceptance: vec!["Token refreshes cleanly".to_string()],
    };
    let mut t1 = Task::new_root("t-1", "/repo", "OAuth", spec.clone(), Priority::P1, 100);
    t1.state = TaskState::Review;
    db.insert_task(&t1).unwrap();
    let t2 = Task::new_root(
        "t-2",
        "/repo",
        "Backlog one",
        spec.clone(),
        Priority::P2,
        110,
    );
    db.insert_task(&t2).unwrap();
    let mut t3 = Task::new_root(
        "t-3",
        "/repo",
        "Blocked infra",
        spec.clone(),
        Priority::P1,
        120,
    );
    t3.state = TaskState::Blocked;
    t3.blocked_reason = Some(BlockedReason::Infra {
        detail: "401".to_string(),
    });
    db.insert_task(&t3).unwrap();
    let mut t4 = Task::new_root("t-4", "/repo", "Blocked gate", spec, Priority::P2, 125);
    t4.state = TaskState::Blocked;
    t4.blocked_reason = Some(BlockedReason::GateFailed {
        summary: "lint".to_string(),
    });
    db.insert_task(&t4).unwrap();

    db.insert_run(&RunRecord {
        id: "r-1".to_string(),
        task_id: "t-1".to_string(),
        account_id: Some("work".to_string()),
        executor: "claude".to_string(),
        worktree_path: "/repo/.wt/t-1".to_string(),
        branch: "aaf/t-1".to_string(),
        status: RunStatus::Running,
        exit_code: None,
        tokens_in: 0,
        tokens_out: 0,
        started_at: 200,
        ended_at: None,
    })
    .unwrap();
    db.finish_run(
        "r-1",
        &RunFinish {
            status: RunStatus::Succeeded,
            exit_code: Some(0),
            tokens_in: 1_000_000,
            tokens_out: 500_000,
            ended_at: 245,
        },
    )
    .unwrap();

    // Hold a live exclusive account lease lock to verify read-only lease detection.
    let leases_dir = dir.path().join("leases");
    fs::create_dir_all(&leases_dir).unwrap();
    let lease_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(leases_dir.join("work.lock"))
        .unwrap();
    lease_file.try_lock().unwrap();

    let cost = CostPolicy {
        rates: CostRates::default(),
        api_accounts: BTreeSet::from(["work".to_string()]),
    };
    let query = StatusQuery {
        repo_root: Some("/repo"),
        data_dir: Some(dir.path()),
        now: 300,
        cost: &cost,
    };
    let status = collect_status(&db, &db, &db, Some(&db), &query).unwrap();
    let status_db = collect_status_from_db(&db, &query).unwrap();
    assert_eq!(status, status_db);

    assert!(!status.frozen);
    assert_eq!(status.consecutive_root_fails, 2);
    assert_eq!(status.counts.review, 1);
    assert_eq!(status.counts.backlog, 1);
    assert_eq!(status.counts.blocked, 2);
    assert_eq!(status.counts.total, 4);
    assert_eq!(status.blocked_reasons.get("infra"), Some(&1));
    assert_eq!(status.blocked_reasons.get("gate_failed"), Some(&1));
    assert_eq!(status.runs.succeeded, 1);
    assert_eq!(status.tokens_in, 1_000_000);
    assert_eq!(status.tokens_out, 500_000);
    assert_eq!(status.cost_basis, CostBasis::ApiAccounts);
    assert!((status.estimated_cost_usd.unwrap() - 10.5).abs() < 1e-9);
    assert_eq!(status.accounts.len(), 1);
    assert_eq!(status.accounts[0].status, "leased");

    // Releasing the lease flips account status back to available.
    drop(lease_file);
    let after_release = collect_status_from_db(&db, &query).unwrap();
    assert_eq!(after_release.accounts[0].status, "available");

    // Emit events sharing timestamps with the run and each other to test ordering & truncation.
    db.emit(&FactoryEvent::new(EventKind::TaskCreated, r#"{"seq":1}"#, 200).for_task("t-1"))
        .unwrap();
    db.emit(&FactoryEvent::new(EventKind::RunStarted, r#"{"seq":2}"#, 200).for_task("t-1"))
        .unwrap();
    db.emit(&FactoryEvent::new(EventKind::RunFinished, r#"{"seq":3}"#, 245).for_task("t-1"))
        .unwrap();

    let trace = collect_trace(&db, &db, &db, "t-1", &cost, 50).unwrap();
    assert_eq!(trace.task_id, "t-1");
    assert_eq!(trace.total_tokens_in, 1_000_000);
    assert_eq!(trace.total_tokens_out, 500_000);
    assert_eq!(trace.total_wall_secs, 45);
    assert_eq!(trace.cost_basis, CostBasis::ApiAccounts);
    assert!((trace.estimated_cost_usd.unwrap() - 10.5).abs() < 1e-9);
    assert!(!trace.events_truncated);
    assert_eq!(trace.entries.len(), 4);
    // At timestamp 200: "run" sorts before "event", and same-timestamp events stay in chronological id ASC order.
    assert_eq!(trace.entries[0].source, "run");
    assert_eq!(trace.entries[0].timestamp, 200);
    assert_eq!(trace.entries[1].source, "event");
    assert_eq!(trace.entries[1].kind, "task_created");
    assert_eq!(trace.entries[2].source, "event");
    assert_eq!(trace.entries[2].kind, "run_started");
    assert_eq!(trace.entries[3].timestamp, 245);

    // Limit smaller than total events sets events_truncated = true and keeps the newest events.
    let truncated = collect_trace(&db, &db, &db, "t-1", &cost, 2).unwrap();
    assert!(truncated.events_truncated);
    assert_eq!(truncated.entries.len(), 3); // 1 run + 2 most recent events

    // Live running run calculates wall secs using `now` with saturating_sub.
    db.insert_run(&RunRecord {
        id: "r-open".to_string(),
        task_id: "t-2".to_string(),
        account_id: None,
        executor: "claude".to_string(),
        worktree_path: "/repo/.wt/t-2".to_string(),
        branch: "aaf/t-2".to_string(),
        status: RunStatus::Running,
        exit_code: None,
        tokens_in: 100,
        tokens_out: 50,
        started_at: 500,
        ended_at: None,
    })
    .unwrap();
    let live = collect_trace_at(&db, &db, &db, "t-2", &cost, 10, 530).unwrap();
    assert_eq!(live.total_wall_secs, 30);
    assert_eq!(live.estimated_cost_usd, None);
    assert_eq!(live.cost_basis, CostBasis::None);
    let skewed = collect_trace_at(&db, &db, &db, "t-2", &cost, 10, 400).unwrap();
    assert_eq!(skewed.total_wall_secs, 0);
}

#[test]
fn cost_policy_excludes_subscription_none_account_and_unused_api_accounts() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("state.db")).unwrap();

    let spec = ImmutableSpec {
        description: "Cost policy".to_string(),
        acceptance: vec!["ok".to_string()],
    };
    db.insert_task(&Task::new_root(
        "t-sub",
        "/repo-sub",
        "Sub only",
        spec.clone(),
        Priority::P2,
        100,
    ))
    .unwrap();
    db.insert_task(&Task::new_root(
        "t-mix",
        "/repo-mix",
        "Mixed",
        spec,
        Priority::P2,
        100,
    ))
    .unwrap();

    let add_run = |id: &str, task: &str, acct: Option<&str>, t_in: u64, t_out: u64| {
        db.insert_run(&RunRecord {
            id: id.to_string(),
            task_id: task.to_string(),
            account_id: acct.map(str::to_string),
            executor: "claude".to_string(),
            worktree_path: "/wt".to_string(),
            branch: "aaf/b".to_string(),
            status: RunStatus::Running,
            exit_code: None,
            tokens_in: 0,
            tokens_out: 0,
            started_at: 100,
            ended_at: None,
        })
        .unwrap();
        db.finish_run(
            id,
            &RunFinish {
                status: RunStatus::Succeeded,
                exit_code: Some(0),
                tokens_in: t_in,
                tokens_out: t_out,
                ended_at: 120,
            },
        )
        .unwrap();
    };

    // Repo with only subscription runs (1M in + 0 out).
    add_run("r-sub-only", "t-sub", Some("sub-acct"), 1_000_000, 0);
    // Mixed repo: subscription + API + account_id = None.
    add_run("r-mix-sub", "t-mix", Some("sub-acct"), 2_000_000, 1_000_000);
    add_run("r-mix-none", "t-mix", None, 500_000, 500_000);
    add_run("r-mix-api", "t-mix", Some("api-acct"), 1_000_000, 200_000);

    // 1. Empty api_accounts -> always None / CostBasis::None.
    let empty_policy = CostPolicy::default();
    let q_empty = StatusQuery {
        repo_root: None,
        data_dir: None,
        now: 200,
        cost: &empty_policy,
    };
    let s_empty = collect_status(&db, &db, &db, Some(&db), &q_empty).unwrap();
    let s_empty_db = collect_status_from_db(&db, &q_empty).unwrap();
    assert_eq!(s_empty, s_empty_db);
    assert_eq!(s_empty.estimated_cost_usd, None);
    assert_eq!(s_empty.cost_basis, CostBasis::None);

    // 2. Machine has API account configured, but scoped repo has ONLY subscription runs -> None.
    let api_policy = CostPolicy {
        rates: CostRates::default(),
        api_accounts: BTreeSet::from(["api-acct".to_string()]),
    };
    let q_sub_repo = StatusQuery {
        repo_root: Some("/repo-sub"),
        data_dir: None,
        now: 200,
        cost: &api_policy,
    };
    let s_sub = collect_status(&db, &db, &db, Some(&db), &q_sub_repo).unwrap();
    let s_sub_db = collect_status_from_db(&db, &q_sub_repo).unwrap();
    assert_eq!(s_sub, s_sub_db);
    assert_eq!(s_sub.tokens_in, 1_000_000);
    assert_eq!(s_sub.estimated_cost_usd, None);
    assert_eq!(s_sub.cost_basis, CostBasis::None);

    // 3. Mixed repo: only api-acct (1M in = $3, 0.2M out = $3 => $6.00) is priced;
    //    sub-acct and account_id=None are excluded from USD while still counted in tokens.
    let q_mix = StatusQuery {
        repo_root: Some("/repo-mix"),
        data_dir: None,
        now: 200,
        cost: &api_policy,
    };
    let s_mix = collect_status(&db, &db, &db, Some(&db), &q_mix).unwrap();
    let s_mix_db = collect_status_from_db(&db, &q_mix).unwrap();
    assert_eq!(s_mix, s_mix_db);
    assert_eq!(s_mix.tokens_in, 3_500_000);
    assert_eq!(s_mix.tokens_out, 1_700_000);
    assert_eq!(s_mix.cost_basis, CostBasis::ApiAccounts);
    assert!((s_mix.estimated_cost_usd.unwrap() - 6.0).abs() < 1e-9);

    let trace_mix = collect_trace(&db, &db, &db, "t-mix", &api_policy, 10).unwrap();
    assert_eq!(trace_mix.cost_basis, CostBasis::ApiAccounts);
    assert!((trace_mix.estimated_cost_usd.unwrap() - 6.0).abs() < 1e-9);

    let trace_sub = collect_trace(&db, &db, &db, "t-sub", &api_policy, 10).unwrap();
    assert_eq!(trace_sub.estimated_cost_usd, None);
    assert_eq!(trace_sub.cost_basis, CostBasis::None);
}

#[test]
fn inspect_throttle_probes_live_advisory_locks_not_stale_files() {
    let dir = tempfile::tempdir().unwrap();
    let throttle_dir = dir.path().join("throttle");
    fs::create_dir_all(throttle_dir.join("guest")).unwrap();

    let open_rw = |name: &str| {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(throttle_dir.join(name))
            .unwrap()
    };

    let slot0 = open_rw("heavy.0.lock");
    let _slot1 = open_rw("heavy.1.lock");
    let drain = open_rw("drain.lock");
    fs::write(throttle_dir.join("guest/heavy.0.rec"), "pid=1\n").unwrap();

    // Files exist on disk, but nothing is locked yet.
    let idle = inspect_throttle(dir.path());
    assert!(idle.throttle_dir_exists);
    assert_eq!(idle.slot_lock_files, 2);
    assert_eq!(idle.throttle_slots, 0);
    assert!(!idle.drain_lock_present);
    assert_eq!(idle.guest_record_files, 1);

    // Lock slot 0 and drain.lock exclusively.
    slot0.try_lock().unwrap();
    drain.try_lock().unwrap();
    let active = inspect_throttle(dir.path());
    assert_eq!(active.slot_lock_files, 2);
    assert_eq!(active.throttle_slots, 1);
    assert!(active.drain_lock_present);

    // Dropping the lock handles clears active counts immediately.
    drop(slot0);
    drop(drain);
    let cleared = inspect_throttle(dir.path());
    assert_eq!(cleared.slot_lock_files, 2);
    assert_eq!(cleared.throttle_slots, 0);
    assert!(!cleared.drain_lock_present);
}

#[test]
fn plan_mode_builds_read_only_prompt_and_rejects_traversal() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("src.rs"), "pub fn hi() {}\n").unwrap();
    fs::write(dir.path().join("outside.rs"), "secret").unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("lib.rs"), "pub fn ok() {}\n").unwrap();

    let err = build_plan_prompt(&repo, "Audit auth", &["../outside.rs".to_string()]).unwrap_err();
    assert!(err.to_string().contains("outside") || err.to_string().contains("invalid"));

    let plan = build_plan_prompt(&repo, "Design token cache", &["lib.rs".to_string()]).unwrap();
    assert!(plan.read_only);
    assert!(plan.prompt.contains("READ-ONLY"));
    assert!(plan.prompt.contains("Design token cache"));
    assert!(plan.prompt.contains("pub fn ok()"));
}
