#![allow(clippy::unwrap_used, clippy::expect_used)]
//! RFC-054 oracle: autonomy rate (excluding automatic `UnblockStep`), first-pass
//! gate metrics + `duration_ms`, CrossAudit fail rate, `ImplementerEscalated`,
//! read-only DB safety, and backward-compatible `TelemetrySummary` serde.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::config::RepoConfig;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::gate::{
    CommandResult, GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION,
    Receipt, StoreComplianceReport,
};
use aaf_domain::ports::{
    AccountStore, AgentOutcome, AgentRequest, AgentRunner, Clock, EventSink, LimitHit, PortResult,
    TaskStore,
};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::factory::{
    AccountPool, Deps, GateRunner, GitOps, Grant, Hooks, Integration, Mode, Notifier, Settings, run,
};
use aaf_engine::git::{MergeOutcome, PrOutcome, TaskWorktree};
use aaf_engine::telemetry::{
    AutonomyMetrics, CostPolicy, StatusQuery, TelemetrySummary, collect_status_from_db,
};
use aaf_engine::webhook::WebhookEvent;
use serde_json::{Value, json};
use tempfile::TempDir;

fn done_task(id: &str, repo: &str, now: i64) -> Task {
    let mut t = Task::new_root(
        id,
        repo,
        format!("Task {id}"),
        ImmutableSpec {
            description: "RFC-054 oracle task".to_string(),
            acceptance: vec!["passes verification".to_string()],
        },
        Priority::P1,
        now,
    );
    t.state = TaskState::Done;
    t
}

#[test]
fn autonomy_metrics_exclude_auto_unblock_and_work_read_only() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("aaf.db");
    let repo = "/repo-rfc054";
    {
        let db = Db::open(&db_path).unwrap();
        let emit = |t, k, p: Value, ts| {
            db.emit(&FactoryEvent::new(k, p.to_string(), ts).for_task(t))
                .unwrap();
        };

        // Task 1 (Done): automatic UnblockStep + first-pass GateResult(clear, 1200ms) + CrossAudit(pass)
        db.insert_task(&done_task("t-auto", repo, 1_000)).unwrap();
        emit(
            "t-auto",
            EventKind::UnblockStep,
            json!({"tier": "T1", "note": "auto retry"}),
            1_010,
        );
        emit(
            "t-auto",
            EventKind::TaskStateChanged,
            json!({"to": "ready", "why": "auto_retry_due"}),
            1_011,
        );
        emit(
            "t-auto",
            EventKind::GateResult,
            json!({"status": "CLEAR", "duration_ms": 1200}),
            1_020,
        );
        emit(
            "t-auto",
            EventKind::CrossAudit,
            json!({"verdict": "pass"}),
            1_030,
        );

        // Task 2 (Done): GateResult(failed, 2400ms) -> ReworkRequested -> GateResult(clear, 1800ms)
        // + CrossAudit(fail) + ImplementerEscalated
        db.insert_task(&done_task("t-human", repo, 1_100)).unwrap();
        emit(
            "t-human",
            EventKind::GateResult,
            json!({"status": "BLOCKED", "duration_ms": 2400}),
            1_110,
        );
        emit(
            "t-human",
            EventKind::ReworkRequested,
            json!({"reason": "fix edge case", "by": "human"}),
            1_120,
        );
        emit(
            "t-human",
            EventKind::ImplementerEscalated,
            json!({"tier": "L", "reworks": 1, "executor": "claude", "model": "opus", "effort": "high"}),
            1_130,
        );
        emit(
            "t-human",
            EventKind::GateResult,
            json!({"status": "clear", "duration_ms": 1800}),
            1_140,
        );
        emit(
            "t-human",
            EventKind::CrossAudit,
            json!({"verdict": "fail: missing oracle"}),
            1_150,
        );
    }

    // Must succeed on strictly read-only handle without creating receipt.key.
    let ro_db = Db::open_read_only(&db_path).unwrap();
    let summary = collect_status_from_db(
        &ro_db,
        &StatusQuery {
            repo_root: Some(repo),
            data_dir: Some(dir.path()),
            now: 2_000,
            cost: &CostPolicy::default(),
        },
    )
    .unwrap();
    assert!(
        !dir.path().join("receipt.key").exists(),
        "read-only telemetry must not create receipt.key"
    );

    assert_eq!(
        summary.autonomy,
        AutonomyMetrics {
            done_tasks: 2,
            done_without_human: 1,
            autonomy_rate_pct: Some(50.0),
            gate_tasks_evaluated: 2,
            gate_first_pass_clear: 1,
            gate_first_pass_rate_pct: Some(50.0),
            gate_avg_duration_ms: Some(1800),
            cross_audit_total: 2,
            cross_audit_failed: 1,
            cross_audit_fail_rate_pct: Some(50.0),
            implementer_escalations: 1,
        }
    );

    let serialized = serde_json::to_value(&summary).unwrap();
    let autonomy = &serialized["autonomy"];
    assert!(
        autonomy.is_object(),
        "TelemetrySummary must include autonomy metrics object: {serialized}"
    );
    assert_eq!(autonomy["done_tasks"], json!(2));
    assert_eq!(autonomy["done_without_human"], json!(1));
    assert_eq!(autonomy["autonomy_rate_pct"], json!(50.0));
    assert_eq!(autonomy["gate_tasks_evaluated"], json!(2));
    assert_eq!(autonomy["gate_first_pass_clear"], json!(1));
    assert_eq!(autonomy["gate_first_pass_rate_pct"], json!(50.0));
    assert_eq!(autonomy["gate_avg_duration_ms"], json!(1800));
    assert_eq!(autonomy["cross_audit_total"], json!(2));
    assert_eq!(autonomy["cross_audit_failed"], json!(1));
    assert_eq!(autonomy["cross_audit_fail_rate_pct"], json!(50.0));
    assert_eq!(autonomy["implementer_escalations"], json!(1));

    // Legacy TelemetrySummary JSON without `autonomy` must deserialize via #[serde(default)].
    let mut legacy = serialized.clone();
    legacy.as_object_mut().unwrap().remove("autonomy");
    let decoded: TelemetrySummary = serde_json::from_value(legacy).unwrap();
    assert_eq!(decoded.autonomy, AutonomyMetrics::default());
}

