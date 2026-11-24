//! One gate round with `--auto-merge`: which merges may reuse a batch
//! verdict and which need their own gate run. Driven through
//! `scheduler::judge_round`, so the batch is exactly the one the test names.

use std::sync::Mutex;
use std::sync::atomic::Ordering;

use aaf_domain::ports::TaskStore;
use aaf_domain::task::{Priority, Task, TaskState};

use super::fakes::{REPO, spec};
use super::world::World;
use crate::factory::finish::Resolution;
use crate::factory::ports::GitOps;
use crate::factory::scheduler::{GateStats, judge_round};
use crate::factory::worker::GateJob;
use crate::git::MergeOutcome;

struct Round {
    /// `task:outcome` in the order the round answered.
    answers: Vec<String>,
    gate_runs: u32,
}

fn strings(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| (*id).to_string()).collect()
}

fn auto_merge_world() -> World {
    let mut world = World::new();
    world.auto_merge = true;
    world
}

/// Puts t1..t<count> at the gate and runs one round over all of them.
fn round(world: &World, count: usize) -> Round {
    let jobs: Vec<GateJob> = (1..=count)
        .map(|n| {
            let id = format!("t{n}");
            let mut task = Task::new_root(&id, REPO, format!("task {id}"), spec(), Priority::P2, 1);
            task.state = TaskState::Gating;
            world.db.insert_task(&task).unwrap();
            GateJob {
                worktree: world.git.task_worktree(&id).unwrap(),
                task_id: id,
                audited: false,
                cached_receipt: None,
            }
        })
        .collect();
    let stats = GateStats::default();
    let answers = Mutex::new(Vec::new());
    judge_round(&world.deps(), jobs, &stats, &|job, resolution| {
        let outcome = match resolution {
            Ok(Resolution::Settled(state)) => state.as_str().to_string(),
            Ok(Resolution::NeedsAudit { .. }) => "needs-audit".to_string(),
            Ok(Resolution::NeedsPostAudit { .. }) => "needs-post-audit".to_string(),
            Ok(Resolution::Failed(signal)) if signal.gate.is_some() => "gate-failed".to_string(),
            Ok(Resolution::Failed(_)) => "failed".to_string(),
            Ok(Resolution::Retry) => "retry".to_string(),
            Err(error) => format!("error: {error}"),
        };
        answers
            .lock()
            .unwrap()
            .push(format!("{}:{outcome}", job.task_id));
    });
    assert!(stats.warnings.lock().unwrap().is_empty());
    Round {
        answers: answers.into_inner().unwrap(),
        gate_runs: stats.runs.load(Ordering::Relaxed),
    }
}

fn merged(world: &World) -> Vec<String> {
    world.git.0.lock().unwrap().merged.clone()
}

#[test]
fn a_green_batch_of_five_merges_all_five_with_exactly_one_gate_run() {
    let world = auto_merge_world();

    let round = round(&world, 5);

    assert_eq!(merged(&world), strings(&["t1", "t2", "t3", "t4", "t5"]));
    assert_eq!(round.gate_runs, 1);
    assert_eq!(
        world.gate.runs(),
        vec![strings(&["t1", "t2", "t3", "t4", "t5"])]
    );
    assert!(
        round.answers.iter().all(|a| a.ends_with(":done")),
        "{:?}",
        round.answers
    );
}

#[test]
fn a_green_sub_batch_is_gated_again_before_it_lands_on_another_sub_batch() {
    let world = auto_merge_world();
    {
        let mut gate = world.gate.state.lock().unwrap();
        gate.red.insert("t3".to_string());
        gate.red_pairs.push(("t1".to_string(), "t4".to_string()));
    }

    let round = round(&world, 5);

    let runs = world.gate.runs();
    assert_eq!(
        runs[..5],
        [
            strings(&["t1", "t2", "t3", "t4", "t5"]),
            strings(&["t1", "t2"]),
            strings(&["t3", "t4", "t5"]),
            strings(&["t3"]),
            strings(&["t4", "t5"]),
        ],
        "the bisection"
    );
    // [t1, t2] and [t4, t5] were each gated green, but never together:
    // t4 lands on a base that already holds t1, and only its own gate run
    // on that base can tell.
    assert_eq!(
        runs.get(5),
        Some(&strings(&["t4"])),
        "a gate run before t4 merges: {runs:?}"
    );
    assert!(
        !merged(&world).contains(&"t4".to_string()),
        "t1 + t4 is red"
    );
    assert!(
        round.answers.contains(&"t4:gate-failed".to_string()),
        "{:?}",
        round.answers
    );
    // t5 was only gated next to t4: it needs its own run as well.
    assert_eq!(runs.get(6), Some(&strings(&["t5"])), "{runs:?}");
    assert_eq!(merged(&world), strings(&["t1", "t2", "t5"]));
    assert_eq!(
        world.state("t4"),
        TaskState::Gating,
        "left to the unblocker"
    );
    assert_eq!((runs.len(), round.gate_runs), (7, 7));
}

