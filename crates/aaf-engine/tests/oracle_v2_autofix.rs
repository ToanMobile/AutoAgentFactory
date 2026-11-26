//! Five mandatory RED -> GREEN oracle tests for v2.1 Pre-Gate Deterministic
//! Formatter (`commands.autofix`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::config::{MachineConfig, RepoConfig};
use aaf_domain::gate::LockStatus;
use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, Clock, LimitHit, PortResult, TaskStore,
};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::config::{ConfigInputs, ConfigOverlay, load as load_config};
use aaf_engine::db::Db;
use aaf_engine::factory::adapters::{RealGate, SystemClock};
use aaf_engine::factory::snapshots::undo_latest_turn;
use aaf_engine::factory::{
    AccountPool, BaseRef, Deps, Grant, Hooks, Mode, Notifier, RealGit, Settings, run,
};
use aaf_engine::gate::autofix::{AUTOFIX_COMMIT_MSG, AutofixOutcome, run_autofix};
use aaf_engine::gate::{self, HiddenPolicy};
use aaf_engine::webhook::WebhookEvent;

struct FixedClock(AtomicI64);
impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct OneAccountPool(AccountProfile);
impl AccountPool for OneAccountPool {
    fn acquire(&self) -> PortResult<Option<Grant>> {
        Ok(Some(Grant::new(Some(self.0.clone()), None)))
    }
    fn report_limit(&self, _id: &str, _hit: &LimitHit) -> PortResult<()> {
        Ok(())
    }
    fn profiles(&self) -> PortResult<Vec<AccountProfile>> {
        Ok(vec![self.0.clone()])
    }
}

struct SilentNotifier;
impl Notifier for SilentNotifier {
    fn notify(&self, _event: &WebhookEvent) -> Result<bool, String> {
        Ok(true)
    }
}

struct DirtyFormatAgent;
impl AgentRunner for DirtyFormatAgent {
    fn run(&self, req: &AgentRequest) -> PortResult<AgentOutcome> {
        fs::write(Path::new(&req.cwd).join("src/a.rs"), "UNFORMATTED\n").unwrap();
        Ok(AgentOutcome {
            exit_code: Some(0),
            timed_out: false,
            final_text: "edited src/a.rs".to_string(),
            tokens_in: 10,
            tokens_out: 10,
            limit_hit: None,
            infra: None,
            log_tail: String::new(),
        })
    }
}

