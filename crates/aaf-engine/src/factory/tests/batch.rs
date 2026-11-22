//! The batched gate: how many gate runs a batch costs and who is blamed.

use std::sync::{Arc, Mutex};

use super::fakes::{FakeGate, FakeGit, GateState};
use crate::factory::batch::{BatchOutcome, Member, Verdict, gate_batch};
use crate::factory::ports::GitOps;

fn setup() -> (Arc<FakeGit>, FakeGate) {
    let git = Arc::new(FakeGit::default());
    let gate = FakeGate {
        git: Arc::clone(&git),
        data_dir: "/fake/data".into(),
        state: Mutex::new(GateState::default()),
    };
    (git, gate)
}

fn members(git: &FakeGit, count: usize) -> Vec<Member> {
    (1..=count)
        .map(|n| {
            let task_id = format!("t{n}");
            Member {
                worktree: git.task_worktree(&task_id).unwrap(),
                task_id,
            }
        })
        .collect()
}

fn ids_where(outcome: &BatchOutcome, pick: fn(&Verdict) -> bool) -> Vec<String> {
    let mut ids: Vec<String> = outcome
        .verdicts
        .iter()
        .filter(|j| pick(&j.verdict))
        .map(|j| j.task_id.clone())
        .collect();
    ids.sort();
    ids
}

fn clear(outcome: &BatchOutcome) -> Vec<String> {
    ids_where(outcome, |v| matches!(v, Verdict::Clear(_)))
}

fn red(outcome: &BatchOutcome) -> Vec<String> {
    ids_where(outcome, |v| matches!(v, Verdict::Red(_)))
}

#[test]
fn ten_green_tasks_cost_exactly_one_gate_run() {
    let (git, gate) = setup();
    let batch = members(&git, 10);

    let outcome = gate_batch(&*git, &gate, &batch);

    assert_eq!(outcome.gate_runs, 1);
    assert_eq!(gate.runs().len(), 1);
    assert_eq!(gate.runs()[0].len(), 10, "one run over all ten branches");
    assert_eq!(clear(&outcome).len(), 10);
    assert!(outcome.verdicts.iter().all(|j| j.batch_group.is_some()));
    let state = git.0.lock().unwrap();
    assert_eq!((state.integrations, state.discarded), (1, 1));
}

#[test]
fn one_red_among_eight_is_isolated_within_the_log_bound() {
    for culprit in 1..=8 {
        let (git, gate) = setup();
        let batch = members(&git, 8);
        let culprit = format!("t{culprit}");
        gate.state.lock().unwrap().red.insert(culprit.clone());

        let outcome = gate_batch(&*git, &gate, &batch);

        // 1 + 2 * log2(8)
        assert!(
            outcome.gate_runs <= 7,
            "{culprit}: {} runs",
            outcome.gate_runs
        );
        assert_eq!(outcome.gate_runs as usize, gate.runs().len());
        assert_eq!(red(&outcome), vec![culprit.clone()]);
        assert_eq!(clear(&outcome).len(), 7, "{culprit}");
        // The culprit's verdict is its own worktree's receipt.
        let own = outcome
            .verdicts
            .iter()
            .find(|j| j.task_id == culprit)
            .unwrap();
        assert!(own.batch_group.is_none());
        let Verdict::Red(receipt) = &own.verdict else {
            panic!("not red");
        };
        assert_eq!(receipt.repo_root, format!("/fake/wt/{culprit}"));
    }
}

#[test]
fn a_batch_of_one_gates_the_task_worktree_directly() {
    let (git, gate) = setup();
    let batch = members(&git, 1);

    let outcome = gate_batch(&*git, &gate, &batch);

    assert_eq!(outcome.gate_runs, 1);
    assert_eq!(git.0.lock().unwrap().integrations, 0);
    assert!(outcome.verdicts[0].batch_group.is_none());
    assert_eq!(clear(&outcome), vec!["t1"]);
}

#[test]
fn a_conflicting_branch_is_gated_alone_and_does_not_sink_the_batch() {
    let (git, gate) = setup();
    let batch = members(&git, 4);
    git.0.lock().unwrap().conflicts.insert("t3".to_string());

    let outcome = gate_batch(&*git, &gate, &batch);

    assert_eq!(clear(&outcome), vec!["t1", "t2", "t3", "t4"]);
    assert_eq!(
        gate.runs(),
        vec![
            vec!["t1".to_string(), "t2".to_string(), "t4".to_string()],
            vec!["t3".to_string()],
        ]
    );
    let alone = outcome.verdicts.iter().find(|j| j.task_id == "t3").unwrap();
    assert!(alone.batch_group.is_none());
}

