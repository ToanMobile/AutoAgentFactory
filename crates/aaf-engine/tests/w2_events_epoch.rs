#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Wave 2 Lane A: SQL `EventSink::events_for_task_by_kinds` and epoch-bounded
//! unblock/audit helpers past the legacy 50-event window.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aaf_domain::account::AccountProfile;
use aaf_domain::config::{AuditScope, RepoConfig};
use aaf_domain::event::{EventKind as EK, FactoryEvent};
use aaf_domain::gate::{
    GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION, Receipt,
    StoreComplianceReport,
};
use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, EventSink, LimitHit, PortResult, TaskStore,
};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::cross_audit::build_checklist;
use aaf_engine::db::Db;
use aaf_engine::factory::adapters::{SystemClock, WebhookNotifier};
use aaf_engine::factory::{
    AccountPool, AuditBackend, Deps, GateRunner, GitOps, Grant, Hooks, Integration, Mode, Settings,
    run, run_with_audit,
};
use aaf_engine::git::{MergeOutcome, PrOutcome, TaskWorktree};
use aaf_engine::unblocker::{FailureSignal, QuotaRoute, failure_digest};
use serde_json::json;
use tempfile::TempDir;

type RegateFn<'a> = &'a dyn Fn(&Path) -> Result<bool, String>;
struct Stub(PathBuf, String);
struct ConstRunner(AgentOutcome);
impl AgentRunner for ConstRunner {
    fn run(&self, _: &AgentRequest) -> PortResult<AgentOutcome> {
        Ok(self.0.clone())
    }
}
impl AccountPool for Stub {
    fn acquire(&self) -> PortResult<Option<Grant>> {
        Ok(Some(Grant::new(None, None)))
    }
    fn after_limit(&self) -> PortResult<Option<QuotaRoute>> {
        Ok(Some(QuotaRoute::Wait))
    }
    fn report_limit(&self, _: &str, _: &LimitHit) -> PortResult<()> {
        Ok(())
    }
    fn profiles(&self) -> PortResult<Vec<AccountProfile>> {
        Ok(vec![])
    }
}
impl GitOps for Stub {
    fn task_worktree(&self, id: &str) -> PortResult<TaskWorktree> {
        let path = self.0.join(format!("wt-{id}"));
        std::fs::create_dir_all(&path).unwrap();
        Ok(TaskWorktree {
            path,
            branch: format!("aaf/{id}"),
        })
    }
    fn commit_all(&self, _: &TaskWorktree, _: &str) -> PortResult<()> {
        Ok(())
    }
    fn has_changes(&self, _: &TaskWorktree) -> PortResult<bool> {
        Ok(true)
    }
    fn diff(&self, _: &TaskWorktree) -> PortResult<String> {
        Ok("diff --git a/src/lib.rs b/src/lib.rs\n--- /dev/null\n+++ b/src/lib.rs\n@@ -0,0 +1,1 @@\n+pub fn ok() {}\n".into())
    }
    fn integrate(&self, ids: &[String]) -> PortResult<Integration> {
        let path = self.0.join("int");
        std::fs::create_dir_all(&path).unwrap();
        let tree = TaskWorktree {
            path,
            branch: "aaf/int".into(),
        };
        Ok(Integration {
            tree,
            merged: ids.to_vec(),
            conflicted: vec![],
        })
    }
    fn discard(&self, _: &Integration) -> PortResult<()> {
        Ok(())
    }
    fn rebase(&self, _: &TaskWorktree) -> PortResult<Vec<String>> {
        Ok(vec![])
    }
    fn base_sha(&self) -> PortResult<String> {
        Ok("base0".into())
    }
    fn merge(&self, _: &TaskWorktree, _: RegateFn<'_>) -> PortResult<MergeOutcome> {
        Ok(MergeOutcome::Merged {
            new_base_sha: "base1".into(),
        })
    }
    fn open_pr(
        &self,
        _: &TaskWorktree,
        _: &str,
        _: &Receipt,
        _: Option<&str>,
    ) -> PortResult<PrOutcome> {
        Ok(PrOutcome::Opened {
            url: "https://example.test/pr/1".into(),
        })
    }
    fn fetch_base(&self) -> PortResult<()> {
        Ok(())
    }
    fn is_merged(&self, _: &str) -> PortResult<bool> {
        Ok(false)
    }
    fn cleanup(&self, _: &str) -> PortResult<()> {
        Ok(())
    }
}
impl GateRunner for Stub {
    fn run(&self, tree: &Path) -> PortResult<Receipt> {
        Ok(Receipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            repo_root: tree.to_string_lossy().into_owned(),
            tree_fp: "git-tree:ok".into(),
            head_sha: Some("h1".into()),
            base_sha: Some("b0".into()),
            mode: GateMode::Scoped,
            status: GateStatus::Clear,
            reasons: vec![],
            config_hash: self.1.clone(),
            commands: vec![],
            static_findings: vec![],
            suppressions: 0,
            head_lock: self.head_lock(tree)?,
            store_compliance: StoreComplianceReport::not_applicable(),
            created_at: 2_000,
        })
    }
    fn head_lock(&self, _: &Path) -> PortResult<HeadLockReport> {
        Ok(HeadLockReport {
            status: LockStatus::Clear,
            lock_fp: String::new(),
            locked_changes: vec![],
            approval: None,
        })
    }
}

