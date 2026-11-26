#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;

use aaf_domain::account::{AccountProfile, AccountState, ProviderKind};
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::run::{RunRecord, RunStatus};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::telemetry::ReportWindow;
use serde_json::json;

use super::{OpsInput, collect_ops_diagnostics};
use crate::cmd::report::empty_report;

fn make_task(id: &str, state: TaskState, cycles: u32, updated_at: i64) -> Task {
    let mut t = Task::new_root(
        id,
        "/tmp/repo",
        format!("Task {id}"),
        ImmutableSpec {
            description: String::new(),
            acceptance: Vec::new(),
        },
        Priority::P1,
        100,
    );
    t.state = state;
    t.unblock_cycles = cycles;
    t.updated_at = updated_at;
    t
}

fn make_account(id: &str, cooldown_until: Option<i64>) -> AccountState {
    AccountState {
        profile: AccountProfile {
            id: id.into(),
            name: id.into(),
            provider: ProviderKind::ClaudeCode,
            config_dir: "/tmp/cfg".into(),
        },
        cooldown_until,
    }
}

fn make_running_run(
    id: &str,
    task_id: &str,
    executor: &str,
    account_id: Option<&str>,
    started_at: i64,
) -> RunRecord {
    RunRecord {
        id: id.into(),
        task_id: task_id.into(),
        account_id: account_id.map(str::to_string),
        executor: executor.into(),
        worktree_path: "/tmp/wt".into(),
        branch: format!("aaf/{task_id}"),
        status: RunStatus::Running,
        exit_code: None,
        tokens_in: 0,
        tokens_out: 0,
        started_at,
        ended_at: None,
    }
}

