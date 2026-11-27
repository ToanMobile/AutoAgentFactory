#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `aaf board` through the real binary, against a store in a temp dir.

#[path = "factory_support.rs"]
mod support;

use aaf_domain::blocked::BlockedReason;
use aaf_domain::budget::RootBudget;
use aaf_domain::config::UnblockerConfig;
use aaf_domain::ports::{
    BudgetStore, KvStore, META_CONSECUTIVE_ROOT_FAILS, META_FACTORY_FROZEN, TaskStore,
};
use aaf_domain::task::TaskState;
use serde_json::json;
use support::{Fixture, init_repo};

fn block(fx: &Fixture, id: &str) {
    let db = fx.db();
    let reason = BlockedReason::NeedsHuman {
        why: "stuck".to_string(),
    };
    db.set_state(id, TaskState::Blocked, Some(&reason), 5)
        .unwrap();
}

#[test]
fn add_creates_a_ready_task_that_list_and_show_report() {
    let fx = Fixture::new();
    let out = fx.aaf(&[
        "board",
        "add",
        "--title",
        "Fix the parser",
        "--spec",
        "Accept empty input.",
        "--accept",
        "empty input parses",
        "--accept",
        "old inputs still parse",
        "--priority",
        "p1",
    ]);

    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["ok"], json!(true));
    assert_eq!(out.json["command"], json!("board"));
    let task = &out.json["task"];
    let id = task["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("t-") && id.len() == 10, "{id}");
    assert_eq!(task["state"], json!("ready"));
    assert_eq!(task["priority"], json!("P1"));
    assert_eq!(task["repo_root"], json!(fx.repo.to_str().unwrap()));
    assert_eq!(task["root_id"], json!(id));
    assert_eq!(
        task["spec"],
        json!({
            "description": "Accept empty input.",
            "acceptance": ["empty input parses", "old inputs still parse"],
        })
    );

    let list = fx.aaf(&["board", "list"]);
    assert_eq!(list.json["count"], json!(1));
    assert_eq!(list.json["tasks"][0]["id"], json!(id));
    assert_eq!(list.json["frozen"], json!(false));

    let shown = fx.show(&id);
    assert_eq!(shown["task"]["title"], json!("Fix the parser"));
    assert_eq!(shown["deps"], json!([]));
    assert_eq!(shown["runs"], json!([]));
    assert_eq!(shown["events"][0]["kind"], json!("task_created"));
    assert_eq!(
        shown["events"][0]["payload"]["title"],
        json!("Fix the parser")
    );
}

