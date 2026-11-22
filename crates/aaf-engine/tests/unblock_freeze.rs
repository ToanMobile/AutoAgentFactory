#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "unblock_support.rs"]
mod support;

use aaf_domain::blocked::{BlockedReason, UnblockAction, UnblockTier};
use aaf_domain::budget::{BudgetCheck, RootBudget};
use aaf_domain::config::UnblockerConfig;
use aaf_domain::event::EventKind;
use aaf_domain::ports::{
    BudgetStore, KvStore, META_CONSECUTIVE_ROOT_FAILS, META_FACTORY_FROZEN, PortError,
};
use aaf_engine::unblocker::FailureSignal;
use support::{CHILD, FakeProposer, ROOT, World, plan, stuck_signal};

fn is_freeze(action: &UnblockAction) -> bool {
    matches!(action, UnblockAction::FreezeRoot { .. })
}

fn step(world: &World, id: &str) -> UnblockAction {
    world
        .ladder()
        .next_action(id, &stuck_signal("e"), None)
        .unwrap()
        .action
}

#[test]
fn cycle_limit_n_allows_exactly_n_steps() {
    let world = World::new(UnblockerConfig {
        max_unblock_cycles: 3,
        ..UnblockerConfig::default()
    });
    // Root and child share the budget, so their cycles add up.
    for id in [ROOT, CHILD, ROOT] {
        assert!(!is_freeze(&step(&world, id)));
    }
    assert!(!world.budgets.get_budget(ROOT).unwrap().frozen);

    let decision = world
        .ladder()
        .next_action(CHILD, &stuck_signal("e"), None)
        .unwrap();
    assert!(is_freeze(&decision.action));
    assert_eq!(decision.tier, UnblockTier::T5Escalate);
    assert!(matches!(
        decision.reason,
        BlockedReason::BudgetExceeded { .. }
    ));
    assert!(world.budgets.get_budget(ROOT).unwrap().frozen);
    assert_eq!(world.events.count(EventKind::BudgetFrozen), 1);
    assert_eq!(
        world
            .events
            .last(EventKind::BudgetFrozen)
            .task_id
            .as_deref(),
        Some(ROOT)
    );
    assert_eq!(world.events.count(EventKind::UnblockStep), 4);
}

#[test]
fn token_limit_n_allows_exactly_n_tokens() {
    let cfg = UnblockerConfig {
        max_token_budget: 1_000,
        ..UnblockerConfig::default()
    };
    let at_limit = World::new(cfg.clone());
    at_limit.budgets.add_usage(ROOT, 1_000, 0, 0).unwrap();
    assert!(!is_freeze(&step(&at_limit, ROOT)));

    let over = World::new(cfg);
    over.budgets.add_usage(ROOT, 1_001, 0, 0).unwrap();
    let action = step(&over, ROOT);
    let UnblockAction::FreezeRoot { why } = action else {
        panic!("expected freeze, got {action:?}");
    };
    assert!(why.contains("token"), "{why}");
    assert!(why.contains("1001/1000"), "{why}");
    assert!(over.budgets.get_budget(ROOT).unwrap().frozen);
}

#[test]
fn wall_clock_limit_n_allows_exactly_n_seconds() {
    let cfg = UnblockerConfig {
        max_wall_clock_secs: 60,
        ..UnblockerConfig::default()
    };
    let at_limit = World::new(cfg.clone());
    at_limit.budgets.add_usage(ROOT, 0, 60, 0).unwrap();
    assert!(!is_freeze(&step(&at_limit, ROOT)));

    let over = World::new(cfg);
    over.budgets.add_usage(ROOT, 0, 61, 0).unwrap();
    assert!(is_freeze(&step(&over, ROOT)));
}

#[test]
fn frozen_root_short_circuits_every_tier() {
    let world = World::new(UnblockerConfig::default());
    world.budgets.set_frozen(ROOT, true).unwrap();
    let proposer = FakeProposer::new(Ok(vec![plan("a")]));
    for id in [ROOT, CHILD, ROOT] {
        let decision = world
            .ladder()
            .next_action(id, &stuck_signal("e"), Some(&proposer))
            .unwrap();
        assert!(is_freeze(&decision.action));
    }
    assert_eq!(proposer.calls(), 0);
    assert!(world.task(ROOT).context_notes.is_empty());
    assert!(world.task(CHILD).context_notes.is_empty());
    // Already frozen: no second freeze event, but each decision is still logged.
    assert_eq!(world.events.count(EventKind::BudgetFrozen), 0);
    assert_eq!(world.events.count(EventKind::UnblockStep), 3);
}

