//! Tests for `sweep_review` merged-PR fallback when a `Review` task has a
//! recorded `PrOpened` URL but no local worktree or non-`aaf/<id>` head branch.

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{EventSink, TaskStore};
use aaf_domain::task::TaskState;
use serde_json::json;

use super::world::World;
use crate::git::pr_merged_gh::VerifiedPrMerge;

const PR_URL: &str = "https://github.com/acme/app/pull/455";
const HEAD_SHA: &str = "1111111111111111111111111111111111111111";
const MERGE_SHA: &str = "2222222222222222222222222222222222222222";

fn setup_review_with_pr(world: &World, id: &str, url: &str) {
    world.add(id);
    for (state, ts) in [
        (TaskState::Running, 100),
        (TaskState::Gating, 110),
        (TaskState::Review, 120),
    ] {
        world.db.set_state(id, state, None, ts).unwrap();
    }
    let ev =
        FactoryEvent::new(EventKind::PrOpened, json!({ "url": url }).to_string(), 125).for_task(id);
    world.db.emit(&ev).unwrap();
}

#[test]
fn sweep_review_completes_merged_pr_without_local_worktree() {
    let world = World::new();
    setup_review_with_pr(&world, "t-pr455", PR_URL);
    world.git.0.lock().unwrap().verified_merged_pr.insert(
        455,
        Ok(Some(VerifiedPrMerge {
            pr_number: 455,
            url: PR_URL.to_string(),
            head_oid: HEAD_SHA.to_string(),
            merge_commit_oid: MERGE_SHA.to_string(),
        })),
    );

    let summary = world.once();
    assert_eq!(world.state("t-pr455"), TaskState::Done);
    assert!(summary.done.iter().any(|id| id == "t-pr455"));

    let merge_events = world
        .db
        .events_for_task_by_kinds("t-pr455", &[EventKind::MergeResult], 10)
        .unwrap();
    assert_eq!(merge_events.len(), 1);
    let payload: serde_json::Value = serde_json::from_str(&merge_events[0].payload_json).unwrap();
    assert_eq!(payload["merged"], true);
    assert_eq!(payload["kind"], "merged_pr");
    assert_eq!(payload["pr_number"], 455);
    assert_eq!(payload["merge_commit_oid"], MERGE_SHA);
}

#[test]
fn sweep_review_keeps_task_in_review_when_pr_open_or_gh_errors() {
    let world = World::new();
    setup_review_with_pr(&world, "t-open", "https://github.com/acme/app/pull/456");
    setup_review_with_pr(&world, "t-err", "https://github.com/acme/app/pull/457");
    {
        let mut git = world.git.0.lock().unwrap();
        git.verified_merged_pr.insert(456, Ok(None));
        git.verified_merged_pr
            .insert(457, Err("gh offline".to_string()));
    }

    let summary = world.once();
    assert_eq!(world.state("t-open"), TaskState::Review);
    assert_eq!(world.state("t-err"), TaskState::Review);
    assert!(
        summary
            .errors
            .iter()
            .any(|e| e.contains("t-err") && e.contains("pr_merge_sweep")),
        "expected pr_merge_sweep warning, got {:?}",
        summary.errors
    );
}
