#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;

use aaf_domain::budget::RootBudget;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::pipeline::CostGuardConfig;
use aaf_domain::ports::{
    BudgetStore, EventSink, KvStore, META_COST_PAUSED_UNTIL, RunStore, TaskStore,
};
use aaf_domain::run::{RunRecord, RunStatus};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use serde_json::json;
use tempfile::tempdir;

use super::{DiagnosticsCtx, collect_token_diagnostics};

fn mk_task(id: &str, now: i64) -> Task {
    Task::new_root(
        id,
        "/repo/demo",
        format!("Task {id}"),
        ImmutableSpec {
            description: "desc".into(),
            acceptance: vec!["ac".into()],
        },
        Priority::P1,
        now,
    )
}

fn mk_run(
    id: &str,
    task_id: &str,
    exec: &str,
    status: RunStatus,
    tin: u64,
    tout: u64,
    started: i64,
) -> RunRecord {
    RunRecord {
        id: id.into(),
        task_id: task_id.into(),
        account_id: Some("acct-1".into()),
        executor: exec.into(),
        worktree_path: "/tmp/wt".into(),
        branch: format!("aaf/{task_id}"),
        status,
        exit_code: Some(if status == RunStatus::Succeeded { 0 } else { 1 }),
        tokens_in: tin,
        tokens_out: tout,
        started_at: started,
        ended_at: (status != RunStatus::Running).then_some(started + 60),
    }
}

fn group_runs(runs: Vec<RunRecord>, since: Option<i64>) -> HashMap<String, Vec<RunRecord>> {
    let mut map: HashMap<String, Vec<RunRecord>> = HashMap::new();
    for r in runs {
        if since.is_none_or(|c| r.started_at >= c) {
            map.entry(r.task_id.clone()).or_default().push(r);
        }
    }
    map
}