fn git_run(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Oracle")
        .env("GIT_AUTHOR_EMAIL", "oracle@example.com")
        .env("GIT_COMMITTER_NAME", "Oracle")
        .env("GIT_COMMITTER_EMAIL", "oracle@example.com")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_repo_with_autofix(repo: &Path, autofix_cmd: &str, lint_cmd: &str) -> RepoConfig {
    fs::create_dir_all(repo.join(".autofactory")).unwrap();
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::create_dir_all(repo.join("tests")).unwrap();
    git_run(repo, &["init", "-b", "main"]);
    fs::write(repo.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
    fs::write(repo.join("src/a.rs"), "INITIAL\n").unwrap();
    fs::write(repo.join("src/unrelated.rs"), "UNRELATED_ORIG\n").unwrap();
    fs::write(repo.join("tests/existing_test.rs"), "TEST_ORIG\n").unwrap();
    let cfg_toml = format!(
        "[project]\nname = \"demo\"\nprofile = \"rust\"\n\n\
         [commands]\nlint = {}\ntest_scoped = \"echo ok\"\nautofix = {}\n\n\
         [gate]\nbase_ref = \"main\"\n",
        toml::Value::String(lint_cmd.to_string()),
        toml::Value::String(autofix_cmd.to_string()),
    );
    fs::write(repo.join(".autofactory/config.toml"), &cfg_toml).unwrap();
    git_run(repo, &["add", "."]);
    git_run(repo, &["commit", "-m", "init"]);

    let mut cfg = RepoConfig::default();
    cfg.gate.base_ref = "main".to_string();
    cfg.commands.lint = lint_cmd.to_string();
    cfg.commands.test_scoped = "echo ok".to_string();
    cfg.commands.autofix = autofix_cmd.to_string();
    cfg.pipeline.plan_stage = false;
    cfg.pipeline.plan_debate = false;
    cfg.cross_audit.dual_review = false;
    cfg.cross_audit.score_gate = false;
    cfg
}

/// Oracle 1: Agent commits unformatted file that would fail `lint`; worker runs
/// `autofix`, creates separate `"aaf: autofix"` commit, clears gate on first
/// pass with 0 LLM rework, and `undo_latest_turn` rolls back BOTH commits.
#[test]
fn autofix_formats_unlocked_changed_file_and_clears_gate_first_pass() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    let worktrees_dir = tmp.path().join("worktrees");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&repo_dir).unwrap();
    fs::create_dir_all(&worktrees_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();

    let autofix_cmd = "printf 'FORMATTED\\n' > src/a.rs";
    let lint_cmd = "grep -qx 'FORMATTED' src/a.rs";
    let repo_cfg = init_repo_with_autofix(&repo_dir, autofix_cmd, lint_cmd);
    let base_sha = git_run(&repo_dir, &["rev-parse", "HEAD"]);

    let gate = Arc::new(RealGate {
        data_dir: data_dir.clone(),
        cfg: repo_cfg.clone(),
        clock: Arc::new(SystemClock),
        machine: MachineConfig {
            max_load_percent: 0,
            ..MachineConfig::default()
        },
        progress: Box::new(|_, _| {}),
        worktrees_dir: worktrees_dir.clone(),
    });
    let git = Arc::new(RealGit::new(
        repo_dir.clone(),
        worktrees_dir.clone(),
        data_dir.join("locks"),
        BaseRef {
            remote: None,
            branch: "main".to_string(),
        },
    ));

    let db = Db::open(&data_dir.join("aaf.db")).unwrap();
    let mut task = Task::new_root(
        "t1",
        repo_dir.to_str().unwrap(),
        "Format task",
        ImmutableSpec {
            description: "Update src/a.rs".to_string(),
            acceptance: vec!["lint passes".to_string()],
        },
        Priority::P1,
        1,
    );
    task.state = TaskState::Ready;
    db.insert_task(&task).unwrap();

    let deps = Deps {
        store: Arc::new(db.clone()),
        clock: Arc::new(FixedClock(AtomicI64::new(1_000))),
        runner: Arc::new(DirtyFormatAgent),
        accounts: Arc::new(OneAccountPool(AccountProfile {
            id: "acc1".to_string(),
            provider: ProviderKind::ClaudeCode,
            config_dir: tmp.path().join("acc1").to_string_lossy().into_owned(),
            name: "acc1".to_string(),
        })),
        git,
        gate,
        notifier: Arc::new(SilentNotifier),
        settings: Settings {
            repo_root: repo_dir.to_string_lossy().into_owned(),
            repo: repo_cfg,
            max_parallel: 1,
            auto_merge: false,
            sandboxed: false,
            hook_required: false,
            agent_path: Default::default(),
            mobile: false,
            data_dir: data_dir.clone(),
            home: tmp.path().to_path_buf(),
            owner_github_allowlist: Vec::new(),
        },
    };

    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    let summary = run(&deps, Mode::Once, &hooks).unwrap();
    assert_eq!(summary.agent_runs, 1, "must clear gate on first pass");
    assert_eq!(summary.review, vec!["t1"]);

    let wt_path = aaf_engine::git::worktree::worktree_path(&repo_dir, &worktrees_dir, "t1")
        .expect("worktree path");
    assert_eq!(
        fs::read_to_string(wt_path.join("src/a.rs")).unwrap(),
        "FORMATTED\n"
    );
    assert!(
        git_run(&wt_path, &["status", "--porcelain"]).is_empty(),
        "worktree must be clean after autofix commit"
    );
    let subjects = git_run(&wt_path, &["log", "--format=%s", "-n", "2"]);
    let lines: Vec<&str> = subjects.lines().collect();
    assert_eq!(lines[0], AUTOFIX_COMMIT_MSG);
    assert_eq!(lines[1], "aaf: Format task");

    // `undo_latest_turn` rolls back both the agent commit AND `"aaf: autofix"`.
    undo_latest_turn(&db, &data_dir, &repo_dir, &worktrees_dir, "t1", 2_000)
        .expect("undo_latest_turn");
    assert_eq!(git_run(&wt_path, &["rev-parse", "HEAD"]), base_sha);
    assert_eq!(
        fs::read_to_string(wt_path.join("src/a.rs")).unwrap(),
        "INITIAL\n"
    );
}

/// Oracle 2 (V2-SEC-03): When agent touches a head-locked file (`Cargo.toml`),
/// `autofix` MUST be `Skipped` and its command MUST NEVER run.
#[test]
fn autofix_skips_when_head_locked_file_modified_v2_sec_03() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let marker = tmp.path().join("ran.marker");
    let autofix_cmd = format!("touch '{}'", marker.display());
    let cfg = init_repo_with_autofix(&repo, &autofix_cmd, "true");

    git_run(&repo, &["checkout", "-b", "aaf/t-lock"]);
    fs::write(repo.join("Cargo.toml"), "[package]\nname = \"tampered\"\n").unwrap();
    fs::write(repo.join("src/a.rs"), "UNFORMATTED\n").unwrap();
    git_run(&repo, &["commit", "-am", "touch locked file"]);

    let outcome = run_autofix(
        &data_dir,
        &repo,
        &cfg,
        HiddenPolicy::Remove,
        &SystemClock,
        &MachineConfig::default(),
        &|_| {},
    )
    .expect("run_autofix");
    assert!(
        matches!(outcome, AutofixOutcome::Skipped { .. }),
        "{outcome:?}"
    );
    assert!(
        !marker.exists(),
        "autofix command must not execute on locked diff"
    );
}

