//! Failures: the unblocker ladder, budgets, freezes, quota waits and what
//! an interrupted run leaves behind.

use std::sync::Mutex;
use std::sync::atomic::Ordering;

use aaf_domain::blocked::BlockedReason;
use aaf_domain::event::EventKind;
use aaf_domain::ports::{
    BudgetStore, EventSink, KvStore, META_CONSECUTIVE_ROOT_FAILS, RunStore, TaskStore,
};
use aaf_domain::run::{RunRecord, RunStatus};
use aaf_domain::task::TaskState;

use super::fakes::{RunKind, failed_outcome, limit_outcome};
use super::world::World;
use crate::factory::{Mode, is_frozen};

fn make_red(world: &World, task_id: &str) {
    world
        .gate
        .state
        .lock()
        .unwrap()
        .red
        .insert(task_id.to_string());
}

fn last_agent_prompt(world: &World) -> String {
    let runner = world.runner.0.lock().unwrap();
    let agent = |r: &&aaf_domain::ports::AgentRequest| r.prompt.contains("# Task:");
    runner
        .requests
        .iter()
        .rev()
        .find(agent)
        .unwrap()
        .prompt
        .clone()
}

#[test]
fn a_red_gate_climbs_the_ladder_one_tier_per_run_and_keeps_the_markers() {
    let world = World::new();
    world.add("t1");
    make_red(&world, "t1");

    // Run 1: T3, back to Ready with the failure noted.
    let summary = world.once();
    assert_eq!(summary.requeued, vec!["t1"]);
    assert_eq!(world.state("t1"), TaskState::Ready);
    assert_eq!(
        summary.agent_runs, 1,
        "one agent run per task and invocation"
    );
    let notes = world.task("t1").context_notes;
    assert_eq!(notes.len(), 1);
    assert!(notes[0].starts_with("unblock[T3] digest="), "{notes:?}");
    assert!(world.notifier.0.lock().unwrap().is_empty());

    // Run 2: the agent is told what failed; the same failure again reaches
    // T4, the proposer has no split, so T5 hands it to a human.
    let summary = world.once();
    assert!(
        last_agent_prompt(&world).contains(&notes[0]),
        "marker line kept in the prompt"
    );
    assert_eq!(summary.blocked, vec!["t1"]);
    assert_eq!(world.runner.count(&RunKind::Proposer), 1);
    let task = world.task("t1");
    assert_eq!(task.state, TaskState::Blocked);
    assert!(matches!(
        task.blocked_reason,
        Some(BlockedReason::NeedsHuman { .. })
    ));
    assert_eq!(task.unblock_cycles, 2);
    assert_eq!(
        task.context_notes, notes,
        "the T3 marker is still there, untouched"
    );
    assert_eq!(task.spec, super::fakes::spec(), "the spec never changes");

    let sent = world.notifier.0.lock().unwrap();
    assert_eq!(sent.len(), 1, "webhook on T5 only");
    assert_eq!(sent[0].task_id.as_deref(), Some("t1"));
    assert_eq!(sent[0].status, "needs_human");
    let events = world.db.events_for_task("t1", 100).unwrap();
    assert!(events.iter().any(|e| e.kind == EventKind::WebhookSent));
    assert_eq!(
        world
            .db
            .get_meta(META_CONSECUTIVE_ROOT_FAILS)
            .unwrap()
            .as_deref(),
        Some("1")
    );
}

#[test]
fn tier_four_creates_subtasks_that_hold_the_parent_back() {
    let world = World::new();
    world.add("t1");
    make_red(&world, "t1");
    world.runner.0.lock().unwrap().proposer_text = Some(
        r#"[{"title":"part one","description":"first half","acceptance":["a passes"]},
            {"title":"part two","description":"second half","acceptance":["b passes"]}]"#
            .to_string(),
    );

    world.once();
    let summary = world.once();

    // The parent is Ready again but its new dependencies keep it waiting;
    // the two subtasks ran in the same invocation.
    assert_eq!(world.state("t1"), TaskState::Ready);
    let children = world.db.children_of("t1").unwrap();
    let ids: Vec<&str> = children.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, vec!["t1-s1", "t1-s2"]);
    assert!(children.iter().all(|c| c.depth == 1 && c.root_id == "t1"));
    let mut deps = world.db.deps_of("t1").unwrap();
    deps.sort();
    assert_eq!(deps, vec!["t1-s1", "t1-s2"]);
    assert_eq!(summary.review, vec!["t1-s1", "t1-s2"]);
    assert_eq!(summary.agent_runs, 3);
    assert!(world.task("t1").context_notes[1].starts_with("unblock[T4] digest="));

    // Nothing more to start: the parent waits for its subtasks to be done.
    let summary = world.once();
    assert_eq!(summary.agent_runs, 0);
    assert_eq!(summary.waiting_for_account, 0);
}