#[test]
fn rejected_audit_is_not_wasted_and_window_and_alerts_are_accurate() {
    let dir = tempdir().unwrap();
    let db = Db::open(&dir.path().join("aaf.db")).unwrap();
    let now = 2_000_000;
    let t1 = mk_task("t-1", now - 1000);
    db.insert_task(&t1).unwrap();
    let all_runs = vec![
        // Outside 24h window -> excluded
        mk_run(
            "r-old",
            "t-1",
            "claude",
            RunStatus::Succeeded,
            900_000,
            100_000,
            now - 200_000,
        ),
        mk_run(
            "r-ok",
            "t-1",
            "claude",
            RunStatus::Succeeded,
            90_000,
            10_000,
            now - 600,
        ),
        mk_run(
            "r-to",
            "t-1",
            "claude",
            RunStatus::TimedOut,
            150_000,
            10_000,
            now - 500,
        ),
        mk_run(
            "r-au1",
            "t-1",
            "cross-audit",
            RunStatus::Succeeded,
            80_000,
            10_000,
            now - 400,
        ),
        // P1-1: cross-audit returning FAIL must NOT count as wasted!
        mk_run(
            "r-au2",
            "t-1",
            "cross-audit",
            RunStatus::Failed,
            80_000,
            10_000,
            now - 300,
        ),
    ];
    for r in &all_runs {
        db.insert_run(r).unwrap();
    }
    for ts in [now - 350, now - 250] {
        let ev = FactoryEvent::new(
            EventKind::CrossAudit,
            json!({
                "phase": "post_gate",
                "verdict": "fail: score 85 < 95",
                "tree_fp": "git-tree:abc123",
                "config_hash": "cfg-1",
                "retry": false
            })
            .to_string(),
            ts,
        )
        .for_task("t-1");
        db.emit(&ev).unwrap();
    }
    db.put_budget(&RootBudget {
        root_id: "t-1".into(),
        tokens_used: 850_000,
        token_limit: 1_000_000,
        wall_secs_used: 1200,
        wall_secs_limit: 3600,
        cycles_used: 1,
        cycles_limit: 5,
        frozen: false,
    })
    .unwrap();

    let runs_map = group_runs(all_runs, Some(now - 86_400));
    let (diag, extras) = collect_token_diagnostics(
        &db,
        &[t1],
        &runs_map,
        Some(now - 86_400),
        now,
        DiagnosticsCtx {
            data_dir: Some(dir.path()),
            cost_guard: CostGuardConfig {
                daily_token_cap: 500_000,
                spike_factor: 2,
            },
            run_timeout_secs: 1800,
            config_error: None,
        },
    );
    assert_eq!(diag["summary"]["total_tokens"], 440_000);
    // P1-1: only r-to (160k) is wasted; r-au2 (90k) is audit_rejected_tokens
    assert_eq!(diag["summary"]["wasted_tokens"], 160_000);
    assert_eq!(diag["summary"]["audit_rejected_tokens"], 90_000);
    assert_eq!(diag["summary"]["audit_rejected_runs"], 1);
    assert_eq!(diag["summary"]["timed_out_tokens"], 160_000);
    assert_eq!(diag["summary"]["audit_tokens"], 180_000);
    assert_eq!(diag["summary"]["duplicate_audit_groups"], 1);

    let codes: Vec<&str> = diag["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["code"].as_str())
        .collect();
    for expected in [
        "cost_guard_24h_near",
        "cost_guard_1h_spike",
        "high_wasted_tokens",
        "high_audit_overhead",
        "duplicate_post_gate_audit",
        "root_budget_near_limit",
        "single_run_spike",
    ] {
        assert!(codes.contains(&expected), "missing {expected} in {codes:?}");
    }
    // P3-4: timeout_token_burn suppressed when timeouts are 100% of already-alerted waste
    assert!(!codes.contains(&"timeout_token_burn"));

    let te = extras.get("t-1").unwrap();
    assert_eq!(te.wasted_tokens, 160_000);
    assert_eq!(te.audit_rejected_tokens, 90_000);
    assert_eq!(te.duplicate_audits, 1);
}

#[test]
fn stale_cost_pause_inconclusive_retry_and_stale_running_are_handled_accurately() {
    let dir = tempdir().unwrap();
    let db = Db::open(&dir.path().join("aaf.db")).unwrap();
    let now = 2_000_000;
    let mut t_done = mk_task("t-done", now - 5000);
    t_done.state = TaskState::Done;
    let t_open = mk_task("t-open", now - 5000);
    db.insert_task(&t_done).unwrap();
    db.insert_task(&t_open).unwrap();

    // P1-3: past META_COST_PAUSED_UNTIL must NOT trigger cost_paused alert
    db.set_meta(META_COST_PAUSED_UNTIL, &(now - 300).to_string())
        .unwrap();

    // P2-1: automatic inconclusive retry (`retry: true`) followed by `pass` must NOT flag duplicate
    for (verdict, retry, ts) in [
        ("inconclusive: timeout", true, now - 400),
        ("pass", false, now - 300),
    ] {
        let ev = FactoryEvent::new(
            EventKind::CrossAudit,
            json!({
                "phase": "post_gate",
                "verdict": verdict,
                "tree_fp": "git-tree:same",
                "config_hash": "cfg-1",
                "retry": retry
            })
            .to_string(),
            ts,
        )
        .for_task("t-open");
        db.emit(&ev).unwrap();
    }

    // P2-2: run stuck in `running` for 2 hours triggers `stale_running_runs`
    let runs = vec![
        mk_run(
            "r-au",
            "t-open",
            "cross-audit",
            RunStatus::Succeeded,
            10_000,
            1_000,
            now - 300,
        ),
        mk_run(
            "r-stuck",
            "t-open",
            "claude",
            RunStatus::Running,
            0,
            0,
            now - 7200,
        ),
    ];
    for r in &runs {
        db.insert_run(r).unwrap();
    }
    // P3-5: done root budget near 100% must NOT raise root_budget_near_limit alert
    db.put_budget(&RootBudget {
        root_id: "t-done".into(),
        tokens_used: 980_000,
        token_limit: 1_000_000,
        wall_secs_used: 100,
        wall_secs_limit: 3600,
        cycles_used: 1,
        cycles_limit: 5,
        frozen: false,
    })
    .unwrap();

    let runs_map = group_runs(runs, None);
    let (diag, _) = collect_token_diagnostics(
        &db,
        &[t_done, t_open],
        &runs_map,
        None,
        now,
        DiagnosticsCtx {
            data_dir: Some(dir.path()),
            cost_guard: CostGuardConfig {
                daily_token_cap: 1_000_000,
                spike_factor: 3,
            },
            run_timeout_secs: 1800,
            config_error: None,
        },
    );
    assert!(diag["burn_rate"]["cost_paused_until"].is_null());
    assert_eq!(diag["burn_rate"]["cost_paused_stale"], true);
    assert_eq!(diag["summary"]["duplicate_audit_groups"], 0);
    assert_eq!(diag["summary"]["stale_running_runs"], 1);
    let codes: Vec<&str> = diag["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["code"].as_str())
        .collect();
    assert!(
        !codes.contains(&"cost_paused"),
        "stale pause fired: {codes:?}"
    );
    assert!(
        !codes.contains(&"duplicate_post_gate_audit"),
        "retry flagged: {codes:?}"
    );
    assert!(
        !codes.contains(&"root_budget_near_limit"),
        "done root flagged: {codes:?}"
    );
    assert!(
        codes.contains(&"stale_running_runs"),
        "missing stale_running_runs: {codes:?}"
    );
}

#[test]
fn detects_zero_token_failure_rate_without_false_positive_when_healthy() {
    let dir = tempdir().unwrap();
    let db = Db::open(&dir.path().join("aaf.db")).unwrap();
    let now = 2_000_000;
    let t1 = mk_task("t-z", now - 500);
    db.insert_task(&t1).unwrap();
    let mut runs = Vec::new();
    for idx in 0..6 {
        runs.push(mk_run(
            &format!("rk-{idx}"),
            "t-z",
            "claude",
            RunStatus::Killed,
            0,
            0,
            now - 100 - idx,
        ));
    }
    for idx in 0..4 {
        runs.push(mk_run(
            &format!("ro-{idx}"),
            "t-z",
            "claude",
            RunStatus::Succeeded,
            10_000,
            2_000,
            now - 50 - idx,
        ));
    }
    for r in &runs {
        db.insert_run(r).unwrap();
    }
    let runs_map = group_runs(runs, None);
    let (diag, _) =
        collect_token_diagnostics(&db, &[t1], &runs_map, None, now, DiagnosticsCtx::default());
    let codes: Vec<&str> = diag["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["code"].as_str())
        .collect();
    assert!(codes.contains(&"zero_token_failures"), "got {codes:?}");
    assert!(!codes.contains(&"high_wasted_tokens"));
}
