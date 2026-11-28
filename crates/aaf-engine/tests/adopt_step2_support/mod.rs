//! Shared by the `adopt_step2_*` merge tests: a repository with a bare
//! origin, a `gh` that answers for one PR, and helpers to branch and merge.

#![allow(dead_code)] // each test crate uses its own part of this module

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aaf_domain::config::MachineConfig;
use aaf_domain::gate::GateMode;
use aaf_domain::ports::Clock;
use aaf_engine::git::{GhClient, GitError, MergeStrategy, PrCheck, PrInfo};
use aaf_engine::pr_ops::{PrMergeOutcome, PrMergeRequest, merge_pr, merge_pr_with_check};
use tempfile::TempDir;

pub struct FixedClock(pub i64);
impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

pub fn git(dir: &Path, args: &[&str]) -> String {
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

pub fn ref_exists(dir: &Path, name: &str) -> bool {
    Command::new("git")
        .args(["show-ref", "--verify", "--quiet", name])
        .current_dir(dir)
        .status()
        .unwrap()
        .success()
}

/// A `gh` that answers for one PR and counts the merges asked of it.
pub struct FakeGh {
    state: Mutex<PrInfo>,
    merges: AtomicUsize,
}

impl FakeGh {
    pub fn new(number: u64, branch: &str, head: &str) -> Self {
        FakeGh {
            state: Mutex::new(PrInfo {
                number,
                state: "OPEN".to_string(),
                head_ref_oid: head.to_string(),
                head_ref_name: branch.to_string(),
                base_ref_name: "main".to_string(),
                mergeable: "MERGEABLE".to_string(),
                title: format!("PR #{number}"),
                url: format!("https://github.com/org/repo/pull/{number}"),
                ..Default::default()
            }),
            merges: AtomicUsize::new(0),
        }
    }

    pub fn merged_at(self, head: &str) -> Self {
        {
            let mut pr = self.state.lock().unwrap();
            pr.state = "MERGED".to_string();
            pr.head_ref_oid = head.to_string();
        }
        self
    }

    pub fn merges(&self) -> usize {
        self.merges.load(Ordering::SeqCst)
    }
}

impl GhClient for FakeGh {
    fn view_pr(&self, _: &Path, _: u64) -> Result<PrInfo, GitError> {
        Ok(self.state.lock().unwrap().clone())
    }

    fn pr_checks(&self, _: &Path, _: u64) -> Result<Vec<PrCheck>, GitError> {
        Ok(Vec::new())
    }

    fn merge_pr(
        &self,
        _: &Path,
        _: u64,
        _: &str,
        _: MergeStrategy,
        _: Option<&str>,
        _: Option<&Path>,
    ) -> Result<(), GitError> {
        self.merges.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

pub struct Fx {
    bare: TempDir,
    pub work: TempDir,
    pub data: TempDir,
    out: TempDir,
}

impl Fx {
    /// `test_scoped` is the base config's test command; `{LOG}` becomes a
    /// file that gets one line per run of it.
    pub fn new(test_scoped: &str) -> Self {
        Fx::with_gate(test_scoped, "")
    }

    /// [`Fx::new`] with extra lines under `[gate]`.
    pub fn with_gate(test_scoped: &str, gate_extra: &str) -> Self {
        let bare = TempDir::new().unwrap();
        git(bare.path(), &["init", "--bare", "-b", "main"]);
        let work = TempDir::new().unwrap();
        let out = TempDir::new().unwrap();
        git(work.path(), &["init", "-b", "main"]);
        git(
            work.path(),
            &["remote", "add", "origin", bare.path().to_str().unwrap()],
        );
        for d in [".autofactory", "src", "tests"] {
            fs::create_dir_all(work.path().join(d)).unwrap();
        }
        let log = out.path().join("log");
        let cmd = test_scoped.replace("{LOG}", &log.to_string_lossy());
        let cfg = format!(
            "[project]\nname = \"adopt\"\nprofile = \"rust\"\n\n[commands]\ntest_scoped = \"{cmd}\"\n\
             test_full = \"true\"\n\n[gate]\nbase_ref = \"origin/main\"\n{gate_extra}\n\n[cross_audit]\n\
             require_different_provider = false\n"
        );
        fs::write(work.path().join(".autofactory/config.toml"), cfg).unwrap();
        fs::write(
            work.path().join("Cargo.toml"),
            "[package]\nname = \"adopt\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(work.path().join("src/lib.rs"), "pub fn v() -> i32 { 1 }\n").unwrap();
        git(work.path(), &["add", "."]);
        git(work.path(), &["commit", "-m", "init"]);
        git(work.path(), &["push", "-u", "origin", "main"]);
        Fx {
            bare,
            work,
            data: TempDir::new().unwrap(),
            out,
        }
    }

    pub fn counting() -> Self {
        Fx::new("echo ran >> '{LOG}'")
    }

    pub fn ran(&self) -> usize {
        fs::read_to_string(self.out.path().join("log"))
            .unwrap_or_default()
            .lines()
            .count()
    }

    pub fn bare(&self) -> PathBuf {
        self.bare.path().to_path_buf()
    }

    pub fn branch(&self, name: &str, edit: impl FnOnce(&Path)) -> String {
        git(self.work.path(), &["checkout", "-b", name, "origin/main"]);
        edit(self.work.path());
        git(self.work.path(), &["add", "-A"]);
        git(self.work.path(), &["commit", "-m", name]);
        let sha = git(self.work.path(), &["rev-parse", "HEAD"]);
        git(self.work.path(), &["push", "-u", "origin", name]);
        git(self.work.path(), &["checkout", "main"]);
        sha
    }

    pub fn edit_lib(&self, name: &str) -> String {
        self.branch(name, |root| {
            fs::write(root.join("src/lib.rs"), "pub fn v() -> i32 { 2 }\n").unwrap();
        })
    }

    pub fn lock_cargo(&self, name: &str) -> String {
        self.branch(name, |root| {
            fs::write(
                root.join("Cargo.toml"),
                "[package]\nname = \"adopt\"\nversion = \"0.2.0\"\n",
            )
            .unwrap();
        })
    }

    pub fn req(&self, pr: u64, head: &str, dry_run: bool) -> PrMergeRequest {
        PrMergeRequest {
            repo_root: self.work.path().to_path_buf(),
            data_dir: self.data.path().to_path_buf(),
            pr_number: pr,
            expect_head: head.to_string(),
            remote: "origin".to_string(),
            mode: GateMode::Scoped,
            strategy: MergeStrategy::Squash,
            subject: None,
            body_file: None,
            ci_wait_secs: 0,
            require_checks: false,
            lock_timeout: Duration::from_secs(10),
            dry_run,
            machine: MachineConfig {
                max_load_percent: 0,
                ..MachineConfig::default()
            },
            global_config: None,
            no_receipt_reuse: false,
            run_commands_despite_head_lock: false,
            strict_merged_head: false,
            keep_non_aaf_local: false,
        }
    }

    pub fn run(&self, req: &PrMergeRequest, gh: &FakeGh) -> PrMergeOutcome {
        merge_pr(req, gh, &FixedClock(1_000), &|_| {}).unwrap()
    }

    pub fn run_checked(
        &self,
        req: &PrMergeRequest,
        gh: &FakeGh,
        check: Option<&dyn Fn() -> bool>,
    ) -> PrMergeOutcome {
        merge_pr_with_check(req, None, gh, &FixedClock(1_000), &|_| {}, check)
            .unwrap()
            .0
    }
}

pub fn code_of(out: &PrMergeOutcome) -> String {
    match out {
        PrMergeOutcome::Blocked { code, .. } => code.clone(),
        other => format!("not blocked: {other:?}"),
    }
}