#[test]
fn ops_diagnostics_flags_mech_zero_vacuous_churn_and_token_efficiency() {
    let now = 10_000;
    let tasks = vec![
        make_task("t-done", TaskState::Done, 0, now - 600),
        make_task("t-churn", TaskState::Gating, 5, now - 60),
    ];
    let mut runs_by_task: HashMap<String, Vec<RunRecord>> = HashMap::new();
    runs_by_task.insert(
        "t-done".into(),
        vec![RunRecord {
            id: "r1".into(),
            task_id: "t-done".into(),
            account_id: None,
            executor: "claude".into(),
            worktree_path: "/tmp/wt".into(),
            branch: "aaf/t-done".into(),
            status: RunStatus::Succeeded,
            exit_code: Some(0),
            tokens_in: 500_000,
            tokens_out: 50_000,
            started_at: now - 400,
            ended_at: Some(now - 200),
        }],
    );
    let events = vec![
        FactoryEvent::new(
            EventKind::GateResult,
            json!({
                "status": "CLEAR",
                "mechanical_score": 0,
                "duration_ms": 12_000,
                "reasons": ["warning: oracle: vacuous test — tests still pass"]
            })
            .to_string(),
            now - 100,
        )
        .for_task("t-churn"),
        FactoryEvent::new(
            EventKind::GateResult,
            json!({
                "status": "CLEAR",
                "mechanical_score": 100,
                "duration_ms": 20_000,
                "reasons": ["warning: oracle: vacuous test"]
            })
            .to_string(),
            now - 90,
        )
        .for_task("t-done"),
        FactoryEvent::new(
            EventKind::GateResult,
            json!({
                "status": "CLEAR",
                "mechanical_score": 100,
                "duration_ms": 30_000,
                "reasons": ["warning: oracle: vacuous test"]
            })
            .to_string(),
            now - 80,
        )
        .for_task("t-done"),
        // Short-circuited / reused CrossAudit events must NOT skew llm_avg
        FactoryEvent::new(
            EventKind::CrossAudit,
            json!({ "short_circuited": true, "llm_average": 10, "effective_score": 10 })
                .to_string(),
            now - 70,
        )
        .for_task("t-done"),
        FactoryEvent::new(
            EventKind::CrossAudit,
            json!({ "verdict": "PASS", "llm_average": 96, "effective_score": 96 }).to_string(),
            now - 65,
        )
        .for_task("t-done"),
    ];
    let report = empty_report(ReportWindow {
        since: Some(now - 86_400),
        now,
    });
    let tok_diag = json!({
        "summary": { "total_tokens": 550_000, "tokens_in": 500_000, "tokens_out": 50_000 }
    });
    let breakdown = vec![json!({
        "id": "t-done", "title": "Done", "state": "done",
        "tokens_total": 550_000, "wasted_tokens": 0, "runs_total": 1
    })];

    let out = collect_ops_diagnostics(&OpsInput {
        data_dir: None,
        tasks: &tasks,
        runs_by_task: &runs_by_task,
        events: &events,
        accounts: &[],
        blocked_upstream: &[],
        report: &report,
        token_diagnostics: &tok_diag,
        breakdown: &breakdown,
        since: Some(now - 86_400),
        now,
    });

    assert_eq!(out["quality"]["clear_with_mech_zero_count"], 1);
    assert_eq!(out["quality"]["vacuous_test_warns"], 3);
    assert_eq!(out["quality"]["llm_avg"], 96.0);
    assert_eq!(out["quality"]["audit_samples"], 1);
    assert_eq!(out["speed"]["done_1h"], 1);
    assert_eq!(out["speed"]["coding_dur_p50_secs"], 200);
    assert_eq!(out["efficiency"]["tokens_per_done"], 550_000);
    assert_eq!(out["efficiency"]["in_out_ratio"], 10.0);

    let codes: Vec<&str> = out["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["code"].as_str())
        .collect();
    assert!(codes.contains(&"clear_gate_with_mechanical_score_zero"));
    assert!(codes.contains(&"vacuous_test_warnings"));
    assert!(codes.contains(&"high_rework_churn"));
    assert!(codes.contains(&"high_tokens_per_done"));
    for a in out["alerts"].as_array().unwrap() {
        assert_eq!(
            a["active_recent"], true,
            "recent alert should be active: {a}"
        );
    }
}

#[test]
fn historical_events_stalled_workers_and_command_loops_are_detected_accurately() {
    let dir = tempfile::tempdir().unwrap();
    let streams = dir.path().join("streams");
    std::fs::create_dir_all(&streams).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let loop_lines = (0..6)
        .map(|i| {
            json!({"ts": now - 60 + i, "type": "tool_use", "tool": "Bash", "target": "aaf slot -- ./gradlew test"})
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(streams.join("run-idle.jsonl"), format!("{loop_lines}\n")).unwrap();
    // Chunked Read calls AND distinct Bash commands sharing a long `cd /worktree/...;`
    // prefix must NOT false-positive as a command loop.
    let prefix = "cd /Volumes/Data/.aaf-worktrees/3c15cbfbfb72a9f8/batch-51556-4796ef22/core/common/src/main";
    let read_chunks = (0..6)
        .map(|i| {
            json!({"ts": now - 30 + i, "type": "tool_use", "tool": "Read", "target": "/tmp/wt/tools/large.html"})
                .to_string()
        })
        .chain((0..6).map(|i| {
            json!({"ts": now - 20 + i, "type": "tool_use", "tool": "Bash", "target": format!("{prefix}; grep -n pattern_{i} File{i}.kt")})
                .to_string()
        }))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(
        streams.join("run-read-chunks.jsonl"),
        format!("{read_chunks}\n"),
    )
    .unwrap();

    let old_ts = now - 7_200;
    let events = vec![
        FactoryEvent::new(
            EventKind::GateResult,
            json!({
                "status": "CLEAR",
                "mechanical_score": 0,
                "duration_ms": 5_000,
                "reasons": ["warning: oracle: vacuous test"]
            })
            .to_string(),
            old_ts,
        )
        .for_task("t-old"),
        FactoryEvent::new(
            EventKind::GateResult,
            json!({ "status": "CLEAR", "mechanical_score": 100, "reasons": ["vacuous test"] })
                .to_string(),
            old_ts + 1,
        )
        .for_task("t-old"),
        FactoryEvent::new(
            EventKind::GateResult,
            json!({ "status": "CLEAR", "mechanical_score": 100, "reasons": ["vacuous test"] })
                .to_string(),
            old_ts + 2,
        )
        .for_task("t-old"),
    ];

    let mut runs_by_task: HashMap<String, Vec<RunRecord>> = HashMap::new();
    runs_by_task.insert(
        "t-stall".into(),
        vec![
            make_running_run("run-idle", "t-stall", "claude", None, now - 1200),
            make_running_run("run-missing-high", "t-stall", "claude", None, now - 1000),
            make_running_run("run-read-chunks", "t-stall", "claude", None, now - 60),
            make_running_run("../evil", "t-stall", "claude", None, now - 1000),
        ],
    );

    let report = empty_report(ReportWindow {
        since: Some(now - 86_400),
        now,
    });
    let out = collect_ops_diagnostics(&OpsInput {
        data_dir: Some(dir.path()),
        tasks: &[],
        runs_by_task: &runs_by_task,
        events: &events,
        accounts: &[],
        blocked_upstream: &[],
        report: &report,
        token_diagnostics: &json!({ "summary": {} }),
        breakdown: &[],
        since: Some(now - 86_400),
        now: now + 400,
    });

    let stalled = out["speed"]["stalled_workers"].as_array().unwrap();
    assert_eq!(stalled.len(), 3, "path traversal run id must be skipped");
    assert_eq!(stalled[0]["run_id"], "run-missing-high");
    assert_eq!(stalled[0]["severity"], "high");
    let idle_row = stalled
        .iter()
        .find(|w| w["run_id"] == "run-idle")
        .expect("run-idle stalled row");
    assert_eq!(idle_row["severity"], "medium");
    assert_eq!(idle_row["waiting_on_slot"], true);
    let loops = out["speed"]["looping_workers"].as_array().unwrap();
    assert_eq!(
        loops.len(),
        1,
        "chunked Read on large file must not false-positive as command loop"
    );
    assert_eq!(loops[0]["run_id"], "run-idle");

    let codes: Vec<&str> = out["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["code"].as_str())
        .collect();
    assert!(codes.contains(&"worker_command_loop"));
    for a in out["alerts"].as_array().unwrap() {
        match a["code"].as_str().unwrap() {
            "clear_gate_with_mechanical_score_zero" | "vacuous_test_warnings" => {
                assert_eq!(
                    a["active_recent"], false,
                    "old event must be historical: {a}"
                );
            }
            "stalled_worker" => {
                assert_eq!(a["severity"], "high");
                assert_eq!(a["active_recent"], true);
            }
            _ => {}
        }
    }
}

#[test]
fn drain_lock_without_waiters_never_false_positives_and_guest_orphan_is_detected() {
    let dir = tempfile::tempdir().unwrap();
    let throttle = dir.path().join("throttle");
    let guest = throttle.join("guest");
    std::fs::create_dir_all(&guest).unwrap();
    let drain_path = throttle.join("drain.lock");
    let drain_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&drain_path)
        .unwrap();
    drain_file.try_lock().unwrap();
    // Unlocked 0-byte heavy.0.lock paired with dead-PGID guest/heavy.0.rec -> dead orphan!
    std::fs::write(throttle.join("heavy.0.lock"), "").unwrap();
    std::fs::write(
        guest.join("heavy.0.rec"),
        "token=abc\nreserved=unknown\npgid=999999\n",
    )
    .unwrap();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let report = empty_report(ReportWindow {
        since: Some(now - 86_400),
        now,
    });
    let empty_runs: HashMap<String, Vec<RunRecord>> = HashMap::new();

    // Even when `now` is 900s ahead of drain.lock mtime, 0 waiters => NO deadlock false positive.
    let out = collect_ops_diagnostics(&OpsInput {
        data_dir: Some(dir.path()),
        tasks: &[],
        runs_by_task: &empty_runs,
        events: &[],
        accounts: &[],
        blocked_upstream: &[],
        report: &report,
        token_diagnostics: &json!({ "summary": {} }),
        breakdown: &[],
        since: Some(now - 86_400),
        now: now + 900,
    });
    let codes: Vec<&str> = out["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["code"].as_str())
        .collect();
    assert!(
        !codes.contains(&"heavy_slot_drain_deadlock"),
        "drain.lock with 0 waiters must not false-positive: {codes:?}"
    );
    assert!(
        codes.contains(&"dead_orphan_heavy_slots"),
        "dead guest/heavy.0.rec must trigger dead_orphan_heavy_slots: {codes:?}"
    );
}

#[test]
fn detects_account_exhaustion_stuck_gating_cause_dag_bottleneck_and_stale_review() {
    let now = 50_000;
    let tasks = vec![
        make_task("t-stuck-gate", TaskState::Gating, 0, now - 3600),
        make_task("t-stale-rev", TaskState::Review, 0, now - 20_000),
        make_task("t-blocker", TaskState::Blocked, 0, now - 10_000),
    ];
    let accounts = vec![
        make_account("acct-1", None),
        make_account("acct-2", Some(now + 3600)),
    ];
    let mut runs_by_task: HashMap<String, Vec<RunRecord>> = HashMap::new();
    runs_by_task.insert(
        "t-long".into(),
        vec![make_running_run(
            "run-long-1",
            "t-long",
            "claude",
            Some("acct-1"),
            now - 2_600,
        )],
    );
    let blocked_upstream = vec![
        ("t-r1".to_string(), "t-blocker".to_string()),
        ("t-r2".to_string(), "t-blocker".to_string()),
        ("t-r3".to_string(), "t-blocker".to_string()),
        ("t-r4".to_string(), "t-blocker".to_string()),
    ];
    let events = vec![
        FactoryEvent::new(
            EventKind::GateResult,
            json!({ "status": "CLEAR", "mechanical_score": 100 }).to_string(),
            now - 600,
        )
        .for_task("t-stuck-gate"),
        FactoryEvent::new(
            EventKind::GateResult,
            json!({ "status": "CLEAR", "mechanical_score": 100 }).to_string(),
            now - 300,
        )
        .for_task("t-stuck-gate"),
        FactoryEvent::new(
            EventKind::GateResult,
            json!({ "status": "CLEAR", "mechanical_score": 100 }).to_string(),
            now - 60,
        )
        .for_task("t-stuck-gate"),
    ];
    let report = empty_report(ReportWindow {
        since: Some(now - 86_400),
        now,
    });
    let out = collect_ops_diagnostics(&OpsInput {
        data_dir: None,
        tasks: &tasks,
        runs_by_task: &runs_by_task,
        events: &events,
        accounts: &accounts,
        blocked_upstream: &blocked_upstream,
        report: &report,
        token_diagnostics: &json!({ "summary": {} }),
        breakdown: &[],
        since: Some(now - 86_400),
        now,
    });
    assert_eq!(
        out["speed"]["stuck_gating_tasks"][0]["cause"],
        "all_accounts_leased"
    );
    assert_eq!(out["speed"]["account_pool"]["total_available"], 0);
    assert_eq!(out["speed"]["dag_bottlenecks"][0]["blocked_count"], 4);
    assert_eq!(
        out["speed"]["stale_review_tasks"].as_array().unwrap().len(),
        1
    );
    let codes: Vec<&str> = out["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["code"].as_str())
        .collect();
    assert!(codes.contains(&"account_pool_exhausted"));
    assert!(codes.contains(&"stuck_gating_tasks"));
    assert!(codes.contains(&"gating_spin_loop"));
    assert!(codes.contains(&"long_running_worker"));
    assert!(codes.contains(&"dag_upstream_bottleneck"));
    assert!(codes.contains(&"stale_review_tasks"));
}

#[test]
fn non_streaming_antigravity_uses_higher_stall_threshold_and_needs_human_dag_is_medium() {
    let dir = tempfile::tempdir().unwrap();
    let streams = dir.path().join("streams");
    std::fs::create_dir_all(&streams).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    std::fs::write(
        streams.join("run-agy.jsonl"),
        format!(
            "{}\n",
            json!({"ts": now, "type": "start", "tool": "antigravity"})
        ),
    )
    .unwrap();

    let mut blocker = make_task("t-human-blk", TaskState::Blocked, 0, now - 3600);
    blocker.blocked_reason = Some(aaf_domain::blocked::BlockedReason::NeedsHuman {
        why: "physical device needed".into(),
    });
    let tasks = vec![blocker];
    let mut runs_by_task: HashMap<String, Vec<RunRecord>> = HashMap::new();
    runs_by_task.insert(
        "t-agy".into(),
        vec![make_running_run("run-agy", "t-agy", "plan", None, now)],
    );
    let blocked_upstream = vec![
        ("t-r1".to_string(), "t-human-blk".to_string()),
        ("t-r2".to_string(), "t-human-blk".to_string()),
        ("t-r3".to_string(), "t-human-blk".to_string()),
        ("t-r4".to_string(), "t-human-blk".to_string()),
    ];
    let report = empty_report(ReportWindow {
        since: Some(now - 86_400),
        now,
    });
    // At +480s (8m): antigravity is NOT stalled yet (< 15m); needs_human DAG blocker is medium.
    let out_8m = collect_ops_diagnostics(&OpsInput {
        data_dir: Some(dir.path()),
        tasks: &tasks,
        runs_by_task: &runs_by_task,
        events: &[],
        accounts: &[],
        blocked_upstream: &blocked_upstream,
        report: &report,
        token_diagnostics: &json!({ "summary": {} }),
        breakdown: &[],
        since: Some(now - 86_400),
        now: now + 480,
    });
    assert!(
        out_8m["speed"]["stalled_workers"]
            .as_array()
            .unwrap()
            .is_empty(),
        "antigravity at 8m idle must not false-positive as stalled"
    );
    let dag_alert = out_8m["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["code"] == "dag_upstream_bottleneck")
        .expect("dag_upstream_bottleneck alert");
    assert_eq!(dag_alert["severity"], "medium");
    assert_eq!(
        out_8m["speed"]["dag_bottlenecks"][0]["blocker_reason"],
        "needs_human"
    );

    // At +960s (16m): antigravity exceeds 15m threshold and IS flagged as medium stalled.
    let out_16m = collect_ops_diagnostics(&OpsInput {
        data_dir: Some(dir.path()),
        tasks: &tasks,
        runs_by_task: &runs_by_task,
        events: &[],
        accounts: &[],
        blocked_upstream: &blocked_upstream,
        report: &report,
        token_diagnostics: &json!({ "summary": {} }),
        breakdown: &[],
        since: Some(now - 86_400),
        now: now + 960,
    });
    let stalled_16m = out_16m["speed"]["stalled_workers"].as_array().unwrap();
    assert_eq!(stalled_16m.len(), 1);
    assert_eq!(stalled_16m[0]["severity"], "medium");

    // Repeated Cross-Audit failures (>=3) on an open task with low unblock_cycles trigger high churn.
    let loop_task = make_task("t-loop", TaskState::Ready, 1, now - 600);
    let fail_evs: Vec<FactoryEvent> = (0..3)
        .map(|i| {
            FactoryEvent::new(
                EventKind::CrossAudit,
                json!({"verdict": "fail: [P1] G6", "dual_consensus": false}).to_string(),
                now - 100 + i,
            )
            .for_task("t-loop")
        })
        .collect();
    let out_loop = collect_ops_diagnostics(&OpsInput {
        data_dir: None, tasks: &[loop_task], runs_by_task: &HashMap::new(),
        events: &fail_evs, accounts: &[], blocked_upstream: &[],
        report: &report, token_diagnostics: &json!({ "summary": {} }),
        breakdown: &[], since: Some(now - 86_400), now,
    });
    let churn_a = out_loop["alerts"].as_array().unwrap().iter().find(|a| a["code"] == "high_rework_churn").expect("high_rework_churn alert");
    assert_eq!(churn_a["severity"], "high");
}