/// Oracle 3: Scope quarantine keeps formatting ONLY on unlocked agent-changed
/// paths, restores unrelated & head-locked edits byte-for-byte, deletes
/// untracked files/symlinks, and preserves `head_lock == Clear`.
#[test]
fn autofix_reverts_out_of_scope_head_locked_and_untracked_files() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let autofix_cmd = "printf 'FORMATTED\\n' > src/a.rs && \
                       printf 'CORRUPT\\n' > src/unrelated.rs && \
                       printf 'TAMPERED_TEST\\n' > tests/existing_test.rs && \
                       printf 'JUNK\\n' > junk.txt && \
                       ln -s src/a.rs bad_link";
    let cfg = init_repo_with_autofix(&repo, autofix_cmd, "true");

    git_run(&repo, &["checkout", "-b", "aaf/t-scope"]);
    fs::write(repo.join("src/a.rs"), "UNFORMATTED\n").unwrap();
    git_run(&repo, &["commit", "-am", "edit only src/a.rs"]);

    let outcome = run_autofix(
        &data_dir,
        &repo,
        &cfg,
        HiddenPolicy::Remove,
        &SystemClock,
        &MachineConfig::default(),
        &|_| {},
    )
    .expect("run_autofix");
    assert_eq!(
        outcome,
        AutofixOutcome::Formatted {
            paths: vec!["src/a.rs".to_string()],
        }
    );
    assert_eq!(
        fs::read_to_string(repo.join("src/a.rs")).unwrap(),
        "FORMATTED\n"
    );
    assert_eq!(
        fs::read_to_string(repo.join("src/unrelated.rs")).unwrap(),
        "UNRELATED_ORIG\n"
    );
    assert_eq!(
        fs::read_to_string(repo.join("tests/existing_test.rs")).unwrap(),
        "TEST_ORIG\n"
    );
    assert!(!repo.join("junk.txt").exists());
    assert!(fs::symlink_metadata(repo.join("bad_link")).is_err());

    let hl = gate::head_lock::evaluate(&data_dir, &repo, &cfg, "main");
    assert_eq!(hl.status, LockStatus::Clear);
}

/// Oracle 4: Non-zero exit resets worktree cleanly to `pre_head` and returns
/// `Failed` so the worker proceeds directly into `Gating`.
#[test]
fn autofix_resets_cleanly_on_nonzero_exit() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let autofix_cmd = "printf 'HALF_WRITTEN\\n' > src/a.rs && printf 'junk' > junk.txt && exit 1";
    let cfg = init_repo_with_autofix(&repo, autofix_cmd, "true");

    git_run(&repo, &["checkout", "-b", "aaf/t-fail"]);
    fs::write(repo.join("src/a.rs"), "AGENT_ORIG\n").unwrap();
    git_run(&repo, &["commit", "-am", "agent edit"]);
    let pre_head = git_run(&repo, &["rev-parse", "HEAD"]);

    let outcome = run_autofix(
        &data_dir,
        &repo,
        &cfg,
        HiddenPolicy::Remove,
        &SystemClock,
        &MachineConfig::default(),
        &|_| {},
    )
    .expect("run_autofix");
    assert!(
        matches!(
            outcome,
            AutofixOutcome::Failed {
                exit_code: Some(1),
                timed_out: false,
                ..
            }
        ),
        "{outcome:?}"
    );
    assert_eq!(git_run(&repo, &["rev-parse", "HEAD"]), pre_head);
    assert_eq!(
        fs::read_to_string(repo.join("src/a.rs")).unwrap(),
        "AGENT_ORIG\n"
    );
    assert!(!repo.join("junk.txt").exists());
    assert!(git_run(&repo, &["status", "--porcelain"]).is_empty());
}

