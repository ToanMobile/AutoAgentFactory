//! `accounts.selection` on the real pieces: a real `Db`, real git
//! repositories and worktrees, the real gate, the router's real lease files
//! and the real `.claude/settings.local.json`. Only the agent is a fake, and
//! it writes a file so the task has a change to land.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::config::{AccountSelection, MachineConfig, RepoConfig};
use aaf_domain::event::EventKind;
use aaf_domain::ports::{
    AccountStore, AgentOutcome, AgentRequest, AgentRunner, EventSink, PortResult, RunStore,
    TaskStore,
};
use aaf_domain::run::{RunRecord, RunStatus};
use aaf_domain::task::{Priority, Task, TaskState};
use tempfile::TempDir;

use super::fakes::{FakeClock, FakeNotifier, FakeRunner, failed_outcome, limit_outcome, spec};
use super::git_real::git;
use crate::accounts::BindingSource;
use crate::db::Db;
use crate::factory::adapters::{RealGate, RouterPool, SystemClock};
use crate::factory::{
    AccountPool, Deps, Hooks, Mode, Purpose, RealGit, RunSummary, SelectingPool, Settings,
    resolve_base, run,
};

use AccountSelection::{LeastUsed, Pinned, Sticky};

/// The scripted fake agent, plus the one thing a real agent does that the
/// loop depends on: it leaves a change in its worktree.
#[derive(Default)]
struct WritingRunner {
    script: FakeRunner,
    files: Mutex<u32>,
}

impl AgentRunner for WritingRunner {
    fn run(&self, request: &AgentRequest) -> PortResult<AgentOutcome> {
        let outcome = self.script.run(request)?;
        if outcome.succeeded() {
            let mut files = self.files.lock().unwrap();
            *files += 1;
            fs::write(
                Path::new(&request.cwd).join(format!("made-{files}.txt")),
                "x\n",
            )
            .unwrap();
        }
        Ok(outcome)
    }
}

struct Lab {
    /// Removes everything when the test ends.
    _tmp: TempDir,
    root: PathBuf,
    db: Db,
    clock: Arc<FakeClock>,
    runner: Arc<WritingRunner>,
}

impl Lab {
    /// A machine with one Claude Code account per name, each with its own
    /// config dir.
    fn new(accounts: &[&str]) -> Lab {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        fs::create_dir_all(root.join("data")).unwrap();
        let db = Db::open(&root.join("data/aaf.db")).unwrap();
        for name in accounts {
            let dir = root.join(format!("home/.claude-{name}"));
            fs::create_dir_all(&dir).unwrap();
            db.upsert_account(&AccountProfile {
                id: (*name).to_string(),
                provider: ProviderKind::ClaudeCode,
                config_dir: dir.to_string_lossy().into_owned(),
                name: (*name).to_string(),
            })
            .unwrap();
        }
        Lab {
            _tmp: tmp,
            root,
            db,
            clock: Arc::new(FakeClock(AtomicI64::new(1_000))),
            runner: Arc::default(),
        }
    }