fn spec() -> ImmutableSpec {
    ImmutableSpec {
        description: "w2".into(),
        acceptance: vec!["ok".into()],
    }
}
fn open_task(id: &str) -> (TempDir, Db) {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    db.insert_task(&Task::new_root(
        id,
        "/fake/repo",
        id,
        spec(),
        Priority::P1,
        100,
    ))
    .unwrap();
    db.set_state(id, TaskState::Ready, None, 101).unwrap();
    (tmp, db)
}
fn emit(db: &Db, id: &str, kind: EK, body: &str, ts: i64) {
    db.emit(&FactoryEvent::new(kind, body, ts).for_task(id))
        .unwrap();
}
fn emit_noise(db: &Db, id: &str, count: usize, ts0: i64) {
    let ks = [EK::RunStarted, EK::RunFinished, EK::GateResult];
    for i in 0..count {
        emit(db, id, ks[i % 3], "{}", ts0 + i as i64);
    }
}
fn emit_reset(db: &Db, id: &str, reset: Option<&str>, ts: i64) {
    if reset == Some("human") {
        emit(
            db,
            id,
            EK::TaskStateChanged,
            r#"{"from":"blocked","to":"ready","by":"human"}"#,
            ts,
        );
    }
    if reset == Some("rework") {
        emit(db, id, EK::ReworkRequested, "{}", ts);
    }
}
fn outcome(code: i32, text: &str, tail: &str) -> AgentOutcome {
    AgentOutcome {
        exit_code: Some(code),
        timed_out: false,
        final_text: text.into(),
        tokens_in: 10,
        tokens_out: 5,
        limit_hit: None,
        infra: None,
        log_tail: tail.into(),
    }
}
fn mk_deps(
    tmp: &TempDir,
    db: &Db,
    runner: Arc<dyn AgentRunner>,
    repo: RepoConfig,
) -> (Deps, Arc<Stub>) {
    let stub = Arc::new(Stub(
        tmp.path().to_path_buf(),
        aaf_engine::gate::config_hash(&repo),
    ));
    let dir = tmp.path().to_path_buf();
    let deps = Deps {
        store: Arc::new(db.clone()),
        clock: Arc::new(SystemClock),
        runner,
        accounts: stub.clone(),
        git: stub.clone(),
        gate: stub.clone(),
        notifier: Arc::new(WebhookNotifier { url: String::new() }),
        settings: Settings {
            repo_root: "/fake/repo".into(),
            repo,
            max_parallel: 1,
            auto_merge: false,
            sandboxed: false,
            hook_required: false,
            agent_path: Default::default(),
            mobile: false,
            data_dir: dir.clone(),
            home: dir,
            owner_github_allowlist: Vec::new(),
        },
    };
    (deps, stub)
}