#[test]
fn a_limit_hit_on_the_only_account_waits_for_the_cooldown_instead_of_failing() {
    let world = World::new();
    world.add("t1");
    world.runner.script("t1", vec![limit_outcome(Some(5_000))]);

    let summary = world.once();

    assert_eq!(summary.requeued, vec!["t1"]);
    let task = world.task("t1");
    assert_eq!(task.state, TaskState::Ready, "not Blocked");
    assert_eq!(task.blocked_reason, None);
    assert!(world.notifier.0.lock().unwrap().is_empty());
    assert_eq!(
        world.db.get_meta(META_CONSECUTIVE_ROOT_FAILS).unwrap(),
        None
    );
    assert_eq!(world.pool.state.lock().unwrap().cooldown["a"], 5_000);
    let runs = world.db.runs_for_task("t1").unwrap();
    assert_eq!(runs[0].status, RunStatus::LimitHit);

    // Still cooling down: nothing starts, the task just waits.
    let summary = world.once();
    assert_eq!(summary.agent_runs, 0);
    assert_eq!(summary.waiting_for_account, 1);
    assert_eq!(world.state("t1"), TaskState::Ready);

    // Auto mode: the idle hook is where the loop waits; once the cooldown
    // is over the task runs on the same account.
    let idles = Mutex::new(0);
    let summary = world.run(Mode::Auto, &|| {
        let mut idles = idles.lock().unwrap();
        *idles += 1;
        world.clock.0.store(5_001, Ordering::SeqCst);
        *idles == 1
    });
    assert_eq!(summary.review, vec!["t1"]);
    assert_eq!(
        world.task("t1").unblock_cycles,
        0,
        "quota rate limits do not burn unblock cycles"
    );
}

#[test]
fn a_limit_hit_moves_the_task_to_another_account_when_there_is_one() {
    let world = World::with_accounts(&["a", "b"]);
    world.add("t1");
    world.runner.script("t1", vec![limit_outcome(None)]);

    let summary = world.once();

    assert_eq!(summary.review, vec!["t1"]);
    let runs = world.db.runs_for_task("t1").unwrap();
    let accounts: Vec<Option<&str>> = runs.iter().map(|r| r.account_id.as_deref()).collect();
    assert_eq!(accounts, vec![Some("a"), Some("b")]);
    // No reset time given: five hours from the hit.
    assert_eq!(
        world.pool.state.lock().unwrap().cooldown["a"],
        1_000 + 18_000
    );
}

#[test]
fn an_exhausted_token_budget_freezes_the_root_and_tells_a_human() {
    let mut world = World::new();
    world.repo.unblocker.max_token_budget = 14;
    world.add("t1");
    world
        .runner
        .script("t1", vec![failed_outcome("compile error")]);

    let summary = world.once();

    assert_eq!(summary.blocked, vec!["t1"]);
    let budget = world.db.get_budget("t1").unwrap();
    // 10 in (input + cache creation) + 5 out is past the limit of 14.
    assert_eq!(budget.tokens_used, 15);
    assert!(budget.frozen);
    let task = world.task("t1");
    assert!(
        matches!(
            task.blocked_reason,
            Some(BlockedReason::BudgetExceeded { .. })
        ),
        "{:?}",
        task.blocked_reason
    );
    assert_eq!(world.notifier.0.lock().unwrap()[0].status, "budget_frozen");

    // Put back by hand without a fresh budget: no agent run is paid for.
    world
        .db
        .set_state("t1", TaskState::Ready, None, 2_000)
        .unwrap();
    world.once();
    assert_eq!(world.runner.count(&RunKind::Agent), 1);
    assert_eq!(world.state("t1"), TaskState::Blocked);
}

#[test]
fn a_budget_exactly_at_its_limit_still_runs() {
    let mut world = World::new();
    world.repo.unblocker.max_token_budget = 15;
    world.add("t1");

    world.once();
    world
        .db
        .set_state("t1", TaskState::Ready, None, 2_000)
        .unwrap();
    world.once();

    assert_eq!(
        world.runner.count(&RunKind::Agent),
        2,
        "15 of 15 is not exceeded"
    );
    assert!(!world.db.get_budget("t1").unwrap().frozen);
}

