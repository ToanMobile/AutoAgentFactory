#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Wave 4 Lane W4-C tests: Dual-Review handover `XONG` gate, clean `what_done`,
//! reviewer HTML table, and protected `.partial.json` garbage collection (R3-PROTO-6/7).

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

use aaf_domain::config::RepoConfig;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::gate::{
    GateMode, GateStatus, GoldenScore, HeadLockReport, LockStatus, PostGateAuditRecord,
    RECEIPT_SCHEMA_VERSION, Receipt, StoreComplianceReport,
};
use aaf_domain::pipeline::{PlanContract, ReviewerVerdictRecord};
use aaf_domain::ports::{EventSink, TaskStore};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::cross_audit::scoring::GOLDEN_IDS;
use aaf_engine::db::{Db, private};
use aaf_engine::gc::{GcOptions, run_gc};
use aaf_engine::gc_audits::{any_run_lock_held, clean_expired_audits, collect_protected_audit_ids};
use aaf_engine::git::worktree;
use aaf_engine::handover::{
    HandoverData, build_handover_from_parts_with_dual, load_handover, render_html, render_json,
};
use aaf_engine::handover_dual::{build_clean_what_done, render_dual_reviewers_html};
use serde_json::json;

fn sample_task(id: &str, repo: &str, state: TaskState) -> Task {
    let mut task = Task::new_root(
        id,
        repo,
        "Dual-review handover task",
        ImmutableSpec {
            description: "Verify dual consensus and clean handover".into(),
            acceptance: vec!["Dual consensus passes".into()],
        },
        Priority::P1,
        1_700_000_000,
    );
    task.state = state;
    task
}

fn clear_receipt(repo: &str, tree_fp: &str) -> Receipt {
    Receipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        repo_root: repo.into(),
        tree_fp: tree_fp.into(),
        head_sha: Some("1".repeat(40)),
        base_sha: Some("0".repeat(40)),
        mode: GateMode::Scoped,
        status: GateStatus::Clear,
        reasons: vec![],
        config_hash: "cfg-w4-1".into(),
        commands: vec![],
        static_findings: vec![],
        suppressions: 0,
        head_lock: HeadLockReport {
            status: LockStatus::Clear,
            lock_fp: String::new(),
            locked_changes: vec![],
            approval: None,
        },
        store_compliance: StoreComplianceReport::not_applicable(),
        created_at: 1_700_000_100,
    }
}

fn golden_scores(score: u8) -> Vec<GoldenScore> {
    GOLDEN_IDS
        .iter()
        .map(|id| GoldenScore {
            id: (*id).into(),
            score,
            deduction_reason: String::new(),
        })
        .collect()
}

fn reviewer(id: &str, executor: &str, model: &str, p1: u32, score: u8) -> ReviewerVerdictRecord {
    ReviewerVerdictRecord {
        id: id.into(),
        executor: executor.into(),
        canonical_provider: aaf_domain::pipeline::canonical_provider(executor),
        model: model.into(),
        effort: "high".into(),
        verdict: "PASS".into(),
        p0: 0,
        p1,
        p2: 1,
        llm_average: Some(score),
        scores: golden_scores(score),
    }
}

fn dual_audit(
    dual_consensus: bool,
    adjudicated: bool,
    reviewers: Vec<ReviewerVerdictRecord>,
) -> PostGateAuditRecord {
    PostGateAuditRecord {
        repo_root: "/tmp/repo".into(),
        tree_fp: "git-tree:dual1".into(),
        config_hash: "cfg-w4-1".into(),
        verdict: "PASS".into(),
        reviewer: "dual-review".into(),
        same_account: false,
        mechanical_score: 100,
        llm_average: Some(97),
        effective_score: 97,
        scores: golden_scores(97),
        dual_consensus,
        adjudicated,
        reviewers,
        created_at: 1_700_000_200,
    }
}

fn eval_dual(
    task: &Task,
    events: &[FactoryEvent],
    receipt: &Receipt,
    audit: Option<&PostGateAuditRecord>,
    score_gate: bool,
) -> HandoverData {
    build_handover_from_parts_with_dual(
        task,
        events,
        Some(receipt),
        audit,
        None,
        false,
        score_gate,
        95,
        90,
        true,
        true,
    )
}

fn init_git_repo(dir: &Path) {
    let run = |args: &[&str]| {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(dir)
                .status()
                .unwrap()
                .success()
        );
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "Test"]);
    fs::write(dir.join("README.md"), "# demo\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "-q", "-m", "init"]);
}