#[test]
fn human_state_changes_count_while_auto_and_normal_lifecycle_do_not() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("aaf.db");
    let repo = "/repo-human-kinds";
    {
        let db = Db::open(&db_path).unwrap();

        // Purely autonomous task (with normal backlog->ready + auto unblock + merge)
        db.insert_task(&done_task("t-clean", repo, 100)).unwrap();
        db.emit(
            &FactoryEvent::new(
                EventKind::TaskStateChanged,
                json!({"from": "backlog", "to": "ready", "why": "aaf board ready", "by": "human"})
                    .to_string(),
                101,
            )
            .for_task("t-clean"),
        )
        .unwrap();
        db.emit(
            &FactoryEvent::new(
                EventKind::TaskStateChanged,
                json!({"to": "ready", "why": "audit_rework"}).to_string(),
                102,
            )
            .for_task("t-clean"),
        )
        .unwrap();
        db.emit(
            &FactoryEvent::new(
                EventKind::TaskStateChanged,
                json!({"from": "review", "to": "done", "why": "merged PR #7", "by": "human"})
                    .to_string(),
                103,
            )
            .for_task("t-clean"),
        )
        .unwrap();

        // Task with human undo (snapshots.rs emits why="undo" without "by")
        db.insert_task(&done_task("t-undo", repo, 200)).unwrap();
        db.emit(
            &FactoryEvent::new(
                EventKind::TaskStateChanged,
                json!({"to": "ready", "why": "undo", "commit_oid": "abc", "ref_name": "refs/aaf/snap/1"})
                    .to_string(),
                201,
            )
            .for_task("t-undo"),
        )
        .unwrap();

        // Task with human approve-plan
        db.insert_task(&done_task("t-approve-plan", repo, 300))
            .unwrap();
        db.emit(
            &FactoryEvent::new(
                EventKind::TaskStateChanged,
                json!({"from": "blocked", "to": "ready", "why": "plan_approved", "by": "human"})
                    .to_string(),
                301,
            )
            .for_task("t-approve-plan"),
        )
        .unwrap();

        // Task with telegram unblock
        db.insert_task(&done_task("t-unblock", repo, 400)).unwrap();
        db.emit(
            &FactoryEvent::new(
                EventKind::TaskStateChanged,
                json!({"to": "ready", "why": "aaf telegram unblock", "by": "telegram:42"})
                    .to_string(),
                401,
            )
            .for_task("t-unblock"),
        )
        .unwrap();
    }

    let ro_db = Db::open_read_only(&db_path).unwrap();
    let summary = collect_status_from_db(
        &ro_db,
        &StatusQuery {
            repo_root: Some(repo),
            data_dir: None,
            now: 500,
            cost: &CostPolicy::default(),
        },
    )
    .unwrap();
    let serialized = serde_json::to_value(&summary).unwrap();
    assert_eq!(serialized["autonomy"]["done_tasks"], json!(4));
    assert_eq!(serialized["autonomy"]["done_without_human"], json!(1));
    assert_eq!(serialized["autonomy"]["autonomy_rate_pct"], json!(25.0));
}