#[test]
fn consecutive_failed_roots_freeze_the_factory_until_it_is_unfrozen() {
    let mut world = World::new();
    world.repo.unblocker.global_freeze_consecutive_fails = 2;
    world.repo.unblocker.max_token_budget = 14;
    for id in ["t1", "t2", "t3"] {
        world.add(id);
        world
            .runner
            .script(id, vec![failed_outcome("compile error")]);
    }

    let summary = world.once();

    assert!(summary.frozen);
    assert!(is_frozen(&world.db).unwrap());
    assert!(summary.blocked.len() >= 2, "{summary:?}");
    assert!(
        matches!(
            world.task("t1").blocked_reason,
            Some(BlockedReason::BudgetExceeded { .. })
        ),
        "{:?}",
        world.task("t1").blocked_reason
    );

    // Frozen: a new run starts nothing at all.
    world.add("t9");
    let before = world.runner.count(&RunKind::Agent);
    let summary = world.once();
    assert!(summary.frozen);
    assert_eq!(summary.agent_runs, 0);
    assert_eq!(world.runner.count(&RunKind::Agent), before);
    assert_eq!(world.state("t9"), TaskState::Ready);

    // `aaf board unfreeze` clears the flag and the counter.
    crate::factory::unfreeze(&world.db, &*world.clock).unwrap();
    assert!(!is_frozen(&world.db).unwrap());
    world.repo.unblocker.max_token_budget = 10_000;
    let summary = world.once();
    assert!(!summary.frozen);
    assert!(summary.review.contains(&"t9".to_string()));
}

#[test]
fn a_success_resets_the_consecutive_failure_counter() {
    let mut world = World::new();
    world.repo.unblocker.global_freeze_consecutive_fails = 2;
    world.repo.unblocker.max_token_budget = 14;
    world.add("t1");
    world
        .runner
        .script("t1", vec![failed_outcome("compile error")]);
    world.once();
    assert_eq!(
        world
            .db
            .get_meta(META_CONSECUTIVE_ROOT_FAILS)
            .unwrap()
            .as_deref(),
        Some("1")
    );

    world.repo.unblocker.max_token_budget = 10_000;
    world.add("t2");
    world.once();

    assert_eq!(
        world
            .db
            .get_meta(META_CONSECUTIVE_ROOT_FAILS)
            .unwrap()
            .as_deref(),
        Some("0")
    );
    assert!(!is_frozen(&world.db).unwrap());
}

#[test]
fn unverified_gate_blocks_only_that_task_without_freezing_factory() {
    let mut world = World::new();
    world.repo.unblocker.global_freeze_consecutive_fails = 2;
    world.gate.state.lock().unwrap().unverified = true;
    for id in ["t1", "t2"] {
        world.add(id);
    }

    let summary = world.once();

    assert!(!summary.frozen);
    assert!(!is_frozen(&world.db).unwrap());
    assert_eq!(world.state("t1"), TaskState::Blocked);
    assert_eq!(world.state("t2"), TaskState::Blocked);
}

#[test]
fn an_agent_that_changes_nothing_is_a_failure_with_a_note() {
    let world = World::new();
    world.add("t1");
    world
        .git
        .0
        .lock()
        .unwrap()
        .no_changes
        .insert("t1".to_string());

    let summary = world.once();

    assert_eq!(summary.requeued, vec!["t1"]);
    assert_eq!(summary.gate_runs, 0, "nothing to gate");
    let notes = world.task("t1").context_notes;
    assert!(notes[0].contains("without changing anything"), "{notes:?}");
    // O12-R10-6: what the agent said about it, framed as untrusted data.
    assert_eq!(
        notes[1], "no_change_reason: <untrusted-data>done</untrusted-data>",
        "{notes:?}"
    );
    assert!(notes[2].starts_with("unblock[T3] digest="), "{notes:?}");
}

#[test]
fn an_executor_error_blocks_the_task_and_is_reported() {
    let world = World::new();
    world.add("t1");
    world.add("t2");
    world
        .runner
        .0
        .lock()
        .unwrap()
        .broken
        .insert("t1".to_string());

    let summary = world.once();

    assert_eq!(summary.blocked, vec!["t1"]);
    assert_eq!(summary.review, vec!["t2"], "the other task is not affected");
    assert_eq!(summary.errors, vec!["t1: io: executor crashed"]);
    let BlockedReason::NeedsHuman { why } = world.task("t1").blocked_reason.unwrap() else {
        panic!("not needs_human");
    };
    assert_eq!(why, "factory error: io: executor crashed");
    let runs = world.db.runs_for_task("t1").unwrap();
    assert_eq!(
        runs[0].status,
        RunStatus::Failed,
        "the run record is closed"
    );
    assert!(world.pool.state.lock().unwrap().leased.is_empty());
}

