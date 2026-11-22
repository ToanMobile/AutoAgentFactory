#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Pillar 1 CLI: `aaf board rework` transitions Review -> Gating (or Ready with --rerun).

#[path = "factory_support.rs"]
mod support;

use aaf_domain::budget::{BudgetCheck, RootBudget};
use aaf_domain::config::UnblockerConfig;
use aaf_domain::ports::{BudgetStore, TaskStore};
use aaf_domain::task::TaskState;
use serde_json::json;
use support::Fixture;

fn put_in_review(fx: &Fixture, id: &str) {
    let db = fx.db();
    db.set_state(id, TaskState::Running, None, 2).unwrap();
    db.set_state(id, TaskState::Gating, None, 3).unwrap();
    db.set_state(id, TaskState::Review, None, 4).unwrap();
}

#[test]
fn board_rework_moves_review_to_gating_appends_note_and_emits_event() {
    let fx = Fixture::new();
    let id = fx.add("needs another pass");

    // Ready task cannot be reworked.
    let refused = fx.aaf(&["board", "rework", &id, "--reason", "not ready yet"]);
    assert_eq!(refused.code, 2, "{}", refused.stderr);
    assert_eq!(refused.json["status"], json!("refused"));

    put_in_review(&fx, &id);
    let out = fx.aaf(&["board", "rework", &id, "--reason", "clippy warning in auth"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["ok"], json!(true));
    assert_eq!(out.json["state"], json!("gating"));

    let shown = fx.show(&id);
    assert_eq!(shown["task"]["state"], json!("gating"));
    let notes = shown["task"]["context_notes"].as_array().unwrap();
    assert!(
        notes
            .iter()
            .any(|n| n.as_str().unwrap_or("").contains("clippy warning in auth")),
        "{notes:?}"
    );
    let events = shown["events"].as_array().unwrap();
    assert!(
        events
            .iter()
            .any(|e| e["kind"] == json!("rework_requested")),
        "{events:?}"
    );
}

#[test]
fn board_rework_with_rerun_moves_review_to_ready_and_refuses_inside_agent_run() {
    let fx = Fixture::new();
    let id = fx.add("agent rework");
    put_in_review(&fx, &id);

    let inside = fx.aaf_in(
        &fx.repo,
        &["board", "rework", &id, "--reason", "sneaky"],
        &[("AAF_RUN_ID", "run-123")],
    );
    assert_eq!(inside.code, 2, "{}", inside.stderr);
    assert_eq!(inside.json["status"], json!("refused"));
    assert_eq!(fx.state(&id), "review");

    let out = fx.aaf(&[
        "board",
        "rework",
        &id,
        "--reason",
        "rewrite parser cleanly",
        "--rerun",
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["state"], json!("ready"));
    assert_eq!(fx.state(&id), "ready");
}

#[test]
fn board_rework_rejects_empty_reason_and_unknown_task_id() {
    let fx = Fixture::new();
    let id = fx.add("validation check");
    put_in_review(&fx, &id);

    let empty = fx.aaf(&["board", "rework", &id, "--reason", "   "]);
    assert_ne!(empty.code, 0, "empty reason must fail");
    assert_eq!(fx.state(&id), "review");

    let unknown = fx.aaf(&["board", "rework", "t-doesnotexist", "--reason", "retry"]);
    assert_ne!(unknown.code, 0, "unknown task id must fail");
}

#[test]
fn board_rework_supports_note_alias_resets_exhausted_budget_and_clears_stale_receipt() {
    let fx = Fixture::new();
    let id = fx.add("budget and receipt rework");
    put_in_review(&fx, &id);

    // Exhaust and freeze the root budget before rework.
    let db = fx.db();
    let mut budget = RootBudget::new(&id, &UnblockerConfig::default());
    budget.tokens_used = budget.token_limit + 100;
    budget.cycles_used = budget.cycles_limit + 1;
    budget.frozen = true;
    db.put_budget(&budget).unwrap();
    assert_ne!(db.get_budget(&id).unwrap().check(), BudgetCheck::Ok);

    // Plant a stale receipt and a stale head-lock approval for the task worktree.
    let stale_receipt = aaf_fp::receipt_path(&fx.data, &fx.repo);
    std::fs::create_dir_all(stale_receipt.parent().unwrap()).unwrap();
    std::fs::write(&stale_receipt, "{}").unwrap();
    assert!(stale_receipt.is_file());

    let wt = aaf_engine::git::worktree::worktree_path(&fx.repo, &fx.worktrees, &id).unwrap();
    let stale_approval = aaf_fp::approval_path(&fx.data, &wt, "lockfp999");
    std::fs::create_dir_all(stale_approval.parent().unwrap()).unwrap();
    std::fs::write(&stale_approval, "{}").unwrap();
    assert!(stale_approval.is_file());

    let out = fx.aaf(&["board", "rework", &id, "--note", "fix review feedback"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["state"], json!("gating"));
    assert_eq!(out.json["budget_reset"], json!(true));

    let refreshed = db.get_budget(&id).unwrap();
    assert_eq!(refreshed.check(), BudgetCheck::Ok);
    assert_eq!(refreshed.tokens_used, 0);
    assert_eq!(refreshed.cycles_used, 0);
    assert!(!refreshed.frozen);
    assert!(
        !stale_receipt.exists(),
        "stale receipt must be removed on rework"
    );
    assert!(
        !stale_approval.exists(),
        "stale head-lock approval must be removed on rework"
    );
}
