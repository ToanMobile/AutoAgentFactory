//! The loop end to end on fakes: pick, run, hand to the gate, land.

use aaf_domain::event::EventKind;
use aaf_domain::ports::{BudgetStore, EventSink, RunStore, TaskStore};
use aaf_domain::run::RunStatus;
use aaf_domain::task::TaskState;

use super::fakes::{RunKind, audit_answer};
use super::world::World;
use crate::factory::Mode;
use crate::git::{MergeOutcome, PrOutcome};

fn kinds(world: &World, task_id: &str) -> Vec<EventKind> {
    let mut events = world.db.events_for_task(task_id, 100).unwrap();
    events.reverse();
    events.into_iter().map(|e| e.kind).collect()
}

fn strings(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| (*id).to_string()).collect()
}

#[test]
fn a_ready_task_runs_is_gated_and_lands_in_review_with_its_usage_recorded() {
    let world = World::new();
    world.add("t1");

    let summary = world.once();

    assert_eq!(summary.review, vec!["t1"]);
    assert_eq!((summary.agent_runs, summary.gate_runs), (1, 1));
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    assert_eq!(world.state("t1"), TaskState::Review);

    let runs = world.db.runs_for_task("t1").unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, RunStatus::Succeeded);
    assert_eq!(runs[0].branch, "aaf/t1");
    assert_eq!(runs[0].account_id.as_deref(), Some("a"));
    assert_eq!((runs[0].tokens_in, runs[0].tokens_out), (10, 5));
    // input (with cache creation) + output
    assert_eq!(world.db.get_budget("t1").unwrap().tokens_used, 15);

    let request = world.runner.0.lock().unwrap().requests[0].clone();
    assert_eq!(request.cwd, "/fake/wt/t1");
    assert_eq!(request.run_id, runs[0].id);
    assert!(
        request
            .prompt
            .contains("Make the parser accept empty input.")
    );
    assert!(
        request
            .prompt
            .contains("empty input parses to an empty list")
    );
    assert_eq!(
        kinds(&world, "t1"),
        vec![
            EventKind::TaskStateChanged, // running
            EventKind::RunStarted,
            EventKind::RunFinished,
            EventKind::TaskStateChanged, // gating
            EventKind::GateResult,
            EventKind::TaskStateChanged, // review
        ]
    );
}

#[test]
fn a_pull_request_url_is_recorded_when_one_is_opened() {
    let world = World::new();
    world.add("t1");
    world.git.0.lock().unwrap().pr = Some(PrOutcome::Opened {
        url: "https://example.invalid/pr/7".to_string(),
    });

    world.once();

    let events = world.db.events_for_task("t1", 100).unwrap();
    let opened = events
        .iter()
        .find(|e| e.kind == EventKind::PrOpened)
        .unwrap();
    assert!(opened.payload_json.contains("https://example.invalid/pr/7"));
    assert_eq!(world.state("t1"), TaskState::Review);
}

#[test]
fn workers_keep_starting_tasks_while_the_gate_is_busy_and_the_queue_becomes_one_batch() {
    // One worker slot and one account: a worker that waited for the gate
    // would keep both, and t2..t4 could never start while t1 is gated.
    let world = World::new();

    let summary = world.once_with_queued_batch(|| {
        // The gate is stuck on t1; three more agents have started since.
        for id in ["t1", "t2", "t3"] {
            assert_eq!(
                world.state(id),
                TaskState::Gating,
                "{id}: waiting is not Blocked"
            );
        }
        assert_eq!(world.state("t4"), TaskState::Running);
        let leased = world.pool.state.lock().unwrap().leased.clone();
        assert_eq!(leased.len(), 1, "only t4's run holds the account");
    });

    assert_eq!(summary.agent_runs, 4);
    assert_eq!(summary.review.len(), 4);
    assert_eq!(
        world.gate.runs(),
        vec![strings(&["t1"]), strings(&["t2", "t3"]), strings(&["t4"])],
        "what queued behind the busy gate became one batch"
    );
    assert_eq!(summary.gate_runs, 3);
    for id in ["t1", "t2", "t3", "t4"] {
        assert_eq!(
            world.task(id).unblock_cycles,
            0,
            "{id}: no unblock cycle for waiting"
        );
    }
}

#[test]
fn a_task_waits_for_its_dependency_to_be_done() {
    let world = World::new();
    world.add("a1");
    world.add("b1");
    world.db.add_dep("b1", "a1").unwrap();

    let summary = world.once();

    // PR mode: a1 is only in Review, so b1 may not start.
    assert_eq!(summary.review, vec!["a1"]);
    assert_eq!(summary.agent_runs, 1);
    assert_eq!(world.state("b1"), TaskState::Ready);

    // The pull request gets merged: the next run sees it and releases b1.
    world.git.0.lock().unwrap().merged.push("a1".to_string());
    let summary = world.once();

    assert_eq!(world.state("a1"), TaskState::Done);
    assert_eq!(summary.done, vec!["a1"]);
    assert_eq!(summary.review, vec!["b1"]);
    let git = world.git.0.lock().unwrap();
    assert_eq!(git.fetches, 2, "one fetch at the start of each run");
    assert_eq!(
        git.cleaned,
        vec!["a1"],
        "a done task gives its worktree back"
    );
}