#[test]
fn handover_dual_review_enforces_consensus_and_real_providers_without_score_gate() {
    let task = sample_task("t-dual", "/tmp/repo", TaskState::Review);
    let receipt = clear_receipt("/tmp/repo", "git-tree:dual1");
    let r_agy = reviewer("rev-a", "antigravity", "model-a", 0, 98);
    let r_claude = reviewer("rev-b", "claude", "model-b", 0, 97);

    // 1. Missing audit when dual_review = true, score_gate = false -> CHƯA XONG
    let no_audit = eval_dual(&task, &[], &receipt, None, false);
    assert_eq!(no_audit.status, "CHƯA XONG");
    assert!(no_audit.missing.iter().any(|m| m.contains("dual-review")));

    // 2. dual_consensus = false -> CHƯA XONG
    let no_cons = dual_audit(false, false, vec![r_agy.clone(), r_claude.clone()]);
    let res = eval_dual(&task, &[], &receipt, Some(&no_cons), false);
    assert_eq!(res.status, "CHƯA XONG");
    assert!(res.missing.iter().any(|m| m.contains("dual_consensus")));

    // 3. Same provider (claude + claude), custom, antigravity+gemini, and P1>0 -> CHƯA XONG
    for bad_reviewers in [
        vec![
            r_claude.clone(),
            reviewer("rev-c", "claude", "model-c", 0, 96),
        ],
        vec![
            r_claude.clone(),
            reviewer("rev-custom", "custom", "wrap", 0, 99),
        ],
        vec![r_agy.clone(), reviewer("rev-g", "gemini", "model-g", 0, 97)],
        vec![
            r_agy.clone(),
            reviewer("rev-p1", "claude", "model-b", 1, 97),
        ],
    ] {
        let bad = dual_audit(true, false, bad_reviewers);
        assert_eq!(
            eval_dual(&task, &[], &receipt, Some(&bad), false).status,
            "CHƯA XONG"
        );
    }

    // 4. Valid dual_consensus = true with antigravity + claude (score_gate = false) -> XONG!
    let valid = dual_audit(true, true, vec![r_agy.clone(), r_claude.clone()]);
    let ok = eval_dual(&task, &[], &receipt, Some(&valid), false);
    assert_eq!(ok.status, "XONG", "missing={:?}", ok.missing);
    assert!(ok.dual_consensus && ok.adjudicated && ok.reviewers.len() == 2);
    assert_eq!(render_json(&ok)["dual_consensus"], json!(true));

    // 5. score_gate = true checks EACH reviewer independently
    let mut low_rev = r_claude;
    low_rev.scores[3].score = 80;
    let mixed = dual_audit(true, false, vec![r_agy, low_rev]);
    let res_score = eval_dual(&task, &[], &receipt, Some(&mixed), true);
    assert_eq!(res_score.status, "CHƯA XONG");
    assert!(
        res_score
            .missing
            .iter()
            .any(|m| m.contains("failed score gate"))
    );
}

#[test]
fn build_what_done_prefers_plan_converged_strips_internal_notes_and_renders_reviewers_html() {
    let mut task = sample_task("t-clean", "/tmp/repo", TaskState::Done);
    let internal = [
        "unblocked: retry after infra",
        "rework: fix missing test",
        "steer: check edge case",
        "post-gate cross-audit rejected: P1 found",
        "cross-audit rejected: stale",
        "audit_rework: round 1",
        "plan_approved: summary [plan_contract_hash=abc]",
        "unblock[T3]: fix build",
        "gate failed: lint",
        "merge conflict: rebase needed",
    ];
    task.context_notes = internal.iter().map(|s| (*s).into()).collect();

    // Without PlanConverged, ALL internal prefixes are stripped.
    let stripped = build_clean_what_done(&task, &[]).join(" · ");
    for bad in internal {
        let prefix = bad.split(':').next().unwrap();
        assert!(
            !stripped.contains(prefix),
            "leaked {prefix:?} in {stripped}"
        );
    }

    // With PlanConverged, PlanContract.summary is preferred.
    let contract = PlanContract {
        summary: "Implement blind parallel dual-review handover gate".into(),
        tier_assessment: "M".into(),
        tier_reasons: Vec::new(),
        files_to_change: Vec::new(),
        invariants_and_edge_cases: Vec::new(),
        red_green_tests: Vec::new(),
        dispositions: Vec::new(),
        open_questions: Vec::new(),
    };
    let conv_ev = FactoryEvent::new(
        EventKind::PlanConverged,
        json!({ "summary": contract.summary, "plan_contract": contract }).to_string(),
        10,
    )
    .for_task(&task.id);
    assert!(
        build_clean_what_done(&task, std::slice::from_ref(&conv_ev))
            .join(" · ")
            .contains(&contract.summary)
    );

    // load_handover retrieves PlanConverged via events_for_task_by_kinds even after >210 events.
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("aaf.db");
    let db = Db::open(&db_path).unwrap();
    db.insert_task(&task).unwrap();
    db.emit(&conv_ev).unwrap();
    for i in 0..215 {
        let ev = FactoryEvent::new(EventKind::RunStarted, json!({"i": i}).to_string(), 20 + i)
            .for_task(&task.id);
        db.emit(&ev).unwrap();
    }
    let ro = Db::open_read_only(&db_path).unwrap();
    let loaded = load_handover(&ro, tmp.path(), &task.id, &RepoConfig::default()).unwrap();
    assert!(
        loaded.what_done.contains(&contract.summary),
        "{}",
        loaded.what_done
    );

    // HTML renders 2-reviewer table, escapes XSS, and includes adjudicated badge.
    let r1 = reviewer("rev-1", "antigravity", "model-alpha", 0, 99);
    let r2 = reviewer("rev-2<script>", "claude", "model-beta", 0, 97);
    let snippet = render_dual_reviewers_html(&[r1.clone(), r2.clone()], true);
    assert!(snippet.contains("adjudicated") && snippet.contains("99/100"));
    assert!(!snippet.contains("<script>") && snippet.contains("rev-2&lt;script&gt;"));

    let receipt = clear_receipt("/tmp/repo", "git-tree:dual1");
    let audit = dual_audit(true, true, vec![r1, r2]);
    let data = eval_dual(&task, &[conv_ev], &receipt, Some(&audit), false);
    let full_html = render_html(&data, tmp.path());
    assert!(full_html.contains("Dual-Review Reviewers"));
    assert!(full_html.contains("antigravity/model-alpha"));
    assert!(full_html.contains("claude/model-beta"));
}

