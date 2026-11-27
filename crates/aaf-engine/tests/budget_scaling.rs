#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for multi-subtask `token_limit` + `wall_secs_limit` scaling
//! and fail-closed exhaustion guards across DB sync, split, and ensure paths.

use std::sync::atomic::{AtomicUsize, Ordering};

use aaf_domain::budget::{BudgetCheck, RootBudget};
use aaf_domain::config::UnblockerConfig;
use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, BudgetStore, PortResult, SandboxPolicy, TaskStore,
};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::split::{SplitSubtask, apply_split, propose_and_apply_split};

fn spec() -> ImmutableSpec {
    ImmutableSpec {
        description: "desc".into(),
        acceptance: vec!["passes".into()],
    }
}

fn root_task(id: &str, repo: &str, state: TaskState) -> Task {
    let mut t = Task::new_root(id, repo, id, spec(), Priority::P1, 1_000);
    t.state = state;
    t
}

#[test]
fn sync_repo_budget_limits_never_unexhausts_exceeded_unfrozen_roots() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    let repo = "/repo/fail-closed";

    let base_cfg = UnblockerConfig {
        max_token_budget: 2_000_000,
        max_wall_clock_secs: 10_800,
        max_unblock_cycles: 5,
        ..Default::default()
    };

    // 1. Token-exceeded unfrozen root (no subtasks): restart sync must NOT ratchet limit to usage.
    db.insert_task(&root_task("t-tok", repo, TaskState::Ready))
        .unwrap();
    let mut b_tok = RootBudget::new("t-tok", &base_cfg);
    b_tok.tokens_used = 2_500_000;
    db.put_budget(&b_tok).unwrap();

    // 2. Wall-exceeded unfrozen root (with 3 active subtasks): sync must NOT un-exhaust.
    let r_wall = root_task("t-wall", repo, TaskState::Ready);
    db.insert_task(&r_wall).unwrap();
    for i in 1..=3 {
        let child = Task::new_subtask(format!("t-wall-s{i}"), &r_wall, "s", spec(), 1_000).unwrap();
        db.insert_task(&child).unwrap();
    }
    let mut b_wall = RootBudget::new("t-wall", &base_cfg);
    b_wall.wall_secs_used = 12_500;
    db.put_budget(&b_wall).unwrap();

    // 3. Cycle-exceeded unfrozen root: raising max_unblock_cycles in config must NOT un-exhaust.
    db.insert_task(&root_task("t-cyc", repo, TaskState::Ready))
        .unwrap();
    let mut b_cyc = RootBudget::new("t-cyc", &base_cfg);
    b_cyc.cycles_used = 6;
    db.put_budget(&b_cyc).unwrap();

    // 4. Healthy multi-subtask root: must still scale cleanly.
    let r_ok = root_task("t-ok", repo, TaskState::Ready);
    db.insert_task(&r_ok).unwrap();
    for i in 1..=4 {
        let child = Task::new_subtask(format!("t-ok-s{i}"), &r_ok, "s", spec(), 1_000).unwrap();
        db.insert_task(&child).unwrap();
    }
    let mut b_ok = RootBudget::new("t-ok", &base_cfg);
    b_ok.wall_secs_used = 7_728;
    db.put_budget(&b_ok).unwrap();

    let raised_cfg = UnblockerConfig {
        max_token_budget: 2_000_000,
        max_wall_clock_secs: 10_800,
        max_unblock_cycles: 8,
        ..Default::default()
    };
    let updated = db.sync_repo_budget_limits(repo, &raised_cfg).unwrap();
    assert_eq!(updated, 1, "only the healthy root should be updated");

    let got_tok = db.get_budget("t-tok").unwrap();
    assert_eq!(got_tok.token_limit, 2_000_000);
    assert_eq!(got_tok.check(), BudgetCheck::TokensExceeded);

    let got_wall = db.get_budget("t-wall").unwrap();
    assert_eq!(got_wall.wall_secs_limit, 10_800);
    assert_eq!(got_wall.check(), BudgetCheck::WallClockExceeded);

    let got_cyc = db.get_budget("t-cyc").unwrap();
    assert_eq!(got_cyc.cycles_limit, 5);
    assert_eq!(got_cyc.check(), BudgetCheck::CyclesExceeded);

    let got_ok = db.get_budget("t-ok").unwrap();
    assert_eq!(
        (
            got_ok.token_limit,
            got_ok.wall_secs_limit,
            got_ok.cycles_limit
        ),
        (5_000_000, 27_000, 8)
    );
    assert_eq!(got_ok.wall_secs_used, 7_728);
    assert_eq!(got_ok.check(), BudgetCheck::Ok);
}