#[test]
fn auto_merge_lands_a_task_and_then_its_dependent_in_the_same_run() {
    let mut world = World::new();
    world.auto_merge = true;
    world.add("a1");
    world.add("b1");
    world.db.add_dep("b1", "a1").unwrap();

    let summary = world.once();

    assert_eq!(summary.done, vec!["a1", "b1"]);
    assert_eq!(world.git.0.lock().unwrap().merged, vec!["a1", "b1"]);
    // Fetched at the start and after each merge.
    assert_eq!(world.git.0.lock().unwrap().fetches, 3);
    assert_eq!(world.git.0.lock().unwrap().cleaned, vec!["a1", "b1"]);
    let events = kinds(&world, "a1");
    assert!(events.contains(&EventKind::MergeResult));
}

#[test]
fn a_clear_batch_merges_without_gating_each_member_again() {
    let mut world = World::new();
    world.auto_merge = true;

    let summary = world.once_with_queued_batch(|| {});

    assert_eq!(summary.done, vec!["t1", "t2", "t3", "t4"]);
    assert_eq!(
        world.gate.runs(),
        vec![
            // t1 rebased before gating, so one gate run covers its merge.
            strings(&["t1"]),
            // t2 + t3 were gated together on the current base: one run
            // covers both merges.
            strings(&["t2", "t3"]),
            // t4 rebased before gating, so one gate run covers its merge.
            strings(&["t4"]),
        ]
    );
    assert_eq!(summary.gate_runs, 3);
}

#[test]
fn a_batch_member_is_gated_again_when_the_base_moved_under_the_batch() {
    let mut world = World::new();
    world.auto_merge = true;
    // Somebody else pushes while t2 merges: its first attempt is "not
    // fast-forward", and the base is no longer the one the batch was gated
    // on.
    world
        .git
        .0
        .lock()
        .unwrap()
        .merge_script
        .insert("t2".to_string(), vec![MergeOutcome::NotFastForward].into());

    world.once_with_queued_batch(|| {});

    let runs = world.gate.runs();
    assert_eq!(runs[1], strings(&["t2", "t3"]));
    assert_eq!(
        runs[2],
        strings(&["t2"]),
        "the stale batch verdict is not reused"
    );
    // t3 was only ever gated on the old base together with t2: once the
    // chain is broken it is gated on what it really lands on.
    assert_eq!(runs[3], strings(&["t3"]));
    assert_eq!(runs.len(), 5, "{runs:?}");
    assert_eq!(world.state("t2"), TaskState::Done);
    assert_eq!(world.state("t3"), TaskState::Done);
}

#[test]
fn a_merge_conflict_is_rebased_once_and_gated_again() {
    let mut world = World::new();
    world.auto_merge = true;
    world.add("t1");
    world.git.0.lock().unwrap().merge_script.insert(
        "t1".to_string(),
        vec![MergeOutcome::Conflict {
            files: vec!["src/lib.rs".to_string()],
        }]
        .into(),
    );

    let summary = world.once();

    assert_eq!(summary.done, vec!["t1"]);
    // Pre-gate rebase (round 1), T2 unblocker rebase, pre-gate rebase (round 2).
    assert_eq!(world.git.0.lock().unwrap().rebases, vec!["t1", "t1", "t1"]);
    let task = world.task("t1");
    assert_eq!(task.unblock_cycles, 1);
    assert!(
        task.context_notes[0].starts_with("unblock[T2] digest="),
        "{:?}",
        task.context_notes
    );
}

#[test]
fn a_rebase_that_conflicts_again_goes_back_to_the_agent_with_the_files() {
    let mut world = World::new();
    world.auto_merge = true;
    world.add("t1");
    let files = vec!["src/lib.rs".to_string()];
    {
        let mut git = world.git.0.lock().unwrap();
        git.merge_script.insert(
            "t1".to_string(),
            vec![MergeOutcome::Conflict {
                files: files.clone(),
            }]
            .into(),
        );
        git.rebase_conflicts.insert("t1".to_string(), files);
    }

    let summary = world.once();

    assert_eq!(summary.requeued, vec!["t1"]);
    assert_eq!(world.state("t1"), TaskState::Ready);
    let notes = world.task("t1").context_notes;
    assert!(notes[0].starts_with("unblock[T2] digest="), "{notes:?}");
    assert!(notes[1].starts_with("unblock[T3] digest="), "{notes:?}");
    assert!(notes[1].contains("src/lib.rs"), "{notes:?}");
}

