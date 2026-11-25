#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for audit round 2 fixes across gate, pr_merge,
//! store_compliance, db, and gc.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aaf_domain::account::ProviderKind;
use aaf_domain::config::{MachineConfig, RepoConfig};
use aaf_domain::gate::{
    ChangeKind, GateMode, GateStatus, LockStatus, LockedChange, Severity, StoreStatus,
};
use aaf_domain::ports::{AccountStore, Clock, RunStore, TaskStore};
use aaf_domain::run::{RunRecord, RunStatus};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::gate::changes::AddedLines;
use aaf_engine::gate::{
    HiddenPolicy, config_hash, head_lock, run_gate_policy, static_checks, stats_repo_id,
};
use aaf_engine::git::{
    GhClient, GitError, MergeStrategy, PrCheck, PrInfo, PrMergeOutcome, PrMergeRequest, merge_pr,
};
use aaf_engine::store_compliance;
use rusqlite::Connection;
use tempfile::TempDir;

#[path = "throttle_support.rs"]
mod throttle_support;
use throttle_support::calm_throttle;

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
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn scan_one(path: &str, line: &str) -> Vec<String> {
    static_checks::scan(&[AddedLines {
        path: path.to_string(),
        lines: vec![(1, line.to_string())],
    }])
    .unwrap()
    .findings
    .into_iter()
    .map(|f| f.rule)
    .collect()
}

#[test]
fn static_checks_closes_comment_prefix_and_hash_and_doc_ext_bypasses() {
    let err = concat!("catch (e) {", "}");
    let log = concat!("console.", "log(1);");

    // Trailing code after balanced `/* ... */` or `*/` must NOT be treated as a comment.
    assert_eq!(
        scan_one("src/a.js", &format!("/* hi */ {err}")),
        vec!["ERR-001"]
    );
    assert_eq!(
        scan_one("src/a.js", &format!("/* a */ /* b */ {log}")),
        vec!["LOG-001"]
    );
    assert_eq!(scan_one("src/a.kt", &format!("*/ {err}")), vec!["ERR-001"]);
    assert_eq!(
        scan_one("src/a.js", &format!("*/ /* x */ {log}")),
        vec!["LOG-001"]
    );

    // Pure comments stay clean.
    assert!(scan_one("src/a.js", &format!("/* {err} */")).is_empty());
    assert!(scan_one("src/a.js", &format!("/* a */ /* b */ // {err}")).is_empty());
    assert!(scan_one("src/a.js", "*/ // done").is_empty());

    // `#` is only a comment on `#`-comment files, never on JS/TS/Rust/Kotlin/Java/C.
    assert_eq!(scan_one("src/a.js", &format!("# {err}")), vec!["ERR-001"]);
    assert_eq!(scan_one("src/a.ts", &format!("# {log}")), vec!["LOG-001"]);
    assert_eq!(scan_one("src/a.rs", &format!("# {err}")), vec!["ERR-001"]);
    let py_err = concat!("# except Exception:", " pass");
    assert!(scan_one("src/a.py", py_err).is_empty());

    // `.mdx` executes JSX/JS so it is NOT a doc path; `.markdown` IS a doc path.
    assert_eq!(scan_one("docs/page.mdx", err), vec!["ERR-001"]);
    assert!(scan_one("docs/notes.markdown", err).is_empty());
}