#[test]
fn budget_reported_by_the_signal_also_freezes() {
    let world = World::new(UnblockerConfig::default());
    let signal = FailureSignal {
        budget: BudgetCheck::WallClockExceeded,
        ..stuck_signal("e")
    };
    let decision = world.ladder().next_action(ROOT, &signal, None).unwrap();
    assert!(is_freeze(&decision.action));
    assert!(world.budgets.get_budget(ROOT).unwrap().frozen);
}

#[test]
fn missing_budget_row_is_created_from_config() {
    let cfg = UnblockerConfig {
        max_unblock_cycles: 1,
        ..UnblockerConfig::default()
    };
    let world = World::new(cfg.clone());
    world.budgets.0.lock().unwrap().clear();
    assert!(!is_freeze(&step(&world, ROOT)));
    let budget = world.budgets.get_budget(ROOT).unwrap();
    assert_eq!(
        budget,
        RootBudget {
            cycles_used: 1,
            ..RootBudget::new(ROOT, &cfg)
        }
    );
    assert!(is_freeze(&step(&world, ROOT)));
}

#[test]
fn factory_freezes_after_exactly_n_consecutive_root_failures() {
    let world = World::new(UnblockerConfig {
        global_freeze_consecutive_fails: 3,
        ..UnblockerConfig::default()
    });
    let ladder = world.ladder();
    assert!(!ladder.is_frozen().unwrap());
    assert!(!ladder.record_root_result("r1", false).unwrap());
    assert!(!ladder.record_root_result("r2", false).unwrap());
    assert!(!ladder.is_frozen().unwrap());
    assert_eq!(world.events.count(EventKind::FactoryFrozen), 0);

    assert!(ladder.record_root_result("r3", false).unwrap());
    assert!(ladder.is_frozen().unwrap());
    assert_eq!(
        world.kv.get_meta(META_FACTORY_FROZEN).unwrap().as_deref(),
        Some("1")
    );
    assert_eq!(
        world
            .kv
            .get_meta(META_CONSECUTIVE_ROOT_FAILS)
            .unwrap()
            .as_deref(),
        Some("3")
    );
    let event = world.events.last(EventKind::FactoryFrozen);
    assert_eq!(event.task_id.as_deref(), Some("r3"));
    let payload: serde_json::Value = serde_json::from_str(&event.payload_json).unwrap();
    assert_eq!(payload["consecutive_fails"], 3);

    // Further failures while frozen do not announce the freeze again.
    assert!(ladder.record_root_result("r4", false).unwrap());
    assert_eq!(world.events.count(EventKind::FactoryFrozen), 1);
}

#[test]
fn a_success_resets_the_failure_streak() {
    let world = World::new(UnblockerConfig {
        global_freeze_consecutive_fails: 3,
        ..UnblockerConfig::default()
    });
    let ladder = world.ladder();
    for ok in [false, false, true, false, false] {
        assert!(!ladder.record_root_result("r", ok).unwrap());
    }
    assert!(!ladder.is_frozen().unwrap());
    assert!(ladder.record_root_result("r", false).unwrap());
}

#[test]
fn a_success_does_not_unfreeze_but_unfreeze_does() {
    let world = World::new(UnblockerConfig {
        global_freeze_consecutive_fails: 1,
        ..UnblockerConfig::default()
    });
    let ladder = world.ladder();
    assert!(ladder.record_root_result("r", false).unwrap());
    assert!(
        ladder.record_root_result("r", true).unwrap(),
        "only a human unfreezes"
    );

    ladder.unfreeze().unwrap();
    assert!(!ladder.is_frozen().unwrap());
    assert_eq!(
        world
            .kv
            .get_meta(META_CONSECUTIVE_ROOT_FAILS)
            .unwrap()
            .as_deref(),
        Some("0")
    );
    assert_eq!(world.events.count(EventKind::FactoryUnfrozen), 1);

    // The streak starts over after an unfreeze.
    assert!(ladder.record_root_result("r", false).unwrap());
    assert_eq!(world.events.count(EventKind::FactoryFrozen), 2);
}

#[test]
fn threshold_zero_behaves_like_one() {
    let world = World::new(UnblockerConfig {
        global_freeze_consecutive_fails: 0,
        ..UnblockerConfig::default()
    });
    assert!(!world.ladder().record_root_result("r", true).unwrap());
    assert!(world.ladder().record_root_result("r", false).unwrap());
}

#[test]
fn corrupt_counter_is_an_error_not_a_silent_reset() {
    let world = World::new(UnblockerConfig::default());
    world
        .kv
        .set_meta(META_CONSECUTIVE_ROOT_FAILS, "many")
        .unwrap();
    let err = world.ladder().record_root_result("r", false).unwrap_err();
    assert!(matches!(err, PortError::Invalid(_)));
}