#[test]
fn ensure_budget_if_missing_scales_healthy_and_guards_exhausted_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    let cfg = UnblockerConfig {
        max_token_budget: 2_000_000,
        max_wall_clock_secs: 10_800,
        max_unblock_cycles: 5,
        ..Default::default()
    };

    // Healthy row scales both token_limit and wall_secs_limit upward.
    let mut healthy = RootBudget::new("r-ok", &cfg);
    healthy.wall_secs_used = 5_000;
    db.put_budget(&healthy).unwrap();
    let scaled = db
        .ensure_budget_if_missing(&RootBudget::new_scaled("r-ok", &cfg, 4))
        .unwrap();
    assert_eq!(
        (scaled.token_limit, scaled.wall_secs_limit),
        (5_000_000, 27_000)
    );
    assert_eq!(scaled.wall_secs_used, 5_000);

    // Exhausted (tokens / wall / cycles) and frozen rows must never be un-exhausted.
    for (id, mutate, expected) in [
        (
            "r-ex-tok",
            Box::new(|b: &mut RootBudget| b.tokens_used = b.token_limit + 1)
                as Box<dyn Fn(&mut RootBudget)>,
            BudgetCheck::TokensExceeded,
        ),
        (
            "r-ex-wall",
            Box::new(|b: &mut RootBudget| b.wall_secs_used = b.wall_secs_limit + 1),
            BudgetCheck::WallClockExceeded,
        ),
        (
            "r-ex-cyc",
            Box::new(|b: &mut RootBudget| b.cycles_used = b.cycles_limit + 1),
            BudgetCheck::CyclesExceeded,
        ),
        (
            "r-frozen",
            Box::new(|b: &mut RootBudget| b.frozen = true),
            BudgetCheck::Frozen,
        ),
    ] {
        let mut b = RootBudget::new(id, &cfg);
        mutate(&mut b);
        db.put_budget(&b).unwrap();
        let after = db
            .ensure_budget_if_missing(&RootBudget::new_scaled(id, &cfg, 4))
            .unwrap();
        assert_eq!(after.token_limit, 2_000_000, "{id}");
        assert_eq!(after.wall_secs_limit, 10_800, "{id}");
        assert_eq!(after.check(), expected, "{id}");
    }
}

