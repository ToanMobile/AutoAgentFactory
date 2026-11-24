//! Behavioral tests for `FactoryHalted` emission/dedup and the secondary
//! auditor auth-wait ceiling (T3b).

use std::sync::atomic::Ordering;
use std::sync::{Arc, Barrier, Mutex};

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::blocked::BlockedReason;
use aaf_domain::event::FactoryEvent;
use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, EventSink, InfraFailure, InfraKind, KvStore,
    META_CONSECUTIVE_ROOT_FAILS, PortResult,
};
use aaf_domain::task::TaskState;

use super::fakes::{FakePool, failed_outcome, ok_outcome};
use super::world::World;
use crate::cross_audit::AuditVerdict;
use crate::cross_audit::prompt::SPEC_BEGIN;
use crate::factory::audit_backend::{self, AuditBackend};
use crate::factory::{Hooks, Mode, run_with_audit, unblock};

fn auth_fail(raw: &str) -> AgentOutcome {
    AgentOutcome {
        infra: Some(InfraFailure {
            kind: InfraKind::Auth,
            raw: raw.to_string(),
        }),
        tokens_in: 0,
        tokens_out: 0,
        ..failed_outcome(raw)
    }
}

fn halt_events_for(world: &World, task_id: &str) -> Vec<FactoryEvent> {
    world
        .db
        .events_for_task(task_id, 50)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind.as_str() == "factory_halted")
        .collect()
}

/// Case 1: A primary auth halt emits exactly one `factory_halted` event attached
/// to the task, carrying `why` and a 16-hex `digest`. Parallel workers halting in
/// the same run only emit one `factory_halted` event for that run.
#[test]
fn halt_emits_single_factory_halted_event_with_why_and_digest() {
    let world = World::new();
    world.add("t1");
    world
        .runner
        .script("t1", vec![auth_fail("Not logged in · Please run /login")]);

    let summary = world.once();
    assert!(summary.halted.is_some(), "{summary:?}");

    let events = halt_events_for(&world, "t1");
    assert_eq!(
        events.len(),
        1,
        "expected 1 factory_halted event: {events:?}"
    );
    assert_eq!(events[0].task_id.as_deref(), Some("t1"));
    let payload: serde_json::Value = serde_json::from_str(&events[0].payload_json).unwrap();
    let why = payload["why"].as_str().unwrap_or("");
    assert!(why.contains("Not logged in"), "payload={payload}");
    let digest = payload["digest"].as_str().unwrap_or("");
    assert_eq!(digest.len(), 16, "digest must be 16 hex chars: {payload}");
    assert!(
        digest.chars().all(|c| c.is_ascii_hexdigit()),
        "digest must be hex: {digest}"
    );

    // Parallel workers halting in the same `aaf run` emit at most one event.
    struct BarrierAuthFail(Arc<Barrier>);
    impl AgentRunner for BarrierAuthFail {
        fn run(&self, _req: &AgentRequest) -> PortResult<AgentOutcome> {
            self.0.wait();
            Ok(auth_fail("Not logged in on parallel worker"))
        }
    }
    let mut par = World::with_accounts(&["a1", "a2"]);
    par.max_parallel = 2;
    par.add("p1");
    par.add("p2");
    let mut deps = par.deps();
    deps.runner = Arc::new(BarrierAuthFail(Arc::new(Barrier::new(2))));
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    let par_summary = crate::factory::run(&deps, Mode::Once, &hooks).unwrap();
    assert!(par_summary.halted.is_some(), "{par_summary:?}");
    let total = halt_events_for(&par, "p1").len() + halt_events_for(&par, "p2").len();
    assert_eq!(total, 1, "parallel halts in one run must emit 1 event");
}

/// Case 2: Same halt reason (differing only in numbers) within 6h is deduplicated;
/// after 6h it emits again; a different reason emits immediately.
#[test]
fn halt_dedups_same_reason_within_six_hours_and_emits_after_or_on_new_reason() {
    let world = World::new();
    world.clock.0.store(10_000, Ordering::SeqCst);
    world.add("t1");

    // 1st halt at t = 10_000.
    world
        .runner
        .script("t1", vec![auth_fail("Not logged in (HTTP 401, req 111)")]);
    assert!(world.once().halted.is_some());
    assert_eq!(halt_events_for(&world, "t1").len(), 1);

    // 2nd halt at t = 13_600 (within 6h), same reason with different numbers -> deduped.
    world.clock.0.store(13_600, Ordering::SeqCst);
    world
        .runner
        .script("t1", vec![auth_fail("Not logged in (HTTP 401, req 999)")]);
    assert!(world.once().halted.is_some());
    assert_eq!(
        halt_events_for(&world, "t1").len(),
        1,
        "same digest within 6h must not emit a duplicate"
    );

    // 3rd halt after 6h (10_000 + 21_600 + 1) -> emits again.
    world.clock.0.store(10_000 + 6 * 3600 + 1, Ordering::SeqCst);
    world
        .runner
        .script("t1", vec![auth_fail("Not logged in (HTTP 401, req 222)")]);
    assert!(world.once().halted.is_some());
    assert_eq!(
        halt_events_for(&world, "t1").len(),
        2,
        "same digest after 6h must emit"
    );

    // 4th halt immediately with a different reason -> emits right away.
    world
        .runner
        .script("t1", vec![auth_fail("Invalid API key revoked by org")]);
    assert!(world.once().halted.is_some());
    assert_eq!(
        halt_events_for(&world, "t1").len(),
        3,
        "different reason within 6h must emit"
    );
}