// ---- End-to-end GateResult duration_ms emission via factory::run ----------

struct FixedClock(i64);
impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

struct FixedPool(Vec<AccountProfile>);
impl AccountPool for FixedPool {
    fn acquire(&self) -> PortResult<Option<Grant>> {
        Ok(Some(Grant::new(self.0.first().cloned(), None)))
    }
    fn report_limit(&self, _: &str, _: &LimitHit) -> PortResult<()> {
        Ok(())
    }
    fn profiles(&self) -> PortResult<Vec<AccountProfile>> {
        Ok(self.0.clone())
    }
}

struct NullNotifier;
impl Notifier for NullNotifier {
    fn notify(&self, _: &WebhookEvent) -> Result<bool, String> {
        Ok(false)
    }
}

struct OkRunner;
impl AgentRunner for OkRunner {
    fn run(&self, _: &AgentRequest) -> PortResult<AgentOutcome> {
        Ok(AgentOutcome {
            exit_code: Some(0),
            timed_out: false,
            final_text: "done".to_string(),
            tokens_in: 10,
            tokens_out: 5,
            limit_hit: None,
            infra: None,
            log_tail: String::new(),
        })
    }
}

struct FakeGit(PathBuf);
impl GitOps for FakeGit {
    fn task_worktree(&self, task_id: &str) -> PortResult<TaskWorktree> {
        let path = self.0.join(task_id);
        std::fs::create_dir_all(&path).unwrap();
        Ok(TaskWorktree {
            branch: format!("aaf/{task_id}"),
            path,
        })
    }
    fn commit_all(&self, _: &TaskWorktree, _: &str) -> PortResult<()> {
        Ok(())
    }
    fn has_changes(&self, _: &TaskWorktree) -> PortResult<bool> {
        Ok(true)
    }
    fn diff(&self, _: &TaskWorktree) -> PortResult<String> {
        Ok("diff --git a/a.rs b/a.rs\n+let x = 1;\n".to_string())
    }
    fn integrate(&self, _: &[String]) -> PortResult<Integration> {
        Ok(Integration {
            tree: TaskWorktree {
                path: self.0.join("clean"),
                branch: "aaf/clean".to_string(),
            },
            merged: Vec::new(),
            conflicted: Vec::new(),
        })
    }
    fn discard(&self, _: &Integration) -> PortResult<()> {
        Ok(())
    }
    fn rebase(&self, _: &TaskWorktree) -> PortResult<Vec<String>> {
        Ok(Vec::new())
    }
    fn base_sha(&self) -> PortResult<String> {
        Ok("base000".to_string())
    }
    fn merge(
        &self,
        _: &TaskWorktree,
        _: &dyn Fn(&Path) -> Result<bool, String>,
    ) -> PortResult<MergeOutcome> {
        Ok(MergeOutcome::Merged {
            new_base_sha: "base111".to_string(),
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
            url: "https://example.invalid/pr/1".to_string(),
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

struct TimedGate(Vec<u64>);
impl GateRunner for TimedGate {
    fn run(&self, worktree: &Path) -> PortResult<Receipt> {
        let commands = self
            .0
            .iter()
            .enumerate()
            .map(|(idx, &ms)| CommandResult {
                name: format!("cmd_{idx}"),
                command: "true".to_string(),
                exit_code: Some(0),
                timed_out: false,
                duration_ms: ms,
                log_tail: String::new(),
            })
            .collect();
        Ok(Receipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            repo_root: worktree.to_string_lossy().into_owned(),
            tree_fp: "git-tree:timed".to_string(),
            head_sha: Some("head111".to_string()),
            base_sha: Some("base000".to_string()),
            mode: GateMode::Scoped,
            status: GateStatus::Clear,
            reasons: Vec::new(),
            config_hash: "cfg".to_string(),
            commands,
            static_findings: Vec::new(),
            suppressions: 0,
            head_lock: HeadLockReport {
                status: LockStatus::Clear,
                lock_fp: String::new(),
                locked_changes: Vec::new(),
                approval: None,
            },
            store_compliance: StoreComplianceReport::not_applicable(),
            created_at: 1_000,
        })
    }
    fn head_lock(&self, _: &Path) -> PortResult<HeadLockReport> {
        Ok(HeadLockReport {
            status: LockStatus::Clear,
            lock_fp: String::new(),
            locked_changes: Vec::new(),
            approval: None,
        })
    }
}

#[test]
fn finish_gate_payload_records_total_command_duration_ms() {
    let dir = TempDir::new().unwrap();
    let repo_dir = dir.path().join("repo");
    let wt_dir = dir.path().join("wt");
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::create_dir_all(&wt_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let db_path = data_dir.join("aaf.db");
    let db = Arc::new(Db::open(&db_path).unwrap());
    let acct = AccountProfile {
        id: "acct-1".to_string(),
        provider: ProviderKind::ClaudeCode,
        config_dir: data_dir.to_string_lossy().into_owned(),
        name: "acct-1".to_string(),
    };
    db.upsert_account(&acct).unwrap();

    let repo_str = repo_dir.to_string_lossy().into_owned();
    let mut task = Task::new_root(
        "t-timed",
        &repo_str,
        "Timed gate task",
        ImmutableSpec {
            description: "Check gate duration_ms".to_string(),
            acceptance: vec!["emits duration_ms".to_string()],
        },
        Priority::P1,
        1_000,
    );
    task.state = TaskState::Ready;
    db.insert_task(&task).unwrap();

    let mut repo_cfg = RepoConfig::default();
    repo_cfg.run.auto_merge = true;
    repo_cfg.cross_audit.audit_scope = aaf_domain::config::AuditScope::HeadLock;

    let deps = Deps {
        store: db.clone(),
        accounts: Arc::new(FixedPool(vec![acct])),
        runner: Arc::new(OkRunner),
        git: Arc::new(FakeGit(wt_dir)),
        gate: Arc::new(TimedGate(vec![450, 750])),
        notifier: Arc::new(NullNotifier),
        clock: Arc::new(FixedClock(1_050)),
        settings: Settings {
            repo_root: repo_str,
            data_dir: data_dir.clone(),
            home: dir.path().to_path_buf(),
            repo: repo_cfg,
            max_parallel: 1,
            auto_merge: true,
            sandboxed: false,
            hook_required: false,
            agent_path: Default::default(),
            mobile: false,
            owner_github_allowlist: Vec::new(),
        },
    };
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    let out = run(&deps, Mode::Once, &hooks).unwrap();
    assert_eq!(out.done, vec!["t-timed".to_string()]);

    let events = db
        .events_for_task_by_kinds("t-timed", &[EventKind::GateResult], 10)
        .unwrap();
    assert_eq!(events.len(), 1);
    let payload: Value = serde_json::from_str(&events[0].payload_json).unwrap();
    assert_eq!(
        payload["duration_ms"],
        json!(1200),
        "GateResult payload must record duration_ms: {payload}"
    );
}

#[test]
fn archived_done_task_does_not_inflate_autonomy_and_straddling_task_is_not_split() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("aaf.db");
    let (repo, now) = ("/repo-archive-autonomy", 1_700_000_000i64 + 30 * 86_400);
    let (old_ts, fresh_ts) = (now - 20 * 86_400, now - 2 * 86_400);
    {
        let db = Db::open(&db_path).unwrap();
        for id in ["t-old", "t-span"] {
            db.insert_task(&done_task(id, repo, old_ts)).unwrap();
        }
        // `t-old`: fully older than cutoff with ReworkRequested -> archived completely.
        // `t-span`: straddles cutoff (old BLOCKED + ReworkRequested, fresh CLEAR) -> never split.
        for (tid, kind, payload, ts) in [
            (
                "t-old",
                EventKind::ReworkRequested,
                r#"{"reason":"x"}"#,
                old_ts,
            ),
            (
                "t-span",
                EventKind::GateResult,
                r#"{"status":"BLOCKED"}"#,
                old_ts,
            ),
            (
                "t-span",
                EventKind::ReworkRequested,
                r#"{"reason":"y"}"#,
                old_ts,
            ),
            (
                "t-span",
                EventKind::GateResult,
                r#"{"status":"CLEAR"}"#,
                fresh_ts,
            ),
        ] {
            db.emit(&FactoryEvent::new(kind, payload, ts).for_task(tid))
                .unwrap();
        }
    }

    let res = aaf_engine::gc::archive_old_terminal_events(dir.path(), now, 14, false).unwrap();
    assert_eq!(res.events_archived, 1);

    let ro = Db::open_read_only(&db_path).unwrap();
    let q = StatusQuery {
        repo_root: Some(repo),
        data_dir: Some(dir.path()),
        now,
        cost: &CostPolicy::default(),
    };
    let a = collect_status_from_db(&ro, &q).unwrap().autonomy;
    assert_eq!((a.done_tasks, a.done_without_human), (1, 0));
    assert_eq!(a.autonomy_rate_pct, Some(0.0));
    assert_eq!((a.gate_tasks_evaluated, a.gate_first_pass_clear), (1, 0));
    assert_eq!(a.gate_first_pass_rate_pct, Some(0.0));
}