/// Oracle 5: Confines `$AAF_DATA_DIR` (macOS Seatbelt EPERM), rejects Tier-3
/// `config.local.toml` and `AAF_COMMANDS_AUTOFIX`, and keeps `config_hash`
/// byte-identical when `autofix` is empty.
#[test]
fn autofix_confines_data_dir_rejects_tier3_and_keeps_config_hash() {
    // 5a: `config_hash` byte-identical when `autofix` is empty (`skip_serializing_if`).
    let default_cfg = RepoConfig::default();
    let json = serde_json::to_string(&default_cfg.commands).unwrap();
    assert!(
        !json.contains("\"autofix\""),
        "empty autofix must be omitted from serialization to preserve config_hash: {json}"
    );
    let mut with_autofix = default_cfg.clone();
    with_autofix.commands.autofix = "cargo fmt".to_string();
    assert_ne!(
        gate::config_hash(&default_cfg),
        gate::config_hash(&with_autofix)
    );

    // 5b: Tier-3 `config.local.toml` and `AAF_COMMANDS_AUTOFIX` are rejected.
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join(".autofactory")).unwrap();
    fs::write(
        repo.join(".autofactory/config.toml"),
        "[project]\nname = \"demo\"\nprofile = \"rust\"\n",
    )
    .unwrap();
    fs::write(
        repo.join(".autofactory/config.local.toml"),
        "[commands]\nautofix = \"echo pwned\"\n",
    )
    .unwrap();
    let env = |k: &str| (k == "AAF_COMMANDS_AUTOFIX").then(|| "echo env_pwned".to_string());
    let eff = load_config(&ConfigInputs {
        global_config: None,
        repo_root: repo.clone(),
        env: &env,
        cli: ConfigOverlay::default(),
    })
    .expect("load config");
    assert!(eff.repo.commands.autofix.is_empty());
    assert!(
        eff.warnings
            .iter()
            .any(|w| w.contains("[commands] ignored")),
        "expected local [commands] warning: {:?}",
        eff.warnings
    );
    assert!(
        eff.warnings
            .iter()
            .any(|w| w.contains("AAF_COMMANDS_AUTOFIX")),
        "expected AAF_COMMANDS_AUTOFIX warning: {:?}",
        eff.warnings
    );

    // 5c: Seatbelt denies reading `$AAF_DATA_DIR/receipt.key` inside autofix.
    #[cfg(target_os = "macos")]
    {
        let sec_tmp = tempfile::tempdir().unwrap();
        let sec_repo = sec_tmp.path().join("repo");
        let data_dir = sec_tmp.path().join("data");
        fs::create_dir_all(&data_dir).unwrap();
        let secret_path = data_dir.join("receipt.key");
        fs::write(&secret_path, "SUPER_SECRET_KEY_MATERIAL").unwrap();

        let steal_cmd = format!("cat '{}' > src/a.rs", secret_path.display());
        let cfg = init_repo_with_autofix(&sec_repo, &steal_cmd, "true");
        git_run(&sec_repo, &["checkout", "-b", "aaf/t-sec"]);
        fs::write(sec_repo.join("src/a.rs"), "SAFE\n").unwrap();
        git_run(&sec_repo, &["commit", "-am", "safe change"]);

        let outcome = run_autofix(
            &data_dir,
            &sec_repo,
            &cfg,
            HiddenPolicy::Remove,
            &SystemClock,
            &MachineConfig::default(),
            &|_| {},
        )
        .expect("run_autofix");
        assert!(
            matches!(outcome, AutofixOutcome::Failed { .. }),
            "Seatbelt must block reading receipt.key: {outcome:?}"
        );
        let content = fs::read_to_string(sec_repo.join("src/a.rs")).unwrap();
        assert!(!content.contains("SUPER_SECRET_KEY_MATERIAL"));
        assert_eq!(content, "SAFE\n");
    }
}