#[test]
fn a_red_conflicting_branch_is_blamed_alone() {
    let (git, gate) = setup();
    let batch = members(&git, 3);
    git.0.lock().unwrap().conflicts.insert("t2".to_string());
    gate.state.lock().unwrap().red.insert("t2".to_string());

    let outcome = gate_batch(&*git, &gate, &batch);

    assert_eq!(outcome.gate_runs, 2);
    assert_eq!(red(&outcome), vec!["t2"]);
    assert_eq!(clear(&outcome), vec!["t1", "t3"]);
}

#[test]
fn two_red_tasks_are_both_found() {
    let (git, gate) = setup();
    let batch = members(&git, 8);
    {
        let mut state = gate.state.lock().unwrap();
        state.red.insert("t2".to_string());
        state.red.insert("t7".to_string());
    }

    let outcome = gate_batch(&*git, &gate, &batch);

    assert_eq!(red(&outcome), vec!["t2", "t7"]);
    assert_eq!(clear(&outcome).len(), 6);
    assert!(outcome.gate_runs < 16, "cheaper than gating all 8 twice");
}

#[test]
fn halves_that_only_fail_together_blame_the_second_half() {
    let (git, gate) = setup();
    let batch = members(&git, 4);
    gate.state
        .lock()
        .unwrap()
        .red_pairs
        .push(("t1".to_string(), "t3".to_string()));

    let outcome = gate_batch(&*git, &gate, &batch);

    // [t1..t4] red, [t1,t2] clear, [t3,t4] clear.
    assert_eq!(outcome.gate_runs, 3);
    assert_eq!(clear(&outcome), vec!["t1", "t2"]);
    assert_eq!(red(&outcome), vec!["t3", "t4"]);
    let Verdict::Red(receipt) = &outcome
        .verdicts
        .iter()
        .find(|j| j.task_id == "t3")
        .unwrap()
        .verdict
    else {
        panic!("not red");
    };
    assert_eq!(receipt.reasons[0], "fails only in combination with: t1, t2");
}

#[test]
fn an_unverified_batch_is_not_bisected() {
    let (git, gate) = setup();
    let batch = members(&git, 4);
    gate.state.lock().unwrap().unverified = true;

    let outcome = gate_batch(&*git, &gate, &batch);

    assert_eq!(outcome.gate_runs, 1);
    assert!(
        outcome
            .verdicts
            .iter()
            .all(|j| matches!(j.verdict, Verdict::Unverified(_)))
    );
}

#[test]
fn without_an_integration_tree_every_task_is_gated_alone() {
    let (git, gate) = setup();
    let batch = members(&git, 3);
    git.0.lock().unwrap().fail_integrate = true;

    let outcome = gate_batch(&*git, &gate, &batch);

    assert_eq!(outcome.gate_runs, 3);
    assert_eq!(clear(&outcome), vec!["t1", "t2", "t3"]);
    assert!(outcome.groups.is_empty());
}

fn group_of(outcome: &BatchOutcome, task_id: &str) -> Option<u32> {
    let judged = outcome.verdicts.iter().find(|j| j.task_id == task_id);
    judged.unwrap().batch_group
}

#[test]
fn green_halves_of_a_bisected_batch_are_groups_of_their_own() {
    let (git, gate) = setup();
    let batch = members(&git, 5);
    gate.state.lock().unwrap().red.insert("t3".to_string());

    let outcome = gate_batch(&*git, &gate, &batch);

    // [t1, t2] and [t4, t5] each passed in a tree without the other.
    let (left, right) = (group_of(&outcome, "t1"), group_of(&outcome, "t4"));
    assert!(left.is_some() && right.is_some());
    assert_ne!(left, right);
    assert_eq!(group_of(&outcome, "t2"), left);
    assert_eq!(group_of(&outcome, "t5"), right);
    assert_eq!(group_of(&outcome, "t3"), None);
    let group = &outcome.groups[&right.unwrap()];
    assert_eq!(group.branches, vec!["aaf/t4", "aaf/t5"]);
    assert_eq!(group.base.as_deref(), Some("base0"));
}

#[test]
fn a_base_that_moves_while_the_integration_tree_is_built_is_not_recorded() {
    let (git, gate) = setup();
    let batch = members(&git, 3);
    git.0.lock().unwrap().integrate_moves_base = true;

    let outcome = gate_batch(&*git, &gate, &batch);

    assert_eq!(clear(&outcome).len(), 3);
    let group = &outcome.groups[&group_of(&outcome, "t1").unwrap()];
    // Nobody can tell which tip the tree was cut from: no merge may rely
    // on it.
    assert_eq!(group.base, None);
}