#[test]
fn list_orders_by_priority_filters_by_state_and_stays_in_its_repository() {
    let fx = Fixture::new();
    let low = fx.add("low");
    let out = fx.aaf(&[
        "board",
        "add",
        "--title",
        "urgent",
        "--spec",
        "s",
        "--accept",
        "a",
        "--priority",
        "P0",
    ]);
    let urgent = out.json["task"]["id"].as_str().unwrap().to_string();
    block(&fx, &low);

    let all = fx.aaf(&["board", "list"]);
    let ids: Vec<&str> = all.json["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![urgent.as_str(), low.as_str()]);

    let blocked = fx.aaf(&["board", "list", "--state", "blocked"]);
    assert_eq!(blocked.json["count"], json!(1));
    assert_eq!(blocked.json["tasks"][0]["id"], json!(low));
    assert_eq!(
        blocked.json["tasks"][0]["blocked_reason"]["kind"],
        json!("needs_human")
    );

    let bad = fx.aaf(&["board", "list", "--state", "sleeping"]);
    assert_eq!(bad.code, 1);
    assert!(bad.json["error"].as_str().unwrap().contains("sleeping"));

    let other = init_repo(&fx.root(), "other");
    assert_eq!(
        fx.aaf_in(&other, &["board", "list"], &[]).json["count"],
        json!(0)
    );
    assert_eq!(
        fx.aaf_in(&other, &["board", "list", "--all"], &[]).json["count"],
        json!(2)
    );
}

#[test]
fn blocked_by_is_recorded_and_an_unknown_blocker_adds_nothing() {
    let fx = Fixture::new();
    let first = fx.add("first");
    let out = fx.aaf(&[
        "board",
        "add",
        "--title",
        "second",
        "--spec",
        "s",
        "--accept",
        "a",
        "--blocked-by",
        &first,
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let second = out.json["task"]["id"].as_str().unwrap().to_string();
    assert_eq!(
        fx.show(&second)["deps"],
        json!([{ "id": first, "state": "ready" }])
    );

    let bad = fx.aaf(&[
        "board",
        "add",
        "--title",
        "third",
        "--spec",
        "s",
        "--accept",
        "a",
        "--blocked-by",
        "t-missing",
    ]);
    assert_eq!(bad.code, 1);
    assert_eq!(bad.json["status"], json!("error"));
    assert_eq!(fx.aaf(&["board", "list"]).json["count"], json!(2));
}

#[test]
fn add_refuses_a_task_without_acceptance_criteria_or_with_empty_text() {
    let fx = Fixture::new();
    let no_accept = fx.aaf(&["board", "add", "--title", "t", "--spec", "s"]);
    assert_ne!(no_accept.code, 0);
    for args in [
        [
            "board", "add", "--title", " ", "--spec", "s", "--accept", "a",
        ],
        [
            "board", "add", "--title", "t", "--spec", "", "--accept", "a",
        ],
        [
            "board", "add", "--title", "t", "--spec", "s", "--accept", "  ",
        ],
    ] {
        let out = fx.aaf(&args);
        assert_eq!(out.code, 1, "{args:?}");
    }
    let bad_priority = fx.aaf(&[
        "board",
        "add",
        "--title",
        "t",
        "--spec",
        "s",
        "--accept",
        "a",
        "--priority",
        "P9",
    ]);
    assert_eq!(bad_priority.code, 1);
    assert_eq!(fx.aaf(&["board", "list"]).json["count"], json!(0));
}

#[test]
fn unblock_moves_a_blocked_task_to_ready_and_records_who_did_it() {
    let fx = Fixture::new();
    let id = fx.add("stuck one");
    block(&fx, &id);

    let out = fx.aaf(&["board", "unblock", &id]);

    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["state"], json!("ready"));
    assert_eq!(out.json["budget_reset"], json!(false));
    let shown = fx.show(&id);
    assert_eq!(shown["task"]["state"], json!("ready"));
    assert_eq!(shown["task"]["blocked_reason"], json!(null));
    let event = &shown["events"][0];
    assert_eq!(event["kind"], json!("task_state_changed"));
    assert_eq!(event["payload"]["by"], json!("human"));
    assert_eq!(event["payload"]["was"]["kind"], json!("needs_human"));

    // Not blocked any more: a second unblock is refused, not an error.
    let again = fx.aaf(&["board", "unblock", &id]);
    assert_eq!(again.code, 2);
    assert_eq!(again.json["status"], json!("refused"));
    assert_eq!(fx.aaf(&["board", "unblock", "t-missing"]).code, 1);
}

#[test]
fn unblock_gives_a_frozen_root_budget_a_fresh_start() {
    let fx = Fixture::new();
    let id = fx.add("over budget");
    block(&fx, &id);
    {
        let db = fx.db();
        let mut budget = RootBudget::new(&id, &UnblockerConfig::default());
        budget.tokens_used = budget.token_limit + 1;
        budget.frozen = true;
        db.put_budget(&budget).unwrap();
    }

    let out = fx.aaf(&["board", "unblock", &id]);

    assert_eq!(out.json["budget_reset"], json!(true), "{}", out.stderr);
    let budget = &fx.show(&id)["budget"];
    assert_eq!(budget["frozen"], json!(false));
    assert_eq!(budget["tokens_used"], json!(0));
    assert_eq!(
        budget["token_limit"],
        json!(UnblockerConfig::default().max_token_budget)
    );
}

#[test]
fn an_agent_run_can_neither_unblock_a_task_nor_unfreeze_the_factory() {
    let fx = Fixture::new();
    let id = fx.add("stuck one");
    block(&fx, &id);
    fx.db().set_meta(META_FACTORY_FROZEN, "1").unwrap();
    let inside = [("AAF_RUN_ID", "run-123")];

    let unblock = fx.aaf_in(&fx.repo, &["board", "unblock", &id], &inside);
    let unfreeze = fx.aaf_in(&fx.repo, &["board", "unfreeze"], &inside);

    for out in [&unblock, &unfreeze] {
        assert_eq!(out.code, 2, "{}", out.stderr);
        assert_eq!(out.json["status"], json!("refused"));
    }
    assert_eq!(fx.state(&id), "blocked");
    assert_eq!(fx.aaf(&["board", "list"]).json["frozen"], json!(true));
    // Reading the board stays allowed.
    assert_eq!(
        fx.aaf_in(&fx.repo, &["board", "show", &id], &inside).code,
        0
    );
}

#[test]
fn unfreeze_clears_the_flag_and_the_failure_counter() {
    let fx = Fixture::new();
    {
        let db = fx.db();
        db.set_meta(META_FACTORY_FROZEN, "1").unwrap();
        db.set_meta(META_CONSECUTIVE_ROOT_FAILS, "3").unwrap();
    }
    assert_eq!(fx.aaf(&["board", "list"]).json["frozen"], json!(true));

    let out = fx.aaf(&["board", "unfreeze"]);

    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["was_frozen"], json!(true));
    assert_eq!(out.json["frozen"], json!(false));
    let db = fx.db();
    assert_eq!(
        db.get_meta(META_FACTORY_FROZEN).unwrap().as_deref(),
        Some("0")
    );
    assert_eq!(
        db.get_meta(META_CONSECUTIVE_ROOT_FAILS).unwrap().as_deref(),
        Some("0")
    );
}

#[test]
fn unblock_on_split_root_scales_both_token_and_wall_limits_from_config() {
    let fx = Fixture::new();
    std::fs::create_dir_all(fx.repo.join(".autofactory")).unwrap();
    std::fs::write(
        fx.repo.join(".autofactory/config.toml"),
        "[unblocker]\nmax_token_budget = 2000000\nmax_wall_clock_secs = 10800\nmax_unblock_cycles = 5\n",
    )
    .unwrap();
    let id = fx.add("split root");
    {
        let db = fx.db();
        let root = db.get_task(&id).unwrap();
        for i in 1..=3 {
            let child = aaf_domain::task::Task::new_subtask(
                format!("{id}-s{i}"),
                &root,
                format!("sub {i}"),
                root.spec.clone(),
                2_000,
            )
            .unwrap();
            db.insert_task(&child).unwrap();
        }
        let mut budget = RootBudget::new(&id, &UnblockerConfig::default());
        budget.wall_secs_limit = 3_600;
        budget.wall_secs_used = 4_000;
        budget.frozen = true;
        db.put_budget(&budget).unwrap();
    }
    block(&fx, &id);

    let out = fx.aaf(&["board", "unblock", &id]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["budget_reset"], json!(true));
    let budget = &fx.show(&id)["budget"];
    assert_eq!(budget["frozen"], json!(false));
    assert_eq!(budget["wall_secs_used"], json!(0));
    assert_eq!(budget["token_limit"], json!(4_000_000));
    assert_eq!(budget["wall_secs_limit"], json!(21_600));
}