#[test]
fn a_member_that_fails_to_merge_ends_the_chain_for_the_members_after_it() {
    let world = auto_merge_world();
    world.git.0.lock().unwrap().merge_script.insert(
        "t2".to_string(),
        vec![MergeOutcome::Conflict {
            files: vec!["src/lib.rs".to_string()],
        }]
        .into(),
    );

    let round = round(&world, 4);

    // The gated tree held t2. Without it, base + t1 + t3 is a tree no gate
    // run ever saw.
    assert_eq!(
        world.gate.runs(),
        vec![
            strings(&["t1", "t2", "t3", "t4"]),
            strings(&["t3"]),
            strings(&["t4"]),
        ]
    );
    assert_eq!(round.gate_runs, 3);
    assert_eq!(merged(&world), strings(&["t1", "t3", "t4"]));
    assert!(
        round.answers.contains(&"t2:failed".to_string()),
        "{:?}",
        round.answers
    );
}

#[test]
fn an_outside_commit_in_the_middle_of_the_chain_sends_the_rest_through_the_gate() {
    let world = auto_merge_world();
    world
        .git
        .0
        .lock()
        .unwrap()
        .external_before
        .insert("t3".to_string());

    let round = round(&world, 5);

    assert_eq!(
        world.gate.runs()[1..],
        [strings(&["t3"]), strings(&["t4"]), strings(&["t5"])]
    );
    assert_eq!(round.gate_runs, 4);
    assert_eq!(merged(&world).len(), 5);
}

#[test]
fn a_batch_gated_on_a_base_that_is_no_longer_current_is_gated_member_by_member() {
    let world = auto_merge_world();
    // The base moves after the integration tree was gated and before the
    // first member merges.
    world
        .git
        .0
        .lock()
        .unwrap()
        .external_before
        .insert("t1".to_string());

    let round = round(&world, 3);

    assert_eq!(
        world.gate.runs(),
        vec![
            strings(&["t1", "t2", "t3"]),
            strings(&["t1"]),
            strings(&["t2"]),
            strings(&["t3"]),
        ]
    );
    assert_eq!(round.gate_runs, 4);
    assert_eq!(merged(&world).len(), 3);
}

fn pr_notes(world: &World) -> Vec<(String, Option<String>)> {
    world.git.0.lock().unwrap().pr_notes.clone()
}

#[test]
fn a_pull_request_gated_in_a_batch_says_whose_receipt_it_carries() {
    let world = World::new();
    // [t1, t2] is gated green as one integration tree; t3 and t4 end up
    // gated alone.
    world
        .gate
        .state
        .lock()
        .unwrap()
        .red
        .insert("t4".to_string());

    round(&world, 4);

    let notes = pr_notes(&world);
    assert_eq!(notes.len(), 3, "{notes:?}");
    for (task, note) in &notes {
        if task == "t3" {
            assert_eq!(*note, None, "its own worktree was gated");
            continue;
        }
        let Some(note) = note else {
            panic!("{task}: a batch receipt presented as the branch's own");
        };
        assert!(note.contains("Batch gate receipt"), "{note}");
        assert!(note.contains("- `aaf/t1`\n- `aaf/t2`\n"), "{note}");
        assert!(
            !note.contains("aaf/t3") && !note.contains("aaf/t4"),
            "{note}"
        );
        assert!(
            note.contains("`tree_fp`")
                && note.contains("integration tree")
                && note.contains("not of this branch"),
            "{note}"
        );
    }
}

#[test]
fn a_pull_request_of_a_task_gated_alone_keeps_the_plain_wording() {
    let world = World::new();

    round(&world, 1);

    assert_eq!(pr_notes(&world), vec![("t1".to_string(), None)]);
}

#[test]
fn a_batch_of_one_is_gated_again_on_the_base_it_lands_on() {
    let world = auto_merge_world();

    let round = round(&world, 1);

    // Its own worktree was gated on the base it was cut from.
    assert_eq!(world.gate.runs(), vec![strings(&["t1"]), strings(&["t1"])]);
    assert_eq!(round.gate_runs, 2);
    assert_eq!(merged(&world), strings(&["t1"]));
}