#[test]
fn the_cross_audit_runs_in_a_clean_tree_of_the_base_not_in_the_authors_worktree() {
    let world = World::new();
    world.add("t1");
    world
        .gate
        .state
        .lock()
        .unwrap()
        .locked
        .insert("t1".to_string(), "lockfp1".to_string());

    world.once();

    let requests = world.runner.0.lock().unwrap().requests.clone();
    let audit = requests
        .iter()
        .find(|r| r.prompt.contains(crate::cross_audit::prompt::SPEC_BEGIN))
        .expect("an audit ran");
    // The author's worktree holds the author's CLAUDE.md and .claude/
    // settings, which an agent started there would load.
    assert_ne!(audit.cwd, "/fake/wt/t1");
    assert!(audit.cwd.starts_with("/fake/wt/batch-"), "{}", audit.cwd);
    // The changes still reach the auditor, as text.
    assert!(audit.prompt.contains("+change of t1"), "{}", audit.prompt);
    // The tree was nothing but the base (no task merged) and is gone again.
    let git = world.git.0.lock().unwrap();
    assert!(!git.trees.contains_key(std::path::Path::new(&audit.cwd)));
    assert_eq!(git.integrations, git.discarded);
}

#[test]
fn locked_changes_are_cross_audited_then_gated_in_the_task_worktree() {
    let world = World::new();
    world.add("t1");
    world.add("t2");
    world
        .gate
        .state
        .lock()
        .unwrap()
        .locked
        .insert("t1".to_string(), "lockfp1".to_string());

    let summary = world.once();

    assert_eq!(world.state("t1"), TaskState::Review);
    assert_eq!(world.state("t2"), TaskState::Review);
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    assert_eq!(world.runner.count(&RunKind::Audit), 1);
    // The approval is signed for the task's own worktree, where its gate runs.
    let approval = aaf_fp::approval_path(
        world.tmp.path(),
        std::path::Path::new("/fake/wt/t1"),
        "lockfp1",
    );
    assert!(approval.is_file());
    // Every gate run that saw t1 saw it alone.
    for run in world.gate.runs() {
        assert!(
            !run.contains(&"t1".to_string()) || run.len() == 1,
            "{run:?}"
        );
    }
    let events = world.db.events_for_task("t1", 100).unwrap();
    let audit = events
        .iter()
        .find(|e| e.kind == EventKind::CrossAudit)
        .unwrap();
    // One account: the auditor is the author's account, and says so.
    assert!(audit.payload_json.contains("\"same_account\":true"));
    let runs = world.db.runs_for_task("t1").unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1].executor, "cross-audit");
    // Agent run and audit run both spend the root budget.
    assert_eq!(world.db.get_budget("t1").unwrap().tokens_used, 30);
}

#[test]
fn a_rejected_cross_audit_signs_nothing_and_returns_the_task_with_the_finding() {
    let world = World::new();
    world.add("t1");
    world
        .gate
        .state
        .lock()
        .unwrap()
        .locked
        .insert("t1".to_string(), "lockfp1".to_string());
    world.runner.0.lock().unwrap().audit_text = Some(audit_answer(&["ASSERT"]));

    let summary = world.once();

    assert_eq!(summary.requeued, vec!["t1"]);
    assert_eq!(world.state("t1"), TaskState::Ready);
    let approval = aaf_fp::approval_path(
        world.tmp.path(),
        std::path::Path::new("/fake/wt/t1"),
        "lockfp1",
    );
    assert!(!approval.exists());
    assert!(
        world.gate.runs().is_empty(),
        "nothing unapproved reaches the gate"
    );
    let notes = world.task("t1").context_notes;
    assert!(notes[0].starts_with("audit_rework:"), "{notes:?}");
    assert!(notes[0].contains("ASSERT"), "{notes:?}");
}

#[test]
fn auto_mode_asks_the_idle_hook_and_stops_when_it_says_so() {
    let world = World::new();
    world.add("t1");
    let calls = std::sync::atomic::AtomicU32::new(0);

    let summary = world.run(Mode::Auto, &|| {
        // First idle: a new task appears, as if `aaf board add` ran.
        if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            world.add("t2");
            return true;
        }
        false
    });

    assert_eq!(summary.review, vec!["t1", "t2"]);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

/// A failing harness check must fail the test, not hang it: the scoped
/// scheduler thread is still parked on the gate latch when the check panics,
/// and the scope joins it before re-raising the panic.
#[test]
fn a_failed_batch_harness_check_fails_fast_instead_of_hanging() {
    let (done_tx, done) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut world = World::new();
        world.auto_merge = true;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            world.once_with_queued_batch(|| panic!("harness check failed"))
        }));
        let _ = done_tx.send(result.is_err());
    });
    let panicked = done
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("the batch harness hung after a failed check");
    assert!(panicked, "the failed check must fail the test");
}
