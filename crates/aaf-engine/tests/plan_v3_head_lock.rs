#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Plan v3 security regression tests for 2-tier `head_lock` (`LockTier`),
//! `reuse::receipt_matches` cache bypass prevention (`V2-IMP-03`), `pr_merge`,
//! `approve_if_pass_with_config` (`V2-SEC-11`), and `worktree::existing_registered`
//! (`UNBLK-07`).

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aaf_domain::config::{GlobalConfig, MachineConfig, RepoConfig};
use aaf_domain::gate::{
    Approval, ChangeKind, GateMode, GateStatus, HeadLockReport, LockStatus, LockTier, LockedChange,
    RECEIPT_SCHEMA_VERSION, Receipt, StoreComplianceReport,
};
use aaf_domain::ports::Clock;
use aaf_engine::cross_audit::{AuditReport, AuditVerdict, approve_if_pass_with_config};
use aaf_engine::gate::{
    GIT_TIMEOUT, HiddenPolicy, config_hash, head_lock, run_gate_policy, stats_repo_id,
};
use aaf_engine::git::{GhClient, GitError, MergeStrategy, PrCheck, PrInfo, worktree};
use aaf_engine::pr_ops::{PrMergeOutcome, PrMergeRequest, merge_pr};
use aaf_engine::throttle::{SystemProbe, Throttle};
use tempfile::TempDir;

struct FixedClock(i64);
impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "dev")
        .env("GIT_AUTHOR_EMAIL", "dev@example.invalid")
        .env("GIT_COMMITTER_NAME", "dev")
        .env("GIT_COMMITTER_EMAIL", "dev@example.invalid")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

struct FakeGh {
    pr: PrInfo,
    merged: AtomicUsize,
}

impl GhClient for FakeGh {
    fn view_pr(&self, _repo_root: &Path, _pr_number: u64) -> Result<PrInfo, GitError> {
        Ok(self.pr.clone())
    }

    fn pr_checks(&self, _repo_root: &Path, _pr_number: u64) -> Result<Vec<PrCheck>, GitError> {
        Ok(vec![])
    }