    fn repo(&self, name: &str) -> PathBuf {
        let repo = self.root.join(name);
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--quiet", "-b", "main"]);
        for (key, value) in [
            ("user.name", "aaf test"),
            ("user.email", "aaf@example.invalid"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", "/dev/null"),
        ] {
            git(&repo, &["config", key, value]);
        }
        fs::write(repo.join("app.txt"), "one\n").unwrap();
        // The owner's local settings are not part of the repository.
        fs::write(repo.join(".gitignore"), ".claude/\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "--quiet", "-m", "init"]);
        repo
    }

    fn pool(&self, repo: &Path, selection: AccountSelection) -> SelectingPool {
        let store = Arc::new(self.db.clone());
        SelectingPool::new(
            RouterPool {
                accounts: store.clone(),
                runs: store.clone(),
                clock: self.clock.clone(),
                leases_dir: self.root.join("data/leases"),
                max_concurrent_per_account: 1,
                provider: Some(ProviderKind::ClaudeCode),
            },
            selection,
            repo,
            store,
        )
    }

    /// One `aaf run` (once, auto-merge) of `repo` in `selection` mode.
    fn once(&self, repo: &Path, selection: AccountSelection) -> RunSummary {
        let name = repo.file_name().unwrap().to_string_lossy().into_owned();
        let mut cfg = RepoConfig::default();
        cfg.commands.test_scoped = "test -f app.txt".to_string();
        let deps = Deps {
            store: Arc::new(self.db.clone()),
            clock: self.clock.clone(),
            runner: self.runner.clone(),
            accounts: Arc::new(self.pool(repo, selection)),
            git: Arc::new(RealGit::new(
                repo.to_path_buf(),
                self.root.join("worktrees").join(&name),
                self.root.join("data/locks"),
                resolve_base(repo, "auto").unwrap(),
            )),
            gate: Arc::new(RealGate {
                data_dir: self.root.join("data"),
                cfg: cfg.clone(),
                clock: Arc::new(SystemClock),
                // Not the load of the machine that runs the tests.
                machine: MachineConfig {
                    max_load_percent: 0,
                    ..MachineConfig::default()
                },
                progress: Box::new(|_, _| {}),
                worktrees_dir: self.root.join("worktrees"),
            }),
            notifier: Arc::new(FakeNotifier::default()),
            settings: Settings {
                repo_root: repo.to_string_lossy().into_owned(),
                repo: cfg,
                max_parallel: 1,
                auto_merge: true,
                sandboxed: false,
                hook_required: false,
                agent_path: Default::default(),
                mobile: false,
                data_dir: self.root.join("data"),
                home: self.root.join("home"),
                owner_github_allowlist: Vec::new(),
            },
        };
        let hooks = Hooks {
            idle: &|| false,
            reap: &|_| Ok(()),
        };
        let summary = run(&deps, Mode::Once, &hooks).unwrap();
        assert!(summary.errors.is_empty(), "{:?}", summary.errors);
        summary
    }

    fn add(&self, repo: &Path, id: &str) {
        let root = repo.to_string_lossy();
        let mut task = Task::new_root(id, root, format!("task {id}"), spec(), Priority::P2, 1);
        task.state = TaskState::Ready;
        self.db.insert_task(&task).unwrap();
    }

    fn state(&self, id: &str) -> TaskState {
        self.db.get_task(id).unwrap().state
    }

    /// The account of every run of the task, oldest first.
    fn accounts(&self, id: &str) -> Vec<String> {
        let runs = self.db.runs_for_task(id).unwrap();
        runs.into_iter().filter_map(|r| r.account_id).collect()
    }

    fn script(&self, id: &str, outcomes: Vec<AgentOutcome>) {
        self.runner.script.script(id, outcomes);
    }

    /// A run of `task` on `account`, as the loop records one.
    fn ran(&self, task: &str, account: &str, executor: &str) {
        let now = self.clock.0.fetch_add(1, Ordering::SeqCst);
        self.db
            .insert_run(&RunRecord {
                id: format!("run-{task}-{now}"),
                task_id: task.to_string(),
                account_id: Some(account.to_string()),
                executor: executor.to_string(),
                worktree_path: String::new(),
                branch: String::new(),
                status: RunStatus::Succeeded,
                exit_code: Some(0),
                tokens_in: 0,
                tokens_out: 0,
                started_at: now,
                ended_at: Some(now),
            })
            .unwrap();
    }
}

fn granted(pool: &SelectingPool, task: &str, purpose: Purpose) -> Option<String> {
    let grant = pool.acquire_for(task, purpose).unwrap()?;
    grant.account_id().map(str::to_string)
}

#[test]
fn a_pinned_repository_keeps_one_account_for_every_run_and_every_task() {
    let lab = Lab::new(&["a", "b", "c"]);
    let repo = lab.repo("r1");
    lab.add(&repo, "t1");
    lab.script("t1", vec![failed_outcome("boom")]);

    // Two runs of one task: the first fails and is retried by the next run.
    let first = lab.once(&repo, Pinned);
    assert_eq!(first.requeued, vec!["t1"]);
    let second = lab.once(&repo, Pinned);
    assert_eq!(second.done, vec!["t1"]);
    // And a second task of the same repository.
    lab.add(&repo, "t2");
    assert_eq!(lab.once(&repo, Pinned).done, vec!["t2"]);

    // `b` and `c` were free and less used the whole time.
    assert_eq!(lab.accounts("t1"), ["a", "a"]);
    assert_eq!(lab.accounts("t2"), ["a"]);
    let binding = lab
        .pool(&repo, Pinned)
        .binding(lab.clock.0.load(Ordering::SeqCst));
    let binding = binding.unwrap();
    assert_eq!(binding.account.map(|a| a.id).as_deref(), Some("a"));
    assert_eq!(binding.source, Some(BindingSource::History));
}

#[test]
fn the_account_of_settings_local_json_wins_over_the_history() {
    let lab = Lab::new(&["a", "b", "c"]);
    let repo = lab.repo("r1");
    lab.add(&repo, "t1");
    assert_eq!(lab.once(&repo, Pinned).done, vec!["t1"]);
    assert_eq!(lab.accounts("t1"), ["a"]);

    fs::create_dir_all(repo.join(".claude")).unwrap();
    fs::write(
        repo.join(".claude/settings.local.json"),
        r#"{ "permissions": { "allow": [] }, "account": "c" }"#,
    )
    .unwrap();
    lab.add(&repo, "t2");
    assert_eq!(lab.once(&repo, Pinned).done, vec!["t2"]);

    assert_eq!(lab.accounts("t2"), ["c"]);
    let binding = lab.pool(&repo, Pinned).binding(1_000).unwrap();
    assert_eq!(binding.source, Some(BindingSource::Settings));
    assert_eq!(binding.settings_account.as_deref(), Some("c"));
    assert!(binding.settings_account_known);

    // A name no account carries binds nothing: the history decides again,
    // and the view says the settings entry was not understood.
    fs::write(
        repo.join(".claude/settings.local.json"),
        r#"{ "account": "nobody" }"#,
    )
    .unwrap();
    let binding = lab.pool(&repo, Pinned).binding(1_000).unwrap();
    assert_eq!(binding.account.map(|a| a.id).as_deref(), Some("c"));
    assert_eq!(binding.source, Some(BindingSource::History));
    assert!(!binding.settings_account_known);
}

#[test]
fn a_limit_hit_in_a_pinned_repository_waits_and_leases_no_other_account() {
    let lab = Lab::new(&["a", "b", "c"]);
    let repo = lab.repo("r1");
    lab.add(&repo, "t1");
    lab.script("t1", vec![limit_outcome(Some(9_000))]);

    let summary = lab.once(&repo, Pinned);

    assert_eq!(summary.agent_runs, 1, "no restart on another account");
    assert_eq!(summary.requeued, vec!["t1"]);
    assert_eq!(summary.waiting_for_account, 1);
    assert_eq!(lab.state("t1"), TaskState::Ready);
    assert_eq!(lab.accounts("t1"), ["a"]);
    // The reset time is on the board and in the binding.
    let events = lab.db.events_for_task("t1", 100).unwrap();
    let step = events
        .iter()
        .find(|e| e.kind == EventKind::UnblockStep)
        .unwrap();
    assert!(
        step.payload_json.contains("wait_for_account"),
        "{}",
        step.payload_json
    );
    assert!(
        events
            .iter()
            .any(|e| e.kind == EventKind::TaskStateChanged && e.payload_json.contains("9000")),
        "{events:?}"
    );
    let binding = lab.pool(&repo, Pinned).binding(1_000).unwrap();
    assert_eq!(binding.cooldown_until, Some(9_000));

    // The next `aaf run` still waits: `b` and `c` are free and stay unused.
    let waiting = lab.once(&repo, Pinned);
    assert_eq!(waiting.agent_runs, 0);
    assert_eq!(waiting.waiting_for_account, 1);
    assert_eq!(lab.accounts("t1"), ["a"]);

    // After the cooldown the task runs on its own account again.
    lab.clock.0.store(9_001, Ordering::SeqCst);
    assert_eq!(lab.once(&repo, Pinned).done, vec!["t1"]);
    assert_eq!(lab.accounts("t1"), ["a", "a"]);
}

#[test]
fn a_pinned_account_that_never_cools_down_is_not_tried_twice_in_one_run() {
    let lab = Lab::new(&["a", "b"]);
    let repo = lab.repo("r1");
    lab.add(&repo, "t1");
    // A reset time in the past leaves the account free again at once.
    lab.script("t1", vec![limit_outcome(Some(1)), limit_outcome(Some(1))]);

    let summary = lab.once(&repo, Pinned);

    assert_eq!(summary.agent_runs, 1);
    assert_eq!(summary.waiting_for_account, 1);
    assert_eq!(lab.accounts("t1"), ["a"]);
}

#[test]
fn two_repositories_are_bound_independently() {
    let lab = Lab::new(&["a", "b", "c"]);
    let (r1, r2) = (lab.repo("r1"), lab.repo("r2"));
    for (repo, id) in [(&r1, "t1"), (&r2, "u1"), (&r1, "t2"), (&r2, "u2")] {
        lab.add(repo, id);
        assert_eq!(lab.once(repo, Pinned).done, vec![id]);
    }

    // The second repository got the least-used account at its first run,
    // and each keeps its own.
    assert_eq!(lab.accounts("t1"), ["a"]);
    assert_eq!(lab.accounts("u1"), ["b"]);
    assert_eq!(lab.accounts("t2"), ["a"]);
    assert_eq!(lab.accounts("u2"), ["b"]);
}

#[test]
fn sticky_moves_on_a_limit_hit_and_then_stays_on_the_new_account() {
    let lab = Lab::new(&["a", "b", "c"]);
    let repo = lab.repo("r1");
    lab.add(&repo, "t1");
    assert_eq!(lab.once(&repo, Sticky).done, vec!["t1"]);
    assert_eq!(lab.accounts("t1"), ["a"]);

    // The repository prefers `a`; its limit moves the task in the same run.
    lab.add(&repo, "t2");
    lab.script("t2", vec![limit_outcome(None)]);
    let summary = lab.once(&repo, Sticky);
    assert_eq!(summary.done, vec!["t2"]);
    assert_eq!(summary.agent_runs, 2);
    assert_eq!(lab.accounts("t2"), ["a", "b"]);

    // `a` is free again, and so is the unused `c`: the next run stays.
    lab.clock.0.store(1_000_000, Ordering::SeqCst);
    lab.add(&repo, "t3");
    assert_eq!(lab.once(&repo, Sticky).done, vec!["t3"]);
    assert_eq!(lab.accounts("t3"), ["b"]);
}

#[test]
fn least_used_rotates_and_restarts_on_a_free_account_as_before() {
    let lab = Lab::new(&["a", "b", "c"]);
    let repo = lab.repo("r1");
    for id in ["t1", "t2", "t3"] {
        lab.add(&repo, id);
        assert_eq!(lab.once(&repo, LeastUsed).done, vec![id]);
    }
    assert_eq!(lab.accounts("t1"), ["a"]);
    assert_eq!(lab.accounts("t2"), ["b"]);
    assert_eq!(lab.accounts("t3"), ["c"]);

    lab.add(&repo, "t4");
    lab.script("t4", vec![limit_outcome(None)]);
    let summary = lab.once(&repo, LeastUsed);
    assert_eq!(summary.done, vec!["t4"]);
    assert_eq!(summary.agent_runs, 2);
    assert_eq!(lab.accounts("t4"), ["a", "b"]);
}

#[test]
fn limits_on_every_account_leave_a_least_used_task_waiting() {
    let lab = Lab::new(&["a", "b", "c"]);
    let repo = lab.repo("r1");
    lab.add(&repo, "t1");
    lab.script("t1", (0..3).map(|_| limit_outcome(Some(9_000))).collect());

    let summary = lab.once(&repo, LeastUsed);

    // The second hit still left `c`: three runs, not two, and only the
    // third is recorded as a wait.
    assert_eq!(summary.agent_runs, 3);
    assert_eq!(summary.waiting_for_account, 1);
    assert_eq!(lab.accounts("t1"), ["a", "b", "c"]);
    let events = lab.db.events_for_task("t1", 100).unwrap();
    let steps: Vec<bool> = events
        .iter()
        .rev()
        .filter(|e| e.kind == EventKind::UnblockStep)
        .map(|e| e.payload_json.contains("switch_account"))
        .collect();
    assert_eq!(steps, [true, true, false]);
}

#[test]
fn the_auditor_is_another_account_in_sticky_and_the_bound_one_in_pinned() {
    let lab = Lab::new(&["a", "b", "c"]);
    let repo = lab.repo("r1");
    lab.add(&repo, "t1");
    lab.ran("t1", "b", "claude");

    let sticky = lab.pool(&repo, Sticky);
    assert_eq!(granted(&sticky, "t1", Purpose::Work).as_deref(), Some("b"));
    assert_eq!(granted(&sticky, "t1", Purpose::Audit).as_deref(), Some("a"));
    // An audit run is not what the task or the repository "ran on".
    lab.ran("t1", "a", crate::cross_audit::AUDIT_EXECUTOR);
    assert_eq!(granted(&sticky, "t1", Purpose::Work).as_deref(), Some("b"));
    // Multi-account pool waits rather than falling back to the author's own account.
    lab.db.set_cooldown("a", Some(i64::MAX)).unwrap();
    lab.db.set_cooldown("c", Some(i64::MAX)).unwrap();
    assert_eq!(granted(&sticky, "t1", Purpose::Audit), None);
    lab.db.set_cooldown("a", None).unwrap();
    lab.db.set_cooldown("c", None).unwrap();

    // Pinned: one account for the repository, the audit included.
    let pinned = lab.pool(&repo, Pinned);
    assert_eq!(granted(&pinned, "t1", Purpose::Audit).as_deref(), Some("b"));
    // While another job of the repository holds it, the audit waits.
    let held = pinned.acquire_for("t1", Purpose::Work).unwrap().unwrap();
    assert_eq!(granted(&pinned, "t1", Purpose::Audit), None);
    drop(held);
}
