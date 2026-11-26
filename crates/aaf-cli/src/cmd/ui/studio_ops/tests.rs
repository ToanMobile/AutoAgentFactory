#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;

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
        report: &report,
        token_diagnostics: &tok_diag,
        breakdown: &breakdown,
        since: Some(now - 86_400),
        now,
    });

    assert_eq!(out["quality"]["clear_with_mech_zero_count"], 1);
    assert_eq!(out["quality"]["vacuous_test_warns"], 3);
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
fn historical_events_are_marked_inactive_and_stalled_workers_escalate_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let streams = dir.path().join("streams");
    std::fs::create_dir_all(&streams).unwrap();
    // Create one fresh stream file and rely on synthetic `now` far ahead of mtime
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    std::fs::write(streams.join("run-idle.jsonl"), "{}\n").unwrap();

    // Events 2 hours old (> 1h RECENT_WINDOW_SECS) -> active_recent=false
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
            RunRecord {
                id: "run-idle".into(),
                task_id: "t-stall".into(),
                account_id: None,
                executor: "claude".into(),
                worktree_path: "/tmp/wt".into(),
                branch: "aaf/t-stall".into(),
                status: RunStatus::Running,
                exit_code: None,
                tokens_in: 0,
                tokens_out: 0,
                started_at: now - 1200,
                ended_at: None,
            },
            RunRecord {
                id: "run-missing-high".into(),
                task_id: "t-stall".into(),
                account_id: None,
                executor: "claude".into(),
                worktree_path: "/tmp/wt".into(),
                branch: "aaf/t-stall".into(),
                status: RunStatus::Running,
                exit_code: None,
                tokens_in: 0,
                tokens_out: 0,
                started_at: now - 1000,
                ended_at: None,
            },
            RunRecord {
                id: "../evil".into(),
                task_id: "t-stall".into(),
                account_id: None,
                executor: "claude".into(),
                worktree_path: "/tmp/wt".into(),
                branch: "aaf/t-stall".into(),
                status: RunStatus::Running,
                exit_code: None,
                tokens_in: 0,
                tokens_out: 0,
                started_at: now - 1000,
                ended_at: None,
            },
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
        report: &report,
        token_diagnostics: &json!({ "summary": {} }),
        breakdown: &[],
        since: Some(now - 86_400),
        now: now + 600, // run-idle stream is 600s old (medium); run-missing-high is 1600s old (high)
    });

    let stalled = out["speed"]["stalled_workers"].as_array().unwrap();
    assert_eq!(stalled.len(), 2, "path traversal run id must be skipped");
    assert_eq!(stalled[0]["run_id"], "run-missing-high");
    assert_eq!(stalled[0]["severity"], "high");
    assert_eq!(stalled[1]["run_id"], "run-idle");
    assert_eq!(stalled[1]["severity"], "medium");

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
fn transient_drain_lock_does_not_false_positive_but_aged_drain_does() {
    let dir = tempfile::tempdir().unwrap();
    let throttle = dir.path().join("throttle");
    std::fs::create_dir_all(&throttle).unwrap();
    let drain_path = throttle.join("drain.lock");
    let drain_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&drain_path)
        .unwrap();
    // Hold flock on drain.lock so scan_throttle reports drain_lock_present = true
    drain_file.try_lock().unwrap();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let report = empty_report(ReportWindow {
        since: Some(now - 86_400),
        now,
    });
    let empty_runs: HashMap<String, Vec<RunRecord>> = HashMap::new();

    // Fresh drain (age ~0s, 0 waiters) -> NO deadlock alert
    let fresh = collect_ops_diagnostics(&OpsInput {
        data_dir: Some(dir.path()),
        tasks: &[],
        runs_by_task: &empty_runs,
        events: &[],
        report: &report,
        token_diagnostics: &json!({ "summary": {} }),
        breakdown: &[],
        since: Some(now - 86_400),
        now,
    });
    let fresh_codes: Vec<&str> = fresh["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["code"].as_str())
        .collect();
    assert!(
        !fresh_codes.contains(&"heavy_slot_drain_deadlock"),
        "transient drain must not false-positive: {fresh_codes:?}"
    );

    // Aged drain (+90s, 0 active slots) -> DOES alert heavy_slot_drain_deadlock
    let aged = collect_ops_diagnostics(&OpsInput {
        data_dir: Some(dir.path()),
        tasks: &[],
        runs_by_task: &empty_runs,
        events: &[],
        report: &report,
        token_diagnostics: &json!({ "summary": {} }),
        breakdown: &[],
        since: Some(now - 86_400),
        now: now + 90,
    });
    let aged_codes: Vec<&str> = aged["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["code"].as_str())
        .collect();
    assert!(
        aged_codes.contains(&"heavy_slot_drain_deadlock"),
        "aged drain with 0 active slots must alert: {aged_codes:?}"
    );
}