#[test]
fn head_lock_blocks_git_mv_into_agent_config_and_falls_through_on_same_account_local() {
    let dir = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["config", "user.email", "t@t"]);
    git(dir.path(), &["config", "user.name", "t"]);
    fs::create_dir_all(dir.path().join("docs")).unwrap();
    fs::write(dir.path().join("docs/notes.md"), "be brief\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);

    git(dir.path(), &["checkout", "-q", "-b", "feat"]);
    git(dir.path(), &["mv", "docs/notes.md", "CLAUDE.md"]);

    let cfg = RepoConfig::default();
    let report = head_lock::evaluate(data.path(), dir.path(), &cfg, "main");
    assert_eq!(report.status, LockStatus::Blocked);
    assert_eq!(
        report.locked_changes,
        vec![LockedChange {
            path: "CLAUDE.md".to_string(),
            change: ChangeKind::Renamed,
            tier: aaf_domain::gate::LockTier::Hard,
        }]
    );

    // Worktree approval fallback: local same_account=true should fall through to
    // common_root's independent approval, while tampered local file must fail closed.
    git(dir.path(), &["commit", "-q", "-m", "mv"]);
    let wt_parent = TempDir::new().unwrap();
    let wt = wt_parent.path().join("wt");
    git(
        dir.path(),
        &["worktree", "add", "--detach", wt.to_str().unwrap(), "HEAD"],
    );

    let mut strict_cfg = RepoConfig::default();
    strict_cfg.cross_audit.require_different_provider = true;
    strict_cfg.gate.base_ref = "main".to_string();

    let key = aaf_fp::load_or_create_key(data.path()).unwrap();
    let lock_fp = head_lock::evaluate(data.path(), &wt, &strict_cfg, "main").lock_fp;
    let common_root = fs::canonicalize(dir.path()).unwrap();
    let wt_canon = fs::canonicalize(&wt).unwrap();

    // Write independent approval under common_root, same_account=true under wt.
    aaf_fp::write_approval(
        data.path(),
        &key,
        &common_root,
        &aaf_domain::gate::Approval {
            lock_fp: lock_fp.clone(),
            reviewer: "auditor".to_string(),
            verdict: "PASS".to_string(),
            same_account: false,
            created_at: 10,
        },
    )
    .unwrap();
    aaf_fp::write_approval(
        data.path(),
        &key,
        &wt_canon,
        &aaf_domain::gate::Approval {
            lock_fp: lock_fp.clone(),
            reviewer: "self".to_string(),
            verdict: "PASS".to_string(),
            same_account: true,
            created_at: 11,
        },
    )
    .unwrap();
    let ok = head_lock::evaluate(data.path(), &wt_canon, &strict_cfg, "main");
    assert_eq!(ok.status, LockStatus::Approved);
    assert!(!ok.approval.unwrap().same_account);

    // Tampering the local approval file must fail closed (never fall through).
    let local_path = aaf_fp::approval_path(data.path(), &wt_canon, &lock_fp);
    fs::write(&local_path, b"corrupted").unwrap();
    let tampered = head_lock::evaluate(data.path(), &wt_canon, &strict_cfg, "main");
    assert_eq!(tampered.status, LockStatus::Blocked);

    let _ = Command::new("git")
        .args(["worktree", "remove", "--force", wt.to_str().unwrap()])
        .current_dir(dir.path())
        .output();
}

#[test]
fn config_hash_changes_with_require_different_provider() {
    let base = RepoConfig::default();
    let mut strict = base.clone();
    strict.cross_audit.require_different_provider = true;
    assert_ne!(config_hash(&base), config_hash(&strict));
}

#[test]
fn warn_policy_receipt_with_hidden_files_is_isolated_from_common_cache_and_strict_reuse() {
    let dir = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["config", "user.email", "t@t"]);
    git(dir.path(), &["config", "user.name", "t"]);
    fs::write(dir.path().join("README.md"), "hi\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);

    // Plant an info/exclude hidden file.
    let exclude = dir.path().join(".git/info/exclude");
    fs::write(&exclude, "conftest.py\n").unwrap();
    fs::write(dir.path().join("conftest.py"), "x = 1\n").unwrap();

    let mut cfg = RepoConfig::default();
    cfg.commands.lint = "true".to_string();
    cfg.commands.test_scoped = "true".to_string();
    let machine = load_free_machine();
    let throttle = calm_throttle(&machine);

    let warned = run_gate_policy(
        data.path(),
        dir.path(),
        &cfg,
        GateMode::Scoped,
        &FixedClock(100),
        &throttle,
        HiddenPolicy::Warn,
    )
    .unwrap();
    assert_eq!(warned.receipt.status, GateStatus::Clear);
    assert!(
        warned
            .receipt
            .reasons
            .iter()
            .any(|r| r.contains("warning: work tree:"))
    );

    // Must NOT be stored in the shared cross-worktree common receipt cache.
    let key = aaf_fp::load_key(data.path()).unwrap();
    let common_id = stats_repo_id(dir.path());
    assert!(
        aaf_fp::read_common_receipt(data.path(), &key, &common_id, &warned.receipt.tree_fp)
            .is_err()
    );

    // Removing the hidden file and running with HiddenPolicy::Remove must NOT
    // reuse the warned local receipt.
    fs::remove_file(dir.path().join("conftest.py")).unwrap();
    let strict = run_gate_policy(
        data.path(),
        dir.path(),
        &cfg,
        GateMode::Scoped,
        &FixedClock(200),
        &throttle,
        HiddenPolicy::Remove,
    )
    .unwrap();
    assert!(!strict.reused);
    assert_eq!(strict.receipt.created_at, 200);
}

struct MockGh {
    pr: PrInfo,
    merged: AtomicUsize,
}

