#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "unblock_support.rs"]
mod support;

use aaf_domain::blocked::{BlockedReason, UnblockAction, UnblockTier};
use aaf_domain::config::UnblockerConfig;
use aaf_domain::event::EventKind;
use aaf_domain::gate::GateStatus;
use aaf_domain::ports::{BudgetStore, PortError};
use aaf_engine::unblocker::{FailureSignal, QuotaRoute, RerunResult, failure_digest};
use support::{CHILD, FakeProposer, ROOT, World, limit_outcome, plan, receipt, spec, stuck_signal};

fn roomy() -> UnblockerConfig {
    UnblockerConfig {
        max_unblock_cycles: 100,
        ..UnblockerConfig::default()
    }
}

fn two_plans() -> FakeProposer {
    FakeProposer::new(Ok(vec![plan("a"), plan("b")]))
}

/// The same failure seen on run `n`: only volatile details differ.
fn repeat(n: u32) -> FailureSignal {
    FailureSignal {
        same_failure_count: n,
        ..stuck_signal(&format!(
            "panicked at /tmp/wt-{n}/src/auth.rs:{}: token expired (pid {})",
            40 + n,
            1000 + n
        ))
    }
}

fn tiers(world: &World, id: &str, signals: &[FailureSignal], p: &FakeProposer) -> Vec<UnblockTier> {
    signals
        .iter()
        .map(|s| world.ladder().next_action(id, s, Some(p)).unwrap().tier)
        .collect()
}

#[test]
fn repeating_failure_climbs_t3_t4_t5_in_order() {
    let world = World::new(roomy());
    let proposer = two_plans();
    let ladder = world.ladder();
    let digest = failure_digest(&repeat(1));

    let first = ladder
        .next_action(ROOT, &repeat(1), Some(&proposer))
        .unwrap();
    assert_eq!(first.tier, UnblockTier::T3FreshContext);
    assert_eq!(
        first.action,
        UnblockAction::RetryFreshContext {
            digest: digest.clone()
        }
    );
    assert_eq!(proposer.calls(), 0, "T3 must not consult the proposer");

    let second = ladder
        .next_action(ROOT, &repeat(2), Some(&proposer))
        .unwrap();
    assert_eq!(second.tier, UnblockTier::T4Decompose);
    assert_eq!(
        second.action,
        UnblockAction::SpawnSubtasks {
            plans: vec![plan("a"), plan("b")]
        }
    );

    let third = ladder
        .next_action(ROOT, &repeat(3), Some(&proposer))
        .unwrap();
    assert_eq!(third.tier, UnblockTier::T5Escalate);
    assert!(matches!(third.action, UnblockAction::EscalateHuman { .. }));
    assert_eq!(proposer.calls(), 1, "T4 is tried once per failure");

    let task = world.task(ROOT);
    assert_eq!(task.unblock_cycles, 3);
    assert_eq!(world.budgets.get_budget(ROOT).unwrap().cycles_used, 3);
    assert_eq!(task.context_notes.len(), 2);
    assert!(task.context_notes[0].starts_with(&format!("unblock[T3] digest={digest}: ")));
    assert!(task.context_notes[1].starts_with(&format!("unblock[T4] digest={digest}: ")));
}

#[test]
fn every_decision_emits_one_unblock_step_event() {
    let world = World::new(roomy());
    let proposer = two_plans();
    tiers(&world, ROOT, &[repeat(1), repeat(2), repeat(3)], &proposer);
    assert_eq!(world.events.count(EventKind::UnblockStep), 3);

    let last = world.events.last(EventKind::UnblockStep);
    assert_eq!(last.task_id.as_deref(), Some(ROOT));
    assert_eq!(last.created_at, 1_000);
    let payload: serde_json::Value = serde_json::from_str(&last.payload_json).unwrap();
    assert_eq!(payload["tier"], "T5");
    assert_eq!(payload["cycle"], 3);
    assert_eq!(payload["reason"]["kind"], "stuck");
    assert_eq!(payload["action"]["action"], "escalate_human");
    assert_eq!(payload["digest"], failure_digest(&repeat(1)).as_str());
}

#[test]
fn merge_conflict_gets_one_mechanical_try_then_climbs() {
    let world = World::new(roomy());
    let proposer = two_plans();
    let conflict = FailureSignal {
        merge_conflict_files: vec!["src/auth.rs".to_string()],
        ..FailureSignal::empty()
    };
    let ladder = world.ladder();
    let first = ladder
        .next_action(ROOT, &conflict, Some(&proposer))
        .unwrap();
    assert_eq!(first.tier, UnblockTier::T2Mechanical);
    assert_eq!(first.action, UnblockAction::RebaseAndRegate);
    assert!(matches!(first.reason, BlockedReason::MergeConflict { .. }));

    let rest = tiers(
        &world,
        ROOT,
        &[conflict.clone(), conflict.clone(), conflict],
        &proposer,
    );
    assert_eq!(
        rest,
        [
            UnblockTier::T3FreshContext,
            UnblockTier::T4Decompose,
            UnblockTier::T5Escalate
        ]
    );
}

