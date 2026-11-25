#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `aaf report`: period report from the real binary against a real database.

#[path = "factory_support.rs"]
mod support;

use aaf_domain::ports::{RunStore, TaskStore};
use aaf_domain::run::{RunFinish, RunRecord, RunStatus};
use support::Fixture;

#[test]
fn report_without_a_database_is_empty_and_ok() {
    let fx = Fixture::new();
    let out = fx.aaf(&["report"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["command"], "report");
    assert_eq!(out.json["report"]["counts"]["total"], 0);
    assert_eq!(out.json["report"]["tasks_done"], 0);
}

#[test]
fn report_counts_tasks_stages_and_executor_cost() {
    let fx = Fixture::new();
    let id = fx.add("Report me");
    {
        let db = fx.db();
        let now = db.get_task(&id).unwrap().created_at;
        db.insert_run(&RunRecord {
            id: "r1".into(),
            task_id: id.clone(),
            account_id: None,
            executor: "claude".into(),
            worktree_path: fx.repo.to_string_lossy().into_owned(),
            branch: "aaf/t".into(),
            status: RunStatus::Running,
            exit_code: None,
            tokens_in: 0,
            tokens_out: 0,
            started_at: now,
            ended_at: None,
        })
        .unwrap();
        db.finish_run(
            "r1",
            &RunFinish {
                status: RunStatus::Succeeded,
                exit_code: Some(0),
                tokens_in: 1_000_000,
                tokens_out: 0,
                ended_at: now + 60,
            },
        )
        .unwrap();
    }
    let out = fx.aaf(&["report", "--since", "24"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let report = &out.json["report"];
    assert_eq!(report["counts"]["total"], 1);
    assert_eq!(report["executors"][0]["executor"], "claude");
    assert_eq!(report["executors"][0]["succeeded"], 1);
    assert_eq!(report["executors"][0]["wall_secs"], 60);
    // No account is billed per API token: tokens only, no USD (W4c).
    assert_eq!(report["total_tokens_in"], 1_000_000);
    assert!(report["estimated_cost_usd"].is_null());
    assert_eq!(report["cost_basis"], "none");
    assert!(report["window"]["since"].is_i64());
}