#[test]
fn an_interrupted_run_is_reaped_and_its_tasks_are_picked_up_again() {
    let world = World::new();
    // t1 was mid-agent, t2 was waiting for the gate when the process died.
    for id in ["t1", "t2"] {
        world.add(id);
        world.db.set_state(id, TaskState::Running, None, 1).unwrap();
    }
    world
        .db
        .set_state("t2", TaskState::Gating, None, 1)
        .unwrap();
    world
        .db
        .insert_run(&RunRecord {
            id: "run-old".to_string(),
            task_id: "t1".to_string(),
            account_id: Some("a".to_string()),
            executor: "claude".to_string(),
            worktree_path: "/fake/wt/t1".to_string(),
            branch: "aaf/t1".to_string(),
            status: RunStatus::Running,
            exit_code: None,
            tokens_in: 0,
            tokens_out: 0,
            started_at: 1,
            ended_at: None,
        })
        .unwrap();
    let reaped = Mutex::new(Vec::new());
    let hooks = crate::factory::Hooks {
        idle: &|| false,
        reap: &|run_id| {
            reaped.lock().unwrap().push(run_id.to_string());
            Ok(())
        },
    };

    let summary = crate::factory::run(&world.deps(), Mode::Once, &hooks).unwrap();

    assert_eq!(*reaped.lock().unwrap(), vec!["run-old"]);
    let runs = world.db.runs_for_task("t1").unwrap();
    assert_eq!(runs[0].status, RunStatus::Killed);
    assert_eq!(runs.len(), 2, "t1 ran again");
    // A rerun first commits what the dead run left in the worktree (git
    // refuses to rebase a dirty tree), then picks up what landed on the
    // base meanwhile.
    assert_eq!(
        world.git.0.lock().unwrap().log,
        vec!["commit:t1", "rebase:t1", "commit:t1"]
    );
    assert_eq!(
        world.runner.count(&RunKind::Agent),
        1,
        "t2 only needed the gate"
    );
    let mut review = summary.review.clone();
    review.sort();
    assert_eq!(review, vec!["t1", "t2"]);
}

#[test]
fn proof_only_gate_block_without_host_capture_stops_for_human_without_rework_or_freeze() {
    struct ProofOnlyGate;
    impl crate::factory::ports::GateRunner for ProofOnlyGate {
        fn head_lock(
            &self,
            _tree: &std::path::Path,
        ) -> aaf_domain::ports::PortResult<aaf_domain::gate::HeadLockReport> {
            Ok(aaf_domain::gate::HeadLockReport {
                status: aaf_domain::gate::LockStatus::Clear,
                lock_fp: String::new(),
                locked_changes: Vec::new(),
                approval: None,
            })
        }
        fn run(
            &self,
            tree: &std::path::Path,
        ) -> aaf_domain::ports::PortResult<aaf_domain::gate::Receipt> {
            Ok(aaf_domain::gate::Receipt {
                schema_version: aaf_domain::gate::RECEIPT_SCHEMA_VERSION,
                repo_root: tree.to_string_lossy().into_owned(),
                tree_fp: "git-tree:t1".to_string(),
                head_sha: None,
                base_sha: None,
                mode: aaf_domain::gate::GateMode::Scoped,
                status: aaf_domain::gate::GateStatus::Blocked,
                reasons: vec![
                    "proof required: fingerprint: io: No such file or directory (os error 2)"
                        .to_string(),
                    "commands skipped: an earlier stage already blocks".to_string(),
                ],
                config_hash: String::new(),
                commands: Vec::new(),
                static_findings: Vec::new(),
                suppressions: 0,
                head_lock: aaf_domain::gate::HeadLockReport {
                    status: aaf_domain::gate::LockStatus::Clear,
                    lock_fp: String::new(),
                    locked_changes: Vec::new(),
                    approval: None,
                },
                store_compliance: aaf_domain::gate::StoreComplianceReport::not_applicable(),
                created_at: 0,
            })
        }
    }

    let mut world = World::new();
    world.repo.unblocker.global_freeze_consecutive_fails = 1;
    world.repo.proof.required = true;
    world.add("t1");
    let mut deps = world.deps();
    deps.gate = std::sync::Arc::new(ProofOnlyGate);
    let hooks = crate::factory::Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    let summary = crate::factory::run(&deps, Mode::Once, &hooks).unwrap();

    assert_eq!(world.state("t1"), TaskState::Blocked, "{summary:?}");
    assert!(summary.requeued.is_empty(), "must not trigger T3/T4 rework");
    assert!(
        !summary.frozen && !is_frozen(&world.db).unwrap(),
        "missing operator proof must not freeze the global factory"
    );
    assert!(
        matches!(
            world.task("t1").blocked_reason,
            Some(BlockedReason::NeedsHuman { ref why }) if why.contains("missing signed device proof")
        ),
        "{:?}",
        world.task("t1").blocked_reason
    );
}
