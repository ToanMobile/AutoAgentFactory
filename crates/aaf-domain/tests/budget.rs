#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Root budget boundaries and the subtask depth rule.

use aaf_domain::budget::{BudgetCheck, MAX_SUBTASK_DEPTH, RootBudget, can_spawn_subtask};
use aaf_domain::config::UnblockerConfig;

fn budget() -> RootBudget {
    RootBudget {
        root_id: "root".to_string(),
        tokens_used: 0,
        token_limit: 1000,
        wall_secs_used: 0,
        wall_secs_limit: 60,
        cycles_used: 0,
        cycles_limit: 5,
        frozen: false,
    }
}

#[test]
fn new_takes_its_limits_from_the_unblocker_config() {
    let cfg = UnblockerConfig {
        max_unblock_cycles: 7,
        max_token_budget: 123,
        max_wall_clock_secs: 456,
        ..UnblockerConfig::default()
    };
    let b = RootBudget::new("r1", &cfg);
    assert_eq!(b.root_id, "r1");
    assert_eq!(
        (b.token_limit, b.wall_secs_limit, b.cycles_limit),
        (123, 456, 7)
    );
    assert_eq!((b.tokens_used, b.wall_secs_used, b.cycles_used), (0, 0, 0));
    assert!(!b.frozen);
    assert_eq!(b.check(), BudgetCheck::Ok);
}

#[test]
fn token_limit_n_allows_exactly_n() {
    let mut b = budget();
    b.tokens_used = 999;
    assert_eq!(b.check(), BudgetCheck::Ok);
    b.tokens_used = 1000;
    assert_eq!(b.check(), BudgetCheck::Ok);
    b.tokens_used = 1001;
    assert_eq!(b.check(), BudgetCheck::TokensExceeded);
}

#[test]
fn wall_clock_limit_n_allows_exactly_n() {
    let mut b = budget();
    b.wall_secs_used = 60;
    assert_eq!(b.check(), BudgetCheck::Ok);
    b.wall_secs_used = 61;
    assert_eq!(b.check(), BudgetCheck::WallClockExceeded);
}

#[test]
fn cycle_limit_n_allows_exactly_n() {
    let mut b = budget();
    b.cycles_used = 5;
    assert_eq!(b.check(), BudgetCheck::Ok);
    b.cycles_used = 6;
    assert_eq!(b.check(), BudgetCheck::CyclesExceeded);
}

#[test]
fn zero_limits_allow_zero_usage_only() {
    let mut b = budget();
    b.token_limit = 0;
    b.wall_secs_limit = 0;
    b.cycles_limit = 0;
    assert_eq!(b.check(), BudgetCheck::Ok);
    b.cycles_used = 1;
    assert_eq!(b.check(), BudgetCheck::CyclesExceeded);
}

#[test]
fn precedence_is_frozen_then_tokens_then_wall_clock_then_cycles() {
    let mut b = budget();
    b.tokens_used = 1001;
    b.wall_secs_used = 61;
    b.cycles_used = 6;
    b.frozen = true;
    assert_eq!(b.check(), BudgetCheck::Frozen);
    b.frozen = false;
    assert_eq!(b.check(), BudgetCheck::TokensExceeded);
    b.tokens_used = 1000;
    assert_eq!(b.check(), BudgetCheck::WallClockExceeded);
    b.wall_secs_used = 60;
    assert_eq!(b.check(), BudgetCheck::CyclesExceeded);
    b.cycles_used = 5;
    assert_eq!(b.check(), BudgetCheck::Ok);
}

#[test]
fn frozen_wins_even_with_no_usage() {
    let mut b = budget();
    b.frozen = true;
    assert_eq!(b.check(), BudgetCheck::Frozen);
}

#[test]
fn add_usage_accumulates_and_saturates() {
    let mut b = budget();
    b.add_usage(400, 10, 1);
    b.add_usage(600, 50, 4);
    assert_eq!(
        (b.tokens_used, b.wall_secs_used, b.cycles_used),
        (1000, 60, 5)
    );
    assert_eq!(b.check(), BudgetCheck::Ok);
    b.add_usage(1, 0, 0);
    assert_eq!(b.check(), BudgetCheck::TokensExceeded);

    b.add_usage(u64::MAX, u64::MAX, u32::MAX);
    assert_eq!(
        (b.tokens_used, b.wall_secs_used, b.cycles_used),
        (u64::MAX, u64::MAX, u32::MAX)
    );
}

#[test]
fn only_a_root_task_may_spawn_and_config_cannot_raise_the_cap() {
    assert_eq!(MAX_SUBTASK_DEPTH, 1);
    assert!(can_spawn_subtask(0, 1));
    assert!(!can_spawn_subtask(1, 1));
    // Config may lower the depth to zero (no subtasks at all)...
    assert!(!can_spawn_subtask(0, 0));
    // ...but never raise it past the hard cap.
    assert!(can_spawn_subtask(0, u8::MAX));
    assert!(!can_spawn_subtask(1, u8::MAX));
    assert!(!can_spawn_subtask(2, u8::MAX));
    assert!(!can_spawn_subtask(u8::MAX, u8::MAX));
}
