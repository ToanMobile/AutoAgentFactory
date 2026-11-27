#![allow(clippy::unwrap_used, clippy::expect_used)]
//! SEC-135: `fetch_base_and_pr` must not fetch a PR head through a pre-planted
//! loose symref under `refs/aaf-merge/pr-*` into `refs/heads/main`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use aaf_domain::config::MachineConfig;
use aaf_domain::gate::GateMode;
use aaf_domain::ports::Clock;
use aaf_engine::git::{GhClient, GitError, MergeStrategy, PrCheck, PrInfo};
use aaf_engine::pr_ops::{PrMergeOutcome, PrMergeRequest, merge_pr};
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
        Ok(())
    }
}

struct Fixture {
    bare: TempDir,
    work: TempDir,
    data: TempDir,
    main_oid: String,
}

impl Fixture {
    fn new() -> Self {
        let bare = TempDir::new().unwrap();
        git(bare.path(), &["init", "--bare", "-b", "main"]);

        let work = TempDir::new().unwrap();
        git(work.path(), &["init", "-b", "main"]);
        git(work.path(), &["config", "user.email", "dev@example.com"]);
        git(work.path(), &["config", "user.name", "Dev"]);
        git(
            work.path(),
            &["remote", "add", "origin", bare.path().to_str().unwrap()],
        );

        fs::create_dir_all(work.path().join(".autofactory")).unwrap();
        fs::create_dir_all(work.path().join("src")).unwrap();
        let cfg = "[project]\nname = \"demo\"\nprofile = \"rust\"\n\n\
                   [commands]\ntest_scoped = \"true\"\ntest_full = \"true\"\n\n\
                   [gate]\nbase_ref = \"origin/main\"\ndocs_fast_path = true\n";
        fs::write(work.path().join(".autofactory/config.toml"), cfg).unwrap();
        fs::write(work.path().join("src/lib.rs"), "pub fn v() -> i32 { 1 }\n").unwrap();
        git(work.path(), &["add", "."]);
        git(work.path(), &["commit", "-m", "initial"]);
        git(work.path(), &["push", "-u", "origin", "main"]);
        let main_oid = git(work.path(), &["rev-parse", "refs/heads/main"]);

        Self {
            bare,
            work,
            data: TempDir::new().unwrap(),
            main_oid,
        }
    }

    /// Creates a PR commit in a separate clone and pushes ONLY to `refs/pull/<n>/head`
    /// on the bare remote, so `self.work` does not have the commit locally and must fetch it.
    fn push_remote_pr_head(&self, pr_number: u64, content: &str) -> String {
        let contrib = TempDir::new().unwrap();
        git(
            contrib.path(),
            &["clone", self.bare.path().to_str().unwrap(), "."],
        );
        git(contrib.path(), &["config", "user.email", "pr@example.com"]);
        git(contrib.path(), &["config", "user.name", "PR Author"]);
        fs::write(contrib.path().join("src/lib.rs"), content).unwrap();
        git(contrib.path(), &["add", "-A"]);
        git(contrib.path(), &["commit", "-m", "pr head"]);
        let pr_oid = git(contrib.path(), &["rev-parse", "HEAD"]);
        let refspec = format!("HEAD:refs/pull/{pr_number}/head");
        git(contrib.path(), &["push", "origin", &refspec]);
        pr_oid
    }

    fn common_dir(&self) -> PathBuf {
        let raw = git(self.work.path(), &["rev-parse", "--git-common-dir"]);
        let p = PathBuf::from(raw);
        if p.is_absolute() {
            p
        } else {
            self.work.path().join(p)
        }
    }

    fn req(&self, pr_number: u64, expect_head: &str) -> PrMergeRequest {
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
            dry_run: true,
            machine: MachineConfig {
                max_load_percent: 0,
                ..MachineConfig::default()
            },
            global_config: None,
        }
    }
}

fn pr_info(number: u64, head: &str) -> PrInfo {
    PrInfo {
        number,
        state: "OPEN".to_string(),
        head_ref_oid: head.to_string(),
        head_ref_name: format!("feat/pr-{number}"),
        base_ref_name: "main".to_string(),
        mergeable: "MERGEABLE".to_string(),
        title: format!("PR #{number}"),
        url: format!("https://github.com/org/repo/pull/{number}"),
        ..Default::default()
    }
}

#[test]
fn pr_fetch_into_planted_symref_does_not_move_main() {
    let fx = Fixture::new();
    let pr_number = 135;
    let pr_head = fx.push_remote_pr_head(pr_number, "pub fn v() -> i32 { 135 }\n");
    assert_ne!(pr_head, fx.main_oid);

    let pid = std::process::id();
    let merge_refs_dir = fx.common_dir().join("refs/aaf-merge");
    fs::create_dir_all(&merge_refs_dir).unwrap();
    for seq in 0..64u64 {
        let name = format!("pr-{pr_number}-{pid}-{seq}");
        fs::write(merge_refs_dir.join(name), "ref: refs/heads/main\n").unwrap();
    }

    let gh = FakeGh {
        pr: pr_info(pr_number, &pr_head),
    };
    let noop = |_: &aaf_engine::throttle::Progress<'_>| {};
    let _ = merge_pr(&fx.req(pr_number, &pr_head), &gh, &FixedClock(1), &noop);

    let main_after = git(fx.work.path(), &["rev-parse", "refs/heads/main"]);
    assert_eq!(
        main_after, fx.main_oid,
        "refs/heads/main moved from {} to {} (PR head {}) via planted refs/aaf-merge symref",
        fx.main_oid, main_after, pr_head
    );
}

#[test]
fn pr_fetch_without_planted_ref_still_fetches() {
    let fx = Fixture::new();
    let pr_number = 136;
    let pr_head = fx.push_remote_pr_head(pr_number, "pub fn v() -> i32 { 136 }\n");
    assert_ne!(pr_head, fx.main_oid);

    let gh = FakeGh {
        pr: pr_info(pr_number, &pr_head),
    };
    let noop = |_: &aaf_engine::throttle::Progress<'_>| {};
    let out = merge_pr(&fx.req(pr_number, &pr_head), &gh, &FixedClock(2), &noop).unwrap();

    match out {
        PrMergeOutcome::DryRunClear {
            pr_number: got_pr,
            head_sha,
            base_sha,
            ..
        } => {
            assert_eq!(got_pr, pr_number);
            assert_eq!(head_sha, pr_head);
            assert_eq!(base_sha, fx.main_oid);
        }
        other => panic!("expected DryRunClear, got {other:?}"),
    }

    let main_after = git(fx.work.path(), &["rev-parse", "refs/heads/main"]);
    assert_eq!(
        main_after, fx.main_oid,
        "dry-run PR verification must not move refs/heads/main"
    );
}