    fn merge_pr(
        &self,
        _repo_root: &Path,
        _pr_number: u64,
        _expect_head: &str,
        _strategy: MergeStrategy,
        _subject: Option<&str>,
        _body_file: Option<&Path>,
    ) -> Result<(), GitError> {
        self.merged.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct RepoFixture {
    _bare: TempDir,
    work: TempDir,
    data: TempDir,
    cfg: RepoConfig,
}

impl RepoFixture {
    fn new() -> Self {
        let bare = TempDir::new().unwrap();
        git(bare.path(), &["init", "--bare", "-b", "main"]);
        let work = TempDir::new().unwrap();
        git(work.path(), &["init", "-b", "main"]);
        git(
            work.path(),
            &["remote", "add", "origin", bare.path().to_str().unwrap()],
        );
        for d in [".autofactory", "src", "tests"] {
            fs::create_dir_all(work.path().join(d)).unwrap();
        }
        let cfg_toml = r#"[project]
name = "v3"
profile = "rust"

[commands]
test_scoped = "true"
test_full = "true"

[gate]
base_ref = "origin/main"

[cross_audit]
require_different_provider = false
"#;
        fs::write(work.path().join(".autofactory/config.toml"), cfg_toml).unwrap();
        fs::write(
            work.path().join("Cargo.toml"),
            "[package]\nname = \"v3\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(work.path().join("src/lib.rs"), "pub fn v() -> i32 { 1 }\n").unwrap();
        fs::write(
            work.path().join("tests/basic_test.rs"),
            "#[test] fn t() { assert_eq!(1, 1); }\n",
        )
        .unwrap();
        git(work.path(), &["add", "."]);
        git(work.path(), &["commit", "-m", "init"]);
        git(work.path(), &["push", "-u", "origin", "main"]);

        let cfg: RepoConfig = toml::from_str(cfg_toml).unwrap();
        Self {
            _bare: bare,
            work,
            data: TempDir::new().unwrap(),
            cfg,
        }
    }

    fn branch(&self, name: &str, edit: impl FnOnce(&Path)) -> String {
        git(self.work.path(), &["checkout", "-b", name, "origin/main"]);
        edit(self.work.path());
        git(self.work.path(), &["add", "-A"]);
        git(self.work.path(), &["commit", "-m", name]);
        let sha = git(self.work.path(), &["rev-parse", "HEAD"]);
        git(self.work.path(), &["push", "-u", "origin", name]);
        git(self.work.path(), &["checkout", "main"]);
        sha
    }

    fn merge_req(&self, pr_number: u64, expect_head: &str, dry_run: bool) -> PrMergeRequest {
        PrMergeRequest {
            repo_root: self.work.path().to_path_buf(),
            data_dir: self.data.path().to_path_buf(),
            pr_number,
            expect_head: expect_head.to_string(),
            remote: "origin".to_string(),
            mode: GateMode::Scoped,
            strategy: MergeStrategy::Squash,
            subject: None,
            body_file: None,
            ci_wait_secs: 0,
            require_checks: false,
            lock_timeout: Duration::from_secs(10),
            dry_run,
            machine: MachineConfig::default(),
            global_config: None,
            no_receipt_reuse: false,
            run_commands_despite_head_lock: false,
            strict_merged_head: false,
            keep_non_aaf_local: false,
        }
    }

    fn gh(&self, number: u64, branch: &str, head: &str) -> FakeGh {
        FakeGh {
            pr: PrInfo {
                number,
                state: "OPEN".to_string(),
                head_ref_oid: head.to_string(),
                head_ref_name: branch.to_string(),
                base_ref_name: "main".to_string(),
                mergeable: "MERGEABLE".to_string(),
                is_draft: false,
                title: format!("PR #{number}"),
                url: format!("https://github.com/org/repo/pull/{number}"),
                ..Default::default()
            },
            merged: AtomicUsize::new(0),
        }
    }

    fn base_sha(&self) -> String {
        git(self.work.path(), &["rev-parse", "origin/main"])
    }

    fn approve(&self, dir: &Path, reviewer: &str, same_account: bool, ts: i64) -> Approval {
        head_lock::approve(
            self.data.path(),
            dir,
            &self.cfg,
            reviewer,
            same_account,
            &FixedClock(ts),
        )
        .unwrap()
    }

    fn run_merge(&self, req: &PrMergeRequest, gh: &FakeGh, ts: i64) -> PrMergeOutcome {
        let noop = |_: &aaf_engine::throttle::Progress<'_>| {};
        merge_pr(req, gh, &FixedClock(ts), &noop).unwrap()
    }
}

#[test]
fn tier1_changes_reject_same_account_across_head_lock_cache_and_pr_merge() {
    let fx = RepoFixture::new();
    let noop = |_: &aaf_engine::throttle::Progress<'_>| {};
    let machine = MachineConfig::default();
    let probe = SystemProbe::detect();
    let throttle = Throttle::system(&machine, &probe, &noop);
    let base_sha = fx.base_sha();
    let allow_line = concat!("// aaf:", "allow(LOG-001) needed\nprintln!(\"ok\");\n");

    let cases: [(&str, &str, &str); 5] = [
        (
            "feat/tier1-cargo",
            "Cargo.toml",
            "[package]\nname = \"v3\"\nversion = \"0.2.0\"\n",
        ),
        (
            "feat/tier1-husky",
            ".husky/pre-commit",
            "#!/bin/sh\necho hook\n",
        ),
        (
            "feat/tier1-agent-skill",
            ".agent/skills/x/SKILL.md",
            "# skill\n",
        ),
        (
            "feat/tier1-sop",
            ".autofactory/sops/a.toml",
            "name = \"deploy\"\n",
        ),
        ("feat/tier1-inline-allow", "src/lib.rs", allow_line),
    ];

    for (idx, (branch, rel_path, content)) in cases.iter().enumerate() {
        let pr_num = 200 + idx as u64;
        let head = fx.branch(branch, |root| {
            let full = root.join(rel_path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&full, content).unwrap();
        });

        git(fx.work.path(), &["checkout", branch]);
        let canon = fs::canonicalize(fx.work.path()).unwrap();

        // 1. head_lock classifies the change as LockTier::Hard and rejects same_account=true.
        let before = head_lock::evaluate(fx.data.path(), &canon, &fx.cfg, &base_sha);
        assert_eq!(before.status, LockStatus::Blocked, "{rel_path}");
        assert!(before.has_hard_lock(), "{rel_path}: {before:?}");
        assert!(
            before
                .locked_changes
                .iter()
                .all(|c| c.tier == LockTier::Hard),
            "{rel_path}: {:?}",
            before.locked_changes
        );

        let same_approval = fx.approve(&canon, "self-auditor", true, 100 + idx as i64);
        let after_same = head_lock::evaluate(fx.data.path(), &canon, &fx.cfg, &base_sha);
        assert_eq!(
            after_same.status,
            LockStatus::Blocked,
            "head_lock must reject same_account=true for Tier-1 {rel_path}"
        );

        // 2. Plant a signed Clear receipt (local + common cache) with same_account=true
        //    and LockTier::Hard -> BOTH run_gate_policy and pr_merge MUST reject it (V2-IMP-03).
        let tree_fp = aaf_fp::tree_fp(&canon, GIT_TIMEOUT).unwrap();
        let key = aaf_fp::load_or_create_key(fx.data.path()).unwrap();
        let forged_cached = Receipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            repo_root: canon.to_string_lossy().into_owned(),
            tree_fp: tree_fp.clone(),
            head_sha: Some(head.clone()),
            base_sha: Some(base_sha.clone()),
            mode: GateMode::Scoped,
            status: GateStatus::Clear,
            reasons: Vec::new(),
            config_hash: config_hash(&fx.cfg),
            commands: Vec::new(),
            static_findings: Vec::new(),
            suppressions: 0,
            head_lock: HeadLockReport {
                status: LockStatus::Approved,
                lock_fp: before.lock_fp.clone(),
                locked_changes: before.locked_changes.clone(),
                approval: Some(same_approval),
            },
            store_compliance: StoreComplianceReport::not_applicable(),
            created_at: 100 + idx as i64,
        };
        let common_id = stats_repo_id(&canon);
        aaf_fp::write_receipt(fx.data.path(), &key, &forged_cached).unwrap();
        aaf_fp::write_common_receipt(fx.data.path(), &key, &common_id, &forged_cached).unwrap();

        let gate_run = run_gate_policy(
            fx.data.path(),
            &canon,
            &fx.cfg,
            GateMode::Scoped,
            &FixedClock(200 + idx as i64),
            &throttle,
            HiddenPolicy::Remove,
        )
        .unwrap();
        assert!(
            !gate_run.reused,
            "reuse::receipt_matches must reject cached same_account=true Hard receipt ({rel_path})"
        );
        assert_eq!(gate_run.receipt.status, GateStatus::Blocked, "{rel_path}");

        // Re-plant common receipt (persist_receipt evicted it) so pr_merge probe_common_receipt
        // is also directly tested against a cached same_account=true Hard receipt.
        aaf_fp::write_common_receipt(fx.data.path(), &key, &common_id, &forged_cached).unwrap();
        git(fx.work.path(), &["checkout", "main"]);

        let gh = fx.gh(pr_num, branch, &head);
        let merge_blocked =
            fx.run_merge(&fx.merge_req(pr_num, &head, false), &gh, 300 + idx as i64);
        assert!(
            matches!(
                merge_blocked,
                PrMergeOutcome::Blocked { ref code, .. } if code == "cross_approval_required"
            ),
            "pr_merge must reject Tier-1 same_account=true ({rel_path}): {merge_blocked:?}"
        );
        assert_eq!(gh.merged.load(Ordering::SeqCst), 0);

        // 3. Independent approval (same_account=false) unlocks both head_lock and pr_merge.
        git(fx.work.path(), &["checkout", branch]);
        let _ = fx.approve(&canon, "independent", false, 400 + idx as i64);
        assert_eq!(
            head_lock::evaluate(fx.data.path(), &canon, &fx.cfg, &base_sha).status,
            LockStatus::Approved,
            "{rel_path}"
        );
        git(fx.work.path(), &["checkout", "main"]);

        let merge_ok = fx.run_merge(&fx.merge_req(pr_num, &head, true), &gh, 500 + idx as i64);
        assert!(
            matches!(merge_ok, PrMergeOutcome::DryRunClear { .. }),
            "independent approval must unlock {rel_path}: {merge_ok:?}"
        );
    }
}

#[test]
fn mixed_tier1_and_tier2_pr_enforces_tier1_dominance() {
    let fx = RepoFixture::new();
    let base_sha = fx.base_sha();
    let head = fx.branch("feat/mixed-tiers", |root| {
        fs::write(
            root.join("tests/basic_test.rs"),
            "#[test] fn t() { assert_eq!(2, 2); }\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"v3\"\nversion = \"0.3.0\"\n",
        )
        .unwrap();
    });

    git(fx.work.path(), &["checkout", "feat/mixed-tiers"]);
    let canon = fs::canonicalize(fx.work.path()).unwrap();
    let rep = head_lock::evaluate(fx.data.path(), &canon, &fx.cfg, &base_sha);
    assert!(
        rep.has_hard_lock(),
        "mixed PR must report has_hard_lock=true"
    );
    assert!(rep.locked_changes.iter().any(|c| c.tier == LockTier::Soft));
    assert!(rep.locked_changes.iter().any(|c| c.tier == LockTier::Hard));

    let _ = fx.approve(&canon, "solo", true, 10);
    assert_eq!(
        head_lock::evaluate(fx.data.path(), &canon, &fx.cfg, &base_sha).status,
        LockStatus::Blocked
    );
    git(fx.work.path(), &["checkout", "main"]);

    let gh = fx.gh(301, "feat/mixed-tiers", &head);
    let out = fx.run_merge(&fx.merge_req(301, &head, false), &gh, 11);
    assert!(matches!(
        out,
        PrMergeOutcome::Blocked { ref code, .. } if code == "cross_approval_required"
    ));
}

#[test]
fn tier2_pure_test_allows_same_account_when_different_provider_not_required() {
    let fx = RepoFixture::new();
    let base_sha = fx.base_sha();
    let head = fx.branch("feat/pure-test", |root| {
        fs::write(
            root.join("tests/basic_test.rs"),
            "#[test] fn t() { assert_eq!(9, 9); }\n",
        )
        .unwrap();
    });

    git(fx.work.path(), &["checkout", "feat/pure-test"]);
    let canon = fs::canonicalize(fx.work.path()).unwrap();
    let rep = head_lock::evaluate(fx.data.path(), &canon, &fx.cfg, &base_sha);
    assert!(
        !rep.has_hard_lock(),
        "pure test must be Tier-2 Soft: {rep:?}"
    );
    assert_eq!(rep.locked_changes.len(), 1);
    assert_eq!(rep.locked_changes[0].tier, LockTier::Soft);

    let _ = fx.approve(&canon, "solo", true, 20);
    assert_eq!(
        head_lock::evaluate(fx.data.path(), &canon, &fx.cfg, &base_sha).status,
        LockStatus::Approved
    );
    git(fx.work.path(), &["checkout", "main"]);

    let gh = fx.gh(302, "feat/pure-test", &head);
    // Blocked if global config forces require_different_provider = true.
    let mut strict_req = fx.merge_req(302, &head, false);
    let mut global = GlobalConfig::default();
    global.cross_audit.require_different_provider = Some(true);
    strict_req.global_config = Some(global);
    let blocked = fx.run_merge(&strict_req, &gh, 21);
    assert!(matches!(
        blocked,
        PrMergeOutcome::Blocked { ref code, .. } if code == "cross_approval_required"
    ));

    // Allowed for solo-dev when require_different_provider = false.
    let merged = fx.run_merge(&fx.merge_req(302, &head, false), &gh, 22);
    assert!(
        matches!(merged, PrMergeOutcome::Merged { pr_number: 302, .. }),
        "{merged:?}"
    );
}

#[test]
fn legacy_locked_change_json_without_tier_deserializes_as_hard_fail_closed() {
    let raw = r#"{"path":"tests/basic_test.rs","change":"modified"}"#;
    let parsed: LockedChange = serde_json::from_str(raw).unwrap();
    assert_eq!(parsed.path, "tests/basic_test.rs");
    assert_eq!(parsed.change, ChangeKind::Modified);
    assert_eq!(parsed.tier, LockTier::Hard);

    let report = HeadLockReport {
        status: LockStatus::Approved,
        lock_fp: "fp".to_string(),
        locked_changes: vec![parsed],
        approval: Some(Approval {
            lock_fp: "fp".to_string(),
            reviewer: "solo".to_string(),
            verdict: "PASS".to_string(),
            same_account: true,
            created_at: 1,
        }),
    };
    assert!(report.has_hard_lock());
}

#[test]
fn existing_registered_returns_none_when_worktree_dir_deleted_even_if_branch_exists() {
    let fx = RepoFixture::new();
    let wt_dir = TempDir::new().unwrap();
    let wt = worktree::create(fx.work.path(), wt_dir.path(), "t-deleted-dir", "main").unwrap();
    fs::write(wt.path.join("src/lib.rs"), "pub fn v() -> i32 { 7 }\n").unwrap();
    git(&wt.path, &["add", "-A"]);
    git(&wt.path, &["commit", "-m", "work"]);

    assert!(
        worktree::existing_registered(fx.work.path(), wt_dir.path(), "t-deleted-dir")
            .unwrap()
            .is_some()
    );

    // Delete the worktree directory on disk while keeping refs/heads/aaf/t-deleted-dir.
    fs::remove_dir_all(&wt.path).unwrap();
    assert!(
        worktree::existing_head(fx.work.path(), wt_dir.path(), "t-deleted-dir")
            .unwrap()
            .is_some(),
        "branch ref still exists for existing_head"
    );
    assert!(
        worktree::existing_registered(fx.work.path(), wt_dir.path(), "t-deleted-dir")
            .unwrap()
            .is_none(),
        "existing_registered must return None once worktree directory is gone"
    );
}

#[test]
fn approve_if_pass_with_config_enforces_lock_fp_toctou_and_mirrors_to_common_root() {
    let fx = RepoFixture::new();
    let wt_dir = TempDir::new().unwrap();
    let wt = worktree::create(
        fx.work.path(),
        wt_dir.path(),
        "t-audit-mirror",
        "origin/main",
    )
    .unwrap();
    fs::write(
        wt.path.join("Cargo.toml"),
        "[package]\nname = \"v3\"\nversion = \"0.9.0\"\n",
    )
    .unwrap();
    git(&wt.path, &["add", "-A"]);
    git(&wt.path, &["commit", "-m", "bump cargo"]);

    let base_sha = fx.base_sha();
    let eval = head_lock::evaluate(fx.data.path(), &wt.path, &fx.cfg, &base_sha);
    assert_eq!(eval.status, LockStatus::Blocked);
    let pass_report = AuditReport {
        verdict: AuditVerdict::Pass {
            warnings: Vec::new(),
        },
        checklist: Vec::new(),
        scores: Vec::new(),
        diff_truncated: false,
        tokens_in: 1,
        tokens_out: 1,
    };
    let try_approve = |fp: &str, ts: i64| {
        approve_if_pass_with_config(
            &pass_report,
            fx.data.path(),
            &wt.path,
            &fx.cfg,
            fp,
            "cross-audit:gemini/default",
            false,
            &FixedClock(ts),
        )
    };

    // Stale lock_fp must be rejected (TOCTOU guard).
    let err = try_approve("0000stale", 1).unwrap_err();
    assert!(err.to_string().contains("locked diff changed"), "{err}");

    // Matching lock_fp signs and mirrors under common repo root.
    let approved = try_approve(&eval.lock_fp, 2)
        .unwrap()
        .expect("approval written");
    assert!(!approved.same_account);

    let key = aaf_fp::load_key(fx.data.path()).unwrap();
    let common_root = fs::canonicalize(fx.work.path()).unwrap();
    let mirrored = aaf_fp::read_approval(fx.data.path(), &key, &common_root, &eval.lock_fp)
        .unwrap()
        .expect("mirrored to common_repo_root");
    assert_eq!(mirrored.reviewer, "cross-audit:gemini/default");
}