#[test]
fn apply_split_atomic_scales_healthy_budget_guards_exhausted_and_handles_large_cfg() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    let drafts = vec![
        SplitSubtask {
            key: "a".into(),
            title: "sub a".into(),
            description: "do a".into(),
            acceptance: vec!["a ok".into()],
            owned_paths: vec!["src/a.rs".into()],
            depends_on: vec![],
        },
        SplitSubtask {
            key: "b".into(),
            title: "sub b".into(),
            description: "do b".into(),
            acceptance: vec!["b ok".into()],
            owned_paths: vec!["src/b.rs".into()],
            depends_on: vec![],
        },
        SplitSubtask {
            key: "c".into(),
            title: "sub c".into(),
            description: "do c".into(),
            acceptance: vec!["c ok".into()],
            owned_paths: vec!["src/c.rs".into()],
            depends_on: vec![],
        },
    ];
    let cfg = UnblockerConfig {
        max_token_budget: 2_000_000,
        max_wall_clock_secs: 10_800,
        ..Default::default()
    };

    // 1. Existing healthy root budget scales 1.0x -> 2.0x on 3-subtask split.
    let r_ok = root_task("t-split-ok", "/repo/s", TaskState::Ready);
    db.insert_task(&r_ok).unwrap();
    db.put_budget(&RootBudget::new("t-split-ok", &cfg)).unwrap();
    let out = apply_split(&db, &r_ok, &drafts, &cfg, 2_000, false).unwrap();
    assert!(out.split);
    let b_ok = db.get_budget("t-split-ok").unwrap();
    assert_eq!(
        (b_ok.token_limit, b_ok.wall_secs_limit),
        (4_000_000, 21_600)
    );

    // 2. Existing exhausted root budget must NOT be un-exhausted by apply_split_atomic.
    let r_ex = root_task("t-split-ex", "/repo/s", TaskState::Ready);
    db.insert_task(&r_ex).unwrap();
    let mut b_ex = RootBudget::new("t-split-ex", &cfg);
    b_ex.wall_secs_used = 12_000;
    db.put_budget(&b_ex).unwrap();
    let _ = apply_split(&db, &r_ex, &drafts, &cfg, 2_000, false).unwrap();
    let got_ex = db.get_budget("t-split-ex").unwrap();
    assert_eq!(got_ex.wall_secs_limit, 10_800);
    assert_eq!(got_ex.check(), BudgetCheck::WallClockExceeded);

    // 3. Large TOML-representable limits (<= i64::MAX) clamp safely on scale.
    let r_big = root_task("t-split-big", "/repo/s", TaskState::Ready);
    db.insert_task(&r_big).unwrap();
    let big_cfg = UnblockerConfig {
        max_token_budget: 5_000_000_000_000_000_000,
        max_wall_clock_secs: i64::MAX as u64,
        ..Default::default()
    };
    let out_big = apply_split(&db, &r_big, &drafts, &big_cfg, 2_000, false).unwrap();
    assert!(out_big.split);
    let b_big = db.get_budget("t-split-big").unwrap();
    assert_eq!(
        (b_big.token_limit, b_big.wall_secs_limit),
        (i64::MAX as u64, i64::MAX as u64)
    );
}

struct CountingRunner(AtomicUsize);

impl AgentRunner for CountingRunner {
    fn run(&self, _req: &AgentRequest) -> PortResult<AgentOutcome> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(AgentOutcome {
            exit_code: Some(0),
            timed_out: false,
            final_text: "[]".into(),
            tokens_in: 100,
            tokens_out: 100,
            limit_hit: None,
            infra: None,
            log_tail: String::new(),
        })
    }
}

#[test]
fn propose_and_apply_split_skips_exhausted_root_before_invoking_planner() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    let cfg = UnblockerConfig::default();
    let parent = root_task("t-prop-ex", "/repo/p", TaskState::Ready);
    db.insert_task(&parent).unwrap();
    let mut b = RootBudget::new("t-prop-ex", &cfg);
    b.wall_secs_used = b.wall_secs_limit + 1;
    db.put_budget(&b).unwrap();

    let runner = CountingRunner(AtomicUsize::new(0));
    let req = AgentRequest {
        run_id: "r1".into(),
        prompt: String::new(),
        cwd: "/repo/p".into(),
        account: None,
        timeout_secs: 120,
        sandbox: SandboxPolicy::default(),
        json_schema: None,
        max_turns: None,
    };
    let outcome =
        propose_and_apply_split(&runner, &db, &parent, &req, &cfg, 4, 2_000, false).unwrap();
    assert!(!outcome.split);
    assert_eq!(outcome.reason.as_deref(), Some("root budget exhausted"));
    assert_eq!(
        runner.0.load(Ordering::SeqCst),
        0,
        "planner must not run for an already-exhausted root"
    );
}