#[test]
fn db_events_for_task_by_kinds_filters_orders_limits_and_handles_empty() {
    let (_tmp, db) = open_task("t1");
    db.insert_task(&Task::new_root(
        "t2",
        "/fake/repo",
        "t2",
        spec(),
        Priority::P1,
        100,
    ))
    .unwrap();
    let target = [
        (EK::PlanDebateTurn, "d1"),
        (EK::CrossAudit, "a1"),
        (EK::UnblockStep, "u1"),
        (EK::TaskStateChanged, "s1"),
        (EK::PlanDebateTurn, "d2"),
        (EK::ReworkRequested, "r1"),
        (EK::CrossAudit, "a2"),
        (EK::UnblockStep, "u2"),
        (EK::TaskStateChanged, "s2"),
        (EK::PlanDebateTurn, "d3"),
    ];
    for (i, (k, tag)) in target.iter().enumerate() {
        let ts = 100 + (i as i64) * 20;
        emit_noise(&db, "t1", 9, ts);
        emit(&db, "t1", *k, &format!(r#"{{"tag":"{tag}"}}"#), ts + 10);
        emit(&db, "t2", *k, r#"{"tag":"other"}"#, ts + 10);
    }
    assert!(
        db.events_for_task_by_kinds("t1", &[], 10)
            .unwrap()
            .is_empty()
    );
    assert!(
        db.events_for_task_by_kinds("t1", &[EK::PlanDebateTurn], 0)
            .unwrap()
            .is_empty()
    );

    let dedup = db
        .events_for_task_by_kinds(
            "t1",
            &[EK::PlanDebateTurn, EK::PlanDebateTurn, EK::CrossAudit],
            u32::MAX,
        )
        .unwrap();
    let tags: Vec<_> = dedup.iter().map(|e| e.payload_json.as_str()).collect();
    assert_eq!(
        tags,
        vec![
            r#"{"tag":"d3"}"#,
            r#"{"tag":"a2"}"#,
            r#"{"tag":"d2"}"#,
            r#"{"tag":"a1"}"#,
            r#"{"tag":"d1"}"#
        ]
    );
    assert!(
        dedup.windows(2).all(|w| w[0].id > w[1].id)
            && dedup.iter().all(|e| e.task_id.as_deref() == Some("t1"))
    );

    let all5 = [
        EK::PlanDebateTurn,
        EK::CrossAudit,
        EK::UnblockStep,
        EK::TaskStateChanged,
        EK::ReworkRequested,
    ];
    let lim = db.events_for_task_by_kinds("t1", &all5, 3).unwrap();
    assert_eq!(
        (
            lim.len(),
            lim[0].payload_json.as_str(),
            lim[2].payload_json.as_str()
        ),
        (3, r#"{"tag":"d3"}"#, r#"{"tag":"u2"}"#)
    );
    assert_eq!(
        db.events_for_task_by_kinds("t1", &all5, u32::MAX)
            .unwrap()
            .len(),
        10
    );
}

#[test]
fn unblock_and_audit_helpers_see_past_50_noise_and_reset_on_epoch() {
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    let fail = outcome(1, "", "compile error: missing symbol foo");
    let digest = failure_digest(&FailureSignal {
        outcome: Some(fail.clone()),
        ..FailureSignal::empty()
    });

    for (reset, stuck) in [
        (None, true),
        (Some("human"), false),
        (Some("rework"), false),
    ] {
        let (tmp, db) = open_task("t1");
        let p = json!({"digest": digest}).to_string();
        emit(&db, "t1", EK::UnblockStep, &p, 10);
        emit(&db, "t1", EK::UnblockStep, &p, 11);
        emit_reset(&db, "t1", reset, 20);
        emit_noise(&db, "t1", 65, 30);
        let (deps, _) = mk_deps(
            &tmp,
            &db,
            Arc::new(ConstRunner(fail.clone())),
            RepoConfig::default(),
        );
        let _ = run(&deps, Mode::Once, &hooks).unwrap();
        let ev = &db
            .events_for_task_by_kinds("t1", &[EK::UnblockStep], 1)
            .unwrap()[0];
        assert_eq!(
            ev.payload_json.contains("same failure 3 times in a row"),
            stuck,
            "prior_same reset={reset:?}: {}",
            ev.payload_json
        );
    }

    for (rework_after, expect_note) in [(false, true), (true, false)] {
        let (tmp, db) = open_task("t1");
        db.append_context_note("t1", "unblock[t3] digest=abc", 10)
            .unwrap();
        emit(&db, "t1", EK::UnblockStep, "{}", 15);
        emit_reset(&db, "t1", Some("human"), 20);
        if rework_after {
            emit_reset(&db, "t1", Some("rework"), 25);
        }
        emit_noise(&db, "t1", 65, 30);
        let (deps, _) = mk_deps(
            &tmp,
            &db,
            Arc::new(ConstRunner(fail.clone())),
            RepoConfig::default(),
        );
        let _ = run(&deps, Mode::Once, &hooks).unwrap();
        let has = db
            .get_task("t1")
            .unwrap()
            .context_notes
            .iter()
            .any(|n| n == "unblocked: human unblock");
        assert_eq!(
            has, expect_note,
            "has_unrecorded_human_unblock rework_after={rework_after}"
        );
    }

    let ans: Vec<_> = build_checklist(&spec(), false)
        .iter()
        .enumerate()
        .map(|(i, it)| {
            if i == 0 {
                json!({"id": it.id, "answer": "yes", "evidence": "src/lib.rs:1 stub"})
            } else {
                json!({"id": it.id, "answer": "no"})
            }
        })
        .collect();
    let fail_reply = json!({"answers": ans}).to_string();
    for (reset, expect_rework) in [(None, false), (Some("human"), true), (Some("rework"), true)] {
        let (tmp, db) = open_task("t1");
        emit(
            &db,
            "t1",
            EK::CrossAudit,
            r#"{"verdict":"fail: STUB (src/lib.rs:1)"}"#,
            10,
        );
        emit_reset(&db, "t1", reset, 20);
        emit_noise(&db, "t1", 65, 30);
        let mut repo = RepoConfig::default();
        repo.cross_audit.audit_scope = AuditScope::Always;
        repo.pipeline.max_audit_reworks = 1;
        let (deps, stub) = mk_deps(
            &tmp,
            &db,
            Arc::new(ConstRunner(outcome(0, "done", ""))),
            repo,
        );
        let audit = AuditBackend {
            provider: "codex".into(),
            accounts: stub,
            hook_required: false,
            runner: Arc::new(ConstRunner(outcome(0, &fail_reply, ""))),
        };
        let _ = run_with_audit(&deps, Some(&audit), Mode::Once, &hooks).unwrap();
        let t = db.get_task("t1").unwrap();
        assert_eq!(
            t.context_notes
                .iter()
                .any(|n| n.starts_with("audit_rework:")),
            expect_rework,
            "reset={reset:?}"
        );
        if expect_rework {
            assert_eq!(t.state, TaskState::Ready);
        }
    }
}
