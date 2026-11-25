//! O12-1: an audit that ends without a verdict (the auditor's reply could
//! not be read, even after the format repair) is asked once more for the
//! same fingerprint before the task goes back to its implementer. A retry
//! never turns a missing verdict into a pass.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

use aaf_domain::ports::{AgentOutcome, AgentRequest, AgentRunner, PortResult};
use aaf_domain::task::TaskState;

use super::fakes::{audit_answer, ok_outcome};
use super::world::World;
use crate::cross_audit::prompt::SPEC_BEGIN;

/// Auditor replies in order; "prose" once they run out. Agents succeed.
struct Auditor(Mutex<VecDeque<String>>, Mutex<usize>);
impl AgentRunner for Auditor {
    fn run(&self, req: &AgentRequest) -> PortResult<AgentOutcome> {
        if !req.prompt.contains(SPEC_BEGIN) {
            return Ok(ok_outcome());
        }
        *self.1.lock().unwrap() += 1;
        let text = self.0.lock().unwrap().pop_front();
        Ok(AgentOutcome {
            final_text: text.unwrap_or_else(|| "prose".to_string()),
            ..ok_outcome()
        })
    }
}

fn locked_world() -> World {
    let world = World::new();
    world.add("t1");
    world
        .gate
        .state
        .lock()
        .unwrap()
        .locked
        .insert("t1".to_string(), "lockfp1".to_string());
    world
}

fn run_once(world: &World, auditor: &Arc<Auditor>) -> crate::factory::RunSummary {
    let mut deps = world.deps();
    deps.runner = auditor.clone();
    let hooks = crate::factory::Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    crate::factory::run(&deps, crate::factory::Mode::Once, &hooks).unwrap()
}

#[test]
fn an_inconclusive_audit_is_asked_again_and_its_pass_approves() {
    let world = locked_world();
    // First audit job: reply and format repair both unreadable. The retry
    // reads a clean answer.
    let replies = ["prose".to_string(), "prose".to_string(), audit_answer(&[])];
    let auditor = Arc::new(Auditor(Mutex::new(replies.into()), Mutex::new(0)));

    let summary = run_once(&world, &auditor);

    assert_eq!(world.state("t1"), TaskState::Review, "{summary:?}");
    assert_eq!(*auditor.1.lock().unwrap(), 3);
    assert!(world.task("t1").context_notes.is_empty());
}

#[test]
fn the_retry_is_once_per_fingerprint_and_never_a_pass() {
    let world = locked_world();
    let auditor = Arc::new(Auditor(Mutex::new(VecDeque::new()), Mutex::new(0)));

    let first = run_once(&world, &auditor);

    // Two audit jobs of three turns each (initial + up to 2 inconclusive retries).
    assert_eq!(*auditor.1.lock().unwrap(), 6);
    // RFC-032: a verdict the auditor never gave is not the implementer's
    // fault: no rework round (no T3/T4 burn), no freeze count. The task
    // waits as an infra block and comes back on its own (RFC-033).
    assert!(first.requeued.is_empty(), "{first:?}");
    let task = world.task("t1");
    assert_eq!(task.state, TaskState::Blocked, "{first:?}");
    assert!(task.retry_after.is_some(), "{task:?}");
    assert!(task.context_notes.is_empty(), "no rework note: {task:?}");
    assert_eq!(world.notifier.0.lock().unwrap().len(), 0);
    assert!(
        !aaf_fp::approval_path(
            world.tmp.path(),
            std::path::Path::new("/fake/wt/t1"),
            "lockfp1"
        )
        .exists()
    );
}

#[test]
fn no_retry_once_the_root_budget_is_spent() {
    let mut world = locked_world();
    // 15 tokens a run: the agent run (15) and the format repair (projected
    // 30) fit; after the audit job (45) the root has nothing left to retry on.
    world.repo.unblocker.max_token_budget = 30;
    let auditor = Arc::new(Auditor(Mutex::new(VecDeque::new()), Mutex::new(0)));

    let summary = run_once(&world, &auditor);

    assert_eq!(*auditor.1.lock().unwrap(), 2, "turn + repair, no retry");
    // Not approved; the spent budget sends it to a human, not back to work.
    assert_eq!(world.state("t1"), TaskState::Blocked, "{summary:?}");
    assert_eq!(summary.blocked, vec!["t1"], "{summary:?}");
    assert!(
        !aaf_fp::approval_path(
            world.tmp.path(),
            std::path::Path::new("/fake/wt/t1"),
            "lockfp1"
        )
        .exists()
    );
}

/// A well-formed reply answering "yes" to STUB (blocking) with `evidence`.
fn stub_yes(evidence: &str) -> String {
    let answers: Vec<serde_json::Value> =
        crate::cross_audit::build_checklist(&super::fakes::spec(), false)
            .iter()
            .map(|item| {
                let hit = item.id == crate::cross_audit::checklist::ID_STUB;
                serde_json::json!({
                    "id": item.id,
                    "answer": if hit { "yes" } else { "no" },
                    "evidence": if hit { evidence } else { "" },
                })
            })
            .collect();
    serde_json::json!({ "answers": answers }).to_string()
}

fn approved(world: &World) -> bool {
    let wt = std::path::Path::new("/fake/wt/t1");
    aaf_fp::approval_path(world.tmp.path(), wt, "lockfp1").exists()
}

/// R10-L3-1: a blocking "yes" without file:line evidence is Inconclusive
/// (G2), but the auditor did find a blocker. The format repair may cite it;
/// it must not turn it into a signed pass by answering "no" everywhere.
#[test]
fn a_repair_never_turns_an_unbacked_blocking_yes_into_a_pass() {
    for evidence in ["the new function is a stub", "src/nowhere.rs:999"] {
        let world = locked_world();
        let replies = [stub_yes(evidence), audit_answer(&[])];
        let auditor = Arc::new(Auditor(Mutex::new(replies.into()), Mutex::new(0)));

        let summary = run_once(&world, &auditor);

        assert!(!approved(&world), "{evidence}: signed a pass: {summary:?}");
        assert_ne!(world.state("t1"), TaskState::Review, "{evidence}");
    }
}

/// The repair may still cite the blocker it named: that Fail is accepted
/// and goes back to the implementer as a finding.
#[test]
fn a_repair_that_cites_the_named_blocker_is_accepted() {
    let world = locked_world();
    let replies = [
        stub_yes("the new function is a stub"),
        audit_answer(&["STUB"]),
    ];
    let auditor = Arc::new(Auditor(Mutex::new(replies.into()), Mutex::new(0)));

    let summary = run_once(&world, &auditor);

    assert!(!approved(&world), "{summary:?}");
    let notes = world.task("t1").context_notes.join("\n");
    assert!(notes.contains("src/lib.rs:2"), "{notes}\n{summary:?}");
}