#[test]
fn flaky_test_is_rerun_first() {
    let world = World::new(roomy());
    let flaky = FailureSignal {
        test_rerun: Some(RerunResult { runs: 2, passed: 2 }),
        ..stuck_signal("test login ... FAILED")
    };
    let first = world.ladder().next_action(ROOT, &flaky, None).unwrap();
    assert_eq!(
        (first.tier, first.action),
        (UnblockTier::T2Mechanical, UnblockAction::RerunTests)
    );
    let second = world.ladder().next_action(ROOT, &flaky, None).unwrap();
    assert_eq!(second.tier, UnblockTier::T3FreshContext);
}

#[test]
fn quota_switches_account_then_waits_and_never_climbs() {
    let world = World::new(roomy());
    let proposer = two_plans();
    let hit = |n| FailureSignal {
        outcome: Some(limit_outcome(Some(5_000))),
        same_failure_count: n,
        ..FailureSignal::empty()
    };
    let ladder = world.ladder();
    let first = ladder.next_action(ROOT, &hit(1), Some(&proposer)).unwrap();
    assert_eq!(first.tier, UnblockTier::T2Mechanical);
    assert_eq!(first.action, UnblockAction::SwitchAccount);
    assert_eq!(
        first.reason,
        BlockedReason::Quota {
            reset_at: Some(5_000)
        }
    );
    for n in 2..6 {
        let next = ladder.next_action(ROOT, &hit(n), Some(&proposer)).unwrap();
        assert_eq!(next.tier, UnblockTier::T2Mechanical);
        assert_eq!(
            next.action,
            UnblockAction::WaitForAccount { until: Some(5_000) }
        );
    }
    assert_eq!(proposer.calls(), 0);
    assert!(world.task(ROOT).context_notes.is_empty());
}

#[test]
fn a_quota_hit_follows_what_the_account_pool_says_not_the_repeat_count() {
    let world = World::new(roomy());
    let hit = |n| FailureSignal {
        outcome: Some(limit_outcome(Some(5_000))),
        same_failure_count: n,
        ..FailureSignal::empty()
    };
    let ladder = world.ladder();
    // A repository pinned to one account: the first hit already waits.
    let pinned = ladder
        .next_action_routed(ROOT, &hit(1), None, Some(QuotaRoute::Wait))
        .unwrap();
    assert_eq!(
        pinned.action,
        UnblockAction::WaitForAccount { until: Some(5_000) }
    );
    // A pool of three: the second hit in a row still leaves an account.
    let third = ladder
        .next_action_routed(ROOT, &hit(2), None, Some(QuotaRoute::Switch))
        .unwrap();
    assert_eq!(third.tier, UnblockTier::T2Mechanical);
    assert_eq!(third.action, UnblockAction::SwitchAccount);
}

#[test]
fn open_dependency_waits_for_it() {
    let world = World::new(roomy());
    let signal = FailureSignal {
        open_dependency: Some("dep-9".to_string()),
        ..stuck_signal("e")
    };
    for _ in 0..3 {
        let decision = world.ladder().next_action(ROOT, &signal, None).unwrap();
        assert_eq!(decision.tier, UnblockTier::T2Mechanical);
        assert_eq!(
            decision.action,
            UnblockAction::WaitForDependency {
                task_id: "dep-9".to_string()
            }
        );
    }
}

#[test]
fn gate_failure_starts_at_t3_and_notes_the_summary() {
    let world = World::new(roomy());
    let signal = FailureSignal {
        gate: Some(receipt(
            GateStatus::Blocked,
            &["test_scoped failed"],
            "test login ... FAILED",
        )),
        ..FailureSignal::empty()
    };
    let decision = world.ladder().next_action(ROOT, &signal, None).unwrap();
    assert_eq!(decision.tier, UnblockTier::T3FreshContext);
    let notes = world.task(ROOT).context_notes;
    assert!(notes[0].ends_with("previous attempt failed (gate_failed): test_scoped failed"));
}

#[test]
fn a_different_failure_gets_its_own_fresh_context_retry() {
    let world = World::new(roomy());
    let proposer = two_plans();
    let other = stuck_signal("error: linker `cc` not found");
    assert_eq!(
        tiers(
            &world,
            ROOT,
            &[repeat(1), other.clone(), other, repeat(2)],
            &proposer
        ),
        [
            UnblockTier::T3FreshContext,
            UnblockTier::T3FreshContext,
            UnblockTier::T4Decompose,
            UnblockTier::T4Decompose
        ]
    );
}