impl GhClient for MockGh {
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

#[test]
fn pr_merge_catches_force_added_gitignored_secret_and_validates_remote() {
    let bare = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    git(bare.path(), &["init", "--bare", "-b", "main"]);
    git(work.path(), &["init", "-b", "main"]);
    git(work.path(), &["config", "user.email", "dev@example.com"]);
    git(work.path(), &["config", "user.name", "Dev"]);
    git(
        work.path(),
        &["remote", "add", "origin", bare.path().to_str().unwrap()],
    );
    fs::create_dir_all(work.path().join(".autofactory")).unwrap();
    fs::write(
        work.path().join(".autofactory/config.toml"),
        "[commands]\ntest_scoped = \"true\"\n[gate]\nbase_ref = \"origin/main\"\n",
    )
    .unwrap();
    fs::write(work.path().join(".gitignore"), "*.secret\n").unwrap();
    git(work.path(), &["add", "."]);
    git(work.path(), &["commit", "-m", "init"]);
    git(work.path(), &["push", "-u", "origin", "main"]);

    // Create a PR branch that force-adds a gitignored file containing a secret.
    git(work.path(), &["checkout", "-b", "feat/ignored-secret"]);
    let secret = concat!("AKIA", "0123456789ABCDEF");
    fs::write(work.path().join("leaked.secret"), format!("KEY={secret}\n")).unwrap();
    git(work.path(), &["add", "-f", "leaked.secret"]);
    git(work.path(), &["commit", "-m", "force-add ignored secret"]);
    let head_sha = git(work.path(), &["rev-parse", "HEAD"]);
    git(
        work.path(),
        &["push", "-u", "origin", "feat/ignored-secret"],
    );
    git(work.path(), &["checkout", "main"]);

    let gh = MockGh {
        pr: PrInfo {
            number: 77,
            state: "OPEN".to_string(),
            head_ref_oid: head_sha.clone(),
            head_ref_name: "feat/ignored-secret".to_string(),
            base_ref_name: "main".to_string(),
            mergeable: "MERGEABLE".to_string(),
            is_draft: false,
            title: "test".to_string(),
            url: "https://example.invalid/pr/77".to_string(),
            ..Default::default()
        },
        merged: AtomicUsize::new(0),
    };
    let noop = |_: &aaf_engine::throttle::Progress<'_>| {};

    // Invalid or unconfigured remotes are rejected immediately.
    for bad_remote in ["--upload-pack=evil", "no-such-remote"] {
        let req = PrMergeRequest {
            repo_root: work.path().to_path_buf(),
            data_dir: data.path().to_path_buf(),
            pr_number: 77,
            expect_head: head_sha.clone(),
            remote: bad_remote.to_string(),
            mode: GateMode::Scoped,
            strategy: MergeStrategy::Squash,
            subject: None,
            body_file: None,
            ci_wait_secs: 0,
            require_checks: false,
            lock_timeout: Duration::from_secs(5),
            dry_run: true,
            machine: load_free_machine(),
            global_config: None,
        };
        let err = merge_pr(&req, &gh, &FixedClock(1), &noop).unwrap_err();
        assert!(matches!(err, GitError::Invalid(_)), "{bad_remote}: {err:?}");
    }

    // Valid remote catches the force-added `.gitignore`d secret on the merged tree.
    let req = PrMergeRequest {
        repo_root: work.path().to_path_buf(),
        data_dir: data.path().to_path_buf(),
        pr_number: 77,
        expect_head: head_sha,
        remote: "origin".to_string(),
        mode: GateMode::Scoped,
        strategy: MergeStrategy::Squash,
        subject: None,
        body_file: None,
        ci_wait_secs: 0,
        require_checks: false,
        lock_timeout: Duration::from_secs(5),
        dry_run: false,
        machine: load_free_machine(),
        global_config: None,
    };
    let out = merge_pr(&req, &gh, &FixedClock(2), &noop).unwrap();
    match out {
        PrMergeOutcome::Blocked { code, receipt, .. } => {
            assert_eq!(code, "gate_blocked");
            let receipt = receipt.expect("gate receipt");
            assert!(
                receipt
                    .static_findings
                    .iter()
                    .any(|f| f.rule == "SEC-001" && f.file == "leaked.secret"),
                "{:?}",
                receipt.static_findings
            );
        }
        other => panic!("expected Blocked(gate_blocked), got {other:?}"),
    }
    assert_eq!(gh.merged.load(Ordering::SeqCst), 0);
}

#[test]
fn store_compliance_ignores_gradle_test_options_and_handles_braced_ios_var_before_paren() {
    // Android: AGP 8+ testOptions/lint targetSdk = 33 must not trigger AND-003 when defaultConfig is 36.
    let android = TempDir::new().unwrap();
    fs::create_dir_all(android.path().join("app/src/main")).unwrap();
    fs::write(
        android.path().join("app/src/main/AndroidManifest.xml"),
        r#"<manifest xmlns:android="http://schemas.android.com/apk/res/android"><application /></manifest>"#,
    )
    .unwrap();
    fs::write(
        android.path().join("app/build.gradle.kts"),
        r#"plugins {
    id("com.android.application")
}
android {
    defaultConfig {
        targetSdk = 36
    }
    testOptions {
        targetSdk = 33
    }
    lint {
        targetSdk = 33
    }
}
"#,
    )
    .unwrap();
    let android_report = store_compliance::scan(android.path(), &RepoConfig::default(), true);
    assert_eq!(
        android_report.status,
        StoreStatus::Clear,
        "{android_report:#?}"
    );
    assert!(
        android_report.findings.is_empty(),
        "{:#?}",
        android_report.findings
    );