/// Case 3: Secondary auditor auth failure (head-lock audit via `audit_infra_next`):
/// waits on attempts 1 and 2, blocks with `NeedsHuman` on attempt 3 without
/// bumping global freeze or polluting `audit_fails_since_reset` / `retry_inconclusive`.
#[test]
fn secondary_auditor_auth_waits_twice_then_blocks_untried_without_affecting_audit_counters() {
    struct AlwaysAuthFailAuditor;
    impl AgentRunner for AlwaysAuthFailAuditor {
        fn run(&self, req: &AgentRequest) -> PortResult<AgentOutcome> {
            if req.prompt.contains(SPEC_BEGIN) {
                Ok(auth_fail("codex: not logged in"))
            } else {
                Ok(ok_outcome())
            }
        }
    }

    let mut world = World::new();
    world.repo.run.executor = "claude".to_string();
    world.repo.cross_audit.executor = "codex".to_string();
    world.repo.cross_audit.require_different_provider = false;
    world.add("t1");
    world
        .gate
        .state
        .lock()
        .unwrap()
        .locked
        .insert("t1".to_string(), "lockfp1".to_string());

    let audit_pool = Arc::new(FakePool {
        accounts: vec![AccountProfile {
            id: "codex-1".to_string(),
            provider: ProviderKind::Codex,
            config_dir: "/fake/home/.codex-1".to_string(),
            name: "codex-1".to_string(),
        }],
        clock: Arc::clone(&world.clock),
        state: Arc::new(Mutex::default()),
    });
    let audit = AuditBackend {
        provider: "codex".to_string(),
        runner: Arc::new(AlwaysAuthFailAuditor),
        accounts: audit_pool,
        hook_required: false,
    };
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };

    // Attempt 1: waits (task stays Gating, not Blocked, not Halted).
    let s1 = run_with_audit(&world.deps(), Some(&audit), Mode::Once, &hooks).unwrap();
    assert_eq!(s1.halted, None, "{s1:?}");
    assert!(s1.blocked.is_empty(), "{s1:?}");
    assert_eq!(world.state("t1"), TaskState::Gating);

    // Attempt 2: still waits.
    world.clock.0.fetch_add(20, Ordering::SeqCst);
    let s2 = run_with_audit(&world.deps(), Some(&audit), Mode::Once, &hooks).unwrap();
    assert_eq!(s2.halted, None, "{s2:?}");
    assert!(s2.blocked.is_empty(), "{s2:?}");
    assert_eq!(world.state("t1"), TaskState::Gating);

    // Attempt 3: hits ceiling -> Blocked (NeedsHuman), no freeze increment.
    world.clock.0.fetch_add(20, Ordering::SeqCst);
    let s3 = run_with_audit(&world.deps(), Some(&audit), Mode::Once, &hooks).unwrap();
    assert_eq!(s3.halted, None, "{s3:?}");
    assert_eq!(s3.blocked, vec!["t1"], "{s3:?}");
    let task = world.task("t1");
    assert_eq!(task.state, TaskState::Blocked);
    match &task.blocked_reason {
        Some(BlockedReason::NeedsHuman { why }) => {
            assert!(why.contains("codex"), "why={why}");
            assert!(why.contains("log in"), "why={why}");
            assert!(why.contains("aaf board unblock t1"), "why={why}");
        }
        other => panic!("expected NeedsHuman, got {other:?}"),
    }
    assert_eq!(
        world.db.get_meta(META_CONSECUTIVE_ROOT_FAILS).unwrap(),
        None,
        "secondary auditor auth block must not count toward global freeze"
    );

    // V2-1: `auth_wait` CrossAudit events must not affect `audit_fails_since_reset`
    // or `retry_inconclusive`.
    let deps = world.deps();
    assert_eq!(unblock::audit_fails_since_reset(&deps, "t1").unwrap(), 0);
    let can_retry = audit_backend::retry_inconclusive(
        &deps,
        &task,
        (
            &AuditVerdict::Inconclusive {
                reason: "prose".to_string(),
            },
            false,
        ),
        "lock_fp",
        "lockfp1",
    )
    .unwrap();
    assert!(
        can_retry,
        "auth_wait events must not consume inconclusive audit retry"
    );
}