#[test]
fn depth_one_task_never_reaches_t4() {
    let world = World::new(roomy());
    let proposer = two_plans();
    let seen = tiers(
        &world,
        CHILD,
        &[repeat(1), repeat(2), repeat(3), repeat(4)],
        &proposer,
    );
    assert_eq!(seen[0], UnblockTier::T3FreshContext);
    assert!(seen[1..].iter().all(|t| *t == UnblockTier::T5Escalate));
    assert_eq!(proposer.calls(), 0);
    // The child spends from the root's budget.
    assert_eq!(world.budgets.get_budget(ROOT).unwrap().cycles_used, 4);

    let why = match world
        .ladder()
        .next_action(CHILD, &repeat(5), Some(&proposer))
        .unwrap()
        .action
    {
        UnblockAction::EscalateHuman { why } => why,
        other => panic!("expected escalation, got {other:?}"),
    };
    assert!(why.contains("depth"));
}

#[test]
fn config_depth_zero_disables_decomposition_for_roots() {
    let world = World::new(UnblockerConfig {
        max_subtask_depth: 0,
        ..roomy()
    });
    let proposer = two_plans();
    assert_eq!(
        tiers(&world, ROOT, &[repeat(1), repeat(2)], &proposer),
        [UnblockTier::T3FreshContext, UnblockTier::T5Escalate]
    );
    assert_eq!(proposer.calls(), 0);
}

#[test]
fn unusable_proposals_escalate_instead_of_spawning() {
    let five = (0..5).map(|i| plan(&format!("p{i}"))).collect();
    let mut blank = plan("x");
    blank.acceptance.clear();
    let cases = vec![
        ("no proposer", None),
        ("empty", Some(FakeProposer::new(Ok(Vec::new())))),
        ("more than four", Some(FakeProposer::new(Ok(five)))),
        (
            "invalid plan",
            Some(FakeProposer::new(Ok(vec![plan("ok"), blank]))),
        ),
        (
            "proposer error",
            Some(FakeProposer::new(Err(PortError::Io("down".into())))),
        ),
    ];
    for (name, proposer) in cases {
        let world = World::new(roomy());
        let ladder = world.ladder();
        let as_dyn = proposer
            .as_ref()
            .map(|p| p as &dyn aaf_engine::unblocker::SubtaskProposer);
        ladder.next_action(ROOT, &repeat(1), as_dyn).unwrap();
        let decision = ladder.next_action(ROOT, &repeat(2), as_dyn).unwrap();
        assert_eq!(decision.tier, UnblockTier::T5Escalate, "case: {name}");
        assert!(
            matches!(decision.action, UnblockAction::EscalateHuman { .. }),
            "case: {name}"
        );
        assert_eq!(world.task(ROOT).context_notes.len(), 1, "case: {name}");
    }
}

#[test]
fn four_plans_are_accepted() {
    let world = World::new(roomy());
    let four: Vec<_> = (0..4).map(|i| plan(&format!("p{i}"))).collect();
    let proposer = FakeProposer::new(Ok(four.clone()));
    let seen = tiers(&world, ROOT, &[repeat(1), repeat(2)], &proposer);
    assert_eq!(seen[1], UnblockTier::T4Decompose);
}

#[test]
fn spec_is_byte_identical_after_any_number_of_steps() {
    let world = World::new(roomy());
    let proposer = two_plans();
    let before = serde_json::to_vec(&spec()).unwrap();
    let conflict = FailureSignal {
        merge_conflict_files: vec!["src/auth.rs".to_string()],
        ..FailureSignal::empty()
    };
    let gate = FailureSignal {
        gate: Some(receipt(
            GateStatus::Blocked,
            &["lint failed"],
            "warning: unused",
        )),
        ..FailureSignal::empty()
    };
    for round in 0..8 {
        for signal in [repeat(round), conflict.clone(), gate.clone()] {
            for id in [ROOT, CHILD] {
                world
                    .ladder()
                    .next_action(id, &signal, Some(&proposer))
                    .unwrap();
            }
        }
    }
    for id in [ROOT, CHILD] {
        let task = world.task(id);
        assert_eq!(serde_json::to_vec(&task.spec).unwrap(), before);
        assert!(!task.context_notes.is_empty());
        assert!(
            task.context_notes
                .iter()
                .all(|n| n.chars().count() <= 300 && !n.contains('\n'))
        );
    }
}

#[test]
fn unknown_task_is_an_error_and_records_nothing() {
    let world = World::new(roomy());
    let err = world
        .ladder()
        .next_action("nope", &repeat(1), None)
        .unwrap_err();
    assert!(matches!(err, PortError::NotFound(_)));
    assert_eq!(world.events.count(EventKind::UnblockStep), 0);
    assert_eq!(world.budgets.get_budget(ROOT).unwrap().cycles_used, 0);
}