    // iOS: `${APP_NAME} $(ACTION)` has `${` before `$(`; both must be stripped so it is
    // judged as a build-setting variable (unresolved IOS-001), not falsely accepted as Ok.
    let ios = TempDir::new().unwrap();
    fs::create_dir_all(ios.path().join("App.xcodeproj")).unwrap();
    fs::create_dir_all(ios.path().join("App")).unwrap();
    fs::write(
        ios.path().join("App.xcodeproj/project.pbxproj"),
        "SDKROOT = iphoneos;\nINFOPLIST_FILE = App/Info.plist;\n",
    )
    .unwrap();
    fs::write(
        ios.path().join("App/Info.plist"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>NSCameraUsageDescription</key>
<string>${APP_NAME} $(ACTION)</string>
</dict></plist>
"#,
    )
    .unwrap();
    let ios_report = store_compliance::scan(ios.path(), &RepoConfig::default(), true);
    assert_eq!(ios_report.status, StoreStatus::Clear);
    assert!(
        ios_report
            .unresolved
            .iter()
            .any(|u| u.starts_with("IOS-001")),
        "expected unresolved IOS-001 for variable purpose string, got {ios_report:#?}"
    );
    assert!(
        ios_report
            .findings
            .iter()
            .any(|f| f.rule == "IOS-001" && f.severity == Severity::Warn),
        "{:#?}",
        ios_report.findings
    );
}

#[test]
fn db_schema_v2_list_accounts_read_only_and_running_run_ids() {
    let dir = TempDir::new().unwrap();
    let v2_path = dir.path().join("v2.db");
    {
        let conn = Connection::open(&v2_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE accounts (
                id TEXT PRIMARY KEY,
                provider TEXT NOT NULL,
                config_dir TEXT NOT NULL,
                cooldown_until INTEGER
            );
            INSERT INTO accounts VALUES ('claude-1', 'claude_code', '/home/u/.claude-work', 42);
            PRAGMA user_version = 2;",
        )
        .unwrap();
    }
    let ro = Db::open_read_only(&v2_path).unwrap();
    let accounts = ro.list_accounts().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].profile.id, "claude-1");
    assert_eq!(accounts[0].profile.provider, ProviderKind::ClaudeCode);
    assert_eq!(accounts[0].profile.name, "work");
    assert_eq!(accounts[0].cooldown_until, Some(42));
    drop(ro);

    // running_run_ids returns only runs with status = 'running'.
    let db = Db::open(&dir.path().join("v4.db")).unwrap();
    let mut task = Task::new_root(
        "t1",
        "/repo",
        "t",
        ImmutableSpec {
            description: "d".to_string(),
            acceptance: vec![],
        },
        Priority::P1,
        1,
    );
    task.state = TaskState::Running;
    db.insert_task(&task).unwrap();
    for (id, status) in [
        ("r-run", RunStatus::Running),
        ("r-done", RunStatus::Succeeded),
    ] {
        db.insert_run(&RunRecord {
            id: id.to_string(),
            task_id: "t1".to_string(),
            account_id: None,
            executor: "claude".to_string(),
            worktree_path: "/wt".to_string(),
            branch: "aaf/t1".to_string(),
            status,
            exit_code: None,
            tokens_in: 0,
            tokens_out: 0,
            started_at: 1,
            ended_at: None,
        })
        .unwrap();
    }
    let running = db.running_run_ids().unwrap();
    assert!(running.contains("r-run"));
    assert!(!running.contains("r-done"));
    assert_eq!(running.len(), 1);
}

/// The real machine probe with the CPU-load wait disabled, so these tests do
/// not stall (up to `heavy_wait_secs`) when the host is busy.
fn load_free_machine() -> MachineConfig {
    MachineConfig {
        max_load_percent: 0,
        ..MachineConfig::default()
    }
}