#[test]
fn gc_audits_protects_non_terminal_tasks_and_held_run_locks_while_cleaning_done_tasks() {
    let tmp = tempfile::tempdir().unwrap();
    let (data, worktrees, repo) = (
        tmp.path().join("data"),
        tmp.path().join("worktrees"),
        tmp.path().join("repo"),
    );
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(&worktrees).unwrap();
    init_git_repo(&repo);
    let repo_str = repo.to_string_lossy().into_owned();

    let db = Db::open(&data.join("aaf.db")).unwrap();
    for (id, st) in [
        ("t-gating", TaskState::Gating),
        ("t-running", TaskState::Running),
        ("t-ready", TaskState::Ready),
        ("t-done", TaskState::Done),
    ] {
        db.insert_task(&sample_task(id, &repo_str, st)).unwrap();
    }

    let make_partial = |task_id: &str| {
        let wt = worktree::worktree_path(&repo, &worktrees, task_id).unwrap();
        let dir = data.join("audits").join(aaf_fp::repo_id(&wt));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{task_id}.partial.json"));
        fs::write(&path, br#"{"partial":true}"#).unwrap();
        path
    };
    let (p_gating, p_running, p_ready, p_done) = (
        make_partial("t-gating"),
        make_partial("t-running"),
        make_partial("t-ready"),
        make_partial("t-done"),
    );

    // 1. Active `locks/*.run.lock` skips older_than_days == 0 partial sweep completely.
    let locks_dir = data.join("locks");
    private::create_dir(&locks_dir).unwrap();
    let lock_file = private::open_file(&locks_dir.join("other-repo.run.lock")).unwrap();
    lock_file.try_lock().unwrap();
    assert!(any_run_lock_held(&data));
    let zero_opts = GcOptions {
        older_than_days: 0,
        ..GcOptions::default()
    };
    run_gc(&data, &worktrees, &zero_opts).unwrap();
    assert!(p_gating.exists() && p_running.exists() && p_ready.exists() && p_done.exists());

    // 2. After releasing lock, dry_run reports without deleting, then real GC cleans Done only.
    lock_file.unlock().unwrap();
    drop(lock_file);
    assert!(!any_run_lock_held(&data));

    let dry = run_gc(
        &data,
        &worktrees,
        &GcOptions {
            dry_run: true,
            ..zero_opts.clone()
        },
    )
    .unwrap();
    assert!(p_done.exists() && dry.removed_files.contains(&p_done.display().to_string()));

    let swept = run_gc(&data, &worktrees, &zero_opts).unwrap();
    assert!(p_gating.exists() && p_running.exists() && p_ready.exists());
    assert!(!p_done.exists() && swept.removed_files.contains(&p_done.display().to_string()));

    // 3. Direct check: skipped_busy_repos protects repo-level partial file too.
    let busy_id = aaf_fp::repo_id(&repo);
    let busy_file = data.join("audits").join(&busy_id).join("repo.partial.json");
    fs::create_dir_all(busy_file.parent().unwrap()).unwrap();
    fs::write(&busy_file, br#"{}"#).unwrap();
    let busy_set: HashSet<String> = [repo_str].into_iter().collect();
    let protected = collect_protected_audit_ids(Some(&db), &busy_set, &data);
    let stats = clean_expired_audits(
        &data,
        SystemTime::now() + Duration::from_secs(60),
        0,
        &protected,
        false,
    );
    assert!(!stats.skipped_locked && busy_file.exists());
}
