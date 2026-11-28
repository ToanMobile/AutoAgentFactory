//! Temporary conditions: the lane puts the task back with `retry_after`, the
//! wait doubles from a minute to half an hour, the count survives a restart,
//! and a PR still not through after twelve retries is cancelled.

use std::sync::Arc;

use aaf_domain::event::EventKind;
use aaf_domain::ports::{AgentOutcome, InfraFailure, InfraKind, KvStore, TaskStore};
use aaf_domain::task::{Priority, Task, TaskState};

use super::support::{ID, Lab, Pr};
use crate::db::Db;
use crate::factory::adopt_lane;
use crate::factory::tests::fakes::{limit_outcome, ok_outcome, spec};

/// `(attempt, code, retry_in_secs)` of every scheduled retry, oldest first.
fn retries(lab: &Lab) -> Vec<(u64, String, i64)> {
    let mut found: Vec<_> = lab
        .events(EventKind::AdoptStep)
        .iter()
        .filter_map(|e| {
            let v: serde_json::Value = serde_json::from_str(&e.payload_json).ok()?;
            v["retry"].as_bool().filter(|r| *r)?;
            Some((
                v["attempt"].as_u64()?,
                v["code"].as_str()?.to_string(),
                v["retry_in_secs"].as_i64()?,
            ))
        })
        .collect();
    found.reverse();
    found
}

fn assert_still_adopting(lab: &Lab) {
    assert_eq!(lab.state(), TaskState::Adopting);
    assert_eq!(lab.gh.merges(), 0);
    assert!(
        lab.gh.comments().is_empty(),
        "a retry is not said on the PR"
    );
    assert!(lab.events(EventKind::AdoptAborted).is_empty());
}

#[test]
fn a_pending_check_retries_after_a_minute_then_two_then_four() {
    let lab = Lab::new(Pr::Lib);
    lab.gh.set_checks("pending");
    let mut now = 1_000;
    for (n, wait) in [(1_u64, 60_i64), (2, 120), (3, 240)] {
        lab.clock_to(now);
        let done = lab.turn();
        assert_eq!(done.error, None);
        assert_still_adopting(&lab);
        assert_eq!(lab.task().retry_after, Some(now + wait), "attempt {n}");
        assert_eq!(
            retries(&lab).last().unwrap(),
            &(n, "ci_pending".to_string(), wait)
        );
        now += wait;
    }
    assert_eq!(lab.reviewer_runs(), 0, "no review while the checks run");
}

#[test]
fn the_attempt_count_comes_from_the_events_so_it_survives_a_restart() {
    let lab = Lab::new(Pr::Lib);
    lab.gh.set_checks("pending");
    lab.turn();
    assert_eq!(retries(&lab).len(), 1);
    // A new process: a fresh handle on the same database file.
    let reopened = Db::open(&lab.world.tmp.path().join("aaf.db")).unwrap();
    let mut deps = lab.deps();
    deps.store = Arc::new(reopened);
    lab.clock_to(5_000);
    adopt_lane::run_job(&deps, &lab.router, ID);
    assert_eq!(
        retries(&lab).iter().map(|r| (r.0, r.2)).collect::<Vec<_>>(),
        [(1, 60), (2, 120)],
        "the second retry waits twice as long"
    );
    assert_eq!(lab.task().retry_after, Some(5_000 + 120));
}

#[test]
fn the_wait_stops_growing_at_half_an_hour() {
    let lab = Lab::new(Pr::Lib);
    lab.gh.set_checks("pending");
    for i in 0..8 {
        lab.clock_to(10_000 * (i + 1));
        lab.turn();
    }
    let waits: Vec<i64> = retries(&lab).iter().map(|r| r.2).collect();
    assert_eq!(waits, [60, 120, 240, 480, 960, 1800, 1800, 1800]);
}

#[test]
fn twelve_retries_then_the_thirteenth_turn_cancels_with_retry_exhausted() {
    let lab = Lab::new(Pr::Lib);
    lab.gh.set_checks("pending");
    for i in 0..12 {
        lab.clock_to(100_000 * (i + 1));
        lab.turn();
        assert_still_adopting(&lab);
    }
    assert_eq!(retries(&lab).len(), 12);
    lab.clock_to(10_000_000);
    lab.turn();
    assert_eq!(lab.state(), TaskState::Cancelled);
    assert_eq!(lab.abort_code().as_deref(), Some("retry_exhausted"));
    let comments = lab.gh.comments();
    assert_eq!(comments.len(), 1);
    assert!(comments[0].starts_with("🤖 aaf-bot:"));
    assert!(comments[0].contains("retry_exhausted") && comments[0].contains("ci_pending"));
    assert_eq!(retries(&lab).len(), 12, "no thirteenth retry");
    let fails = lab
        .world
        .db
        .get_meta(aaf_domain::ports::META_CONSECUTIVE_ROOT_FAILS)
        .unwrap();
    assert_eq!(fails, None);
}

#[test]
fn a_failed_gh_merge_retries_instead_of_cancelling() {
    let lab = Lab::new(Pr::Lib);
    *lab.gh.merge_error.lock().unwrap() = Some("gh: connection reset".to_string());
    lab.turn();
    assert_still_adopting(&lab);
    let (attempt, code, _) = retries(&lab).pop().unwrap();
    assert_eq!((attempt, code.as_str()), (1, "merge_error"));
    assert_eq!(
        lab.reviewer_runs(),
        2,
        "the review is redone on the next turn"
    );
}

#[test]
fn a_red_base_holds_the_pr_back_without_gating_it() {
    let mut lab = Lab::new(Pr::Lib);
    lab.world.repo.full_gate.cooldown_hours = 24;
    let root = lab.work.to_string_lossy().into_owned();
    let mut fixer = Task::new_root("fixer", &root, "fix the base", spec(), Priority::P1, 1);
    fixer.state = TaskState::Ready;
    lab.world.db.insert_task(&fixer).unwrap();
    let key = format!("full_gate.{}.fix_task", aaf_fp::repo_id(&lab.work));
    lab.world.db.set_meta(&key, "fixer").unwrap();
    lab.turn();
    assert_still_adopting(&lab);
    assert_eq!(retries(&lab).pop().unwrap().1, "base_red");
    let views = lab.gh.views.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(views, 0, "no gh call");
    assert_eq!(lab.commands_ran(), 0, "no gate run");
    // With the full gate off the hold ends.
    lab.world.repo.full_gate.cooldown_hours = 0;
    lab.clock_to(10_000);
    lab.turn();
    assert_eq!(lab.state(), TaskState::Done);
}

#[test]
fn a_reviewer_that_hit_its_limit_means_retry_not_review_failed() {
    let lab = Lab::new(Pr::Lib);
    lab.runners[0].fake.0.lock().unwrap().audit_outcome = Some(limit_outcome(Some(9_999)));
    lab.turn();
    assert_still_adopting(&lab);
    assert_eq!(retries(&lab).pop().unwrap().1, "reviewer_deferred");
}

#[test]
fn a_reviewer_whose_login_is_refused_means_retry_not_a_halt() {
    let lab = Lab::new(Pr::Lib);
    lab.runners[1].fake.0.lock().unwrap().audit_outcome = Some(AgentOutcome {
        exit_code: Some(1),
        infra: Some(InfraFailure {
            kind: InfraKind::Auth,
            raw: "not logged in".to_string(),
        }),
        ..ok_outcome()
    });
    lab.turn();
    assert_still_adopting(&lab);
    assert_eq!(retries(&lab).pop().unwrap().1, "reviewer_deferred");
}
