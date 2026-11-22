#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The gate as other crates see it: only public items, on a real git repo.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use aaf_domain::config::{MachineConfig, RepoConfig};
use aaf_domain::gate::{GateMode, GateStatus, LockStatus};
use aaf_domain::ports::Clock;
use aaf_engine::gate::{self, head_lock};
use aaf_engine::throttle::{
    LoadProbe, Pressure, PriorityTools, SAMPLE_EVERY, SystemGroup, ThreadSleeper, Throttle,
};
use tempfile::TempDir;

/// An idle machine. With the real probe the heavy command below would wait
/// for as long as the machine running this test is busy.
struct Calm;

impl LoadProbe for Calm {
    fn load_percent(&self) -> Option<u32> {
        Some(0)
    }

    fn pressure(&self) -> Option<Pressure> {
        Some(Pressure::Normal)
    }

    fn free_memory_bytes(&self) -> Option<u64> {
        Some(u64::MAX)
    }
}

struct At(i64);

impl Clock for At {
    fn now(&self) -> i64 {
        self.0
    }
}

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

#[test]
fn gate_approve_gate_round_trip_through_the_public_api() {
    let repo = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    let root = repo.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(root.join("tests")).unwrap();
    fs::write(root.join("tests/smoke_test.py"), "assert True\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "base"]);
    fs::write(root.join("tests/smoke_test.py"), "assert 1 == 1\n").unwrap();

    let mut cfg = RepoConfig::default();
    cfg.commands.test_full = "true".to_string();
    let clock = At(77);

    let blocked = gate::run_gate_in(data.path(), root, &cfg, GateMode::Release, &clock).unwrap();
    assert_eq!(blocked.status, GateStatus::Blocked);
    assert_eq!(blocked.status.exit_code(), 2);
    let base_sha = blocked.base_sha.clone().unwrap();
    let report = head_lock::evaluate(data.path(), root, &cfg, &base_sha);
    assert_eq!(report, blocked.head_lock);

    let approval =
        head_lock::approve(data.path(), root, &cfg, "second-agent", true, &clock).unwrap();
    assert_eq!(approval.lock_fp, report.lock_fp);
    assert!(approval.same_account);

    let machine = MachineConfig::default();
    let throttle = Throttle {
        machine: &machine,
        probe: &Calm,
        sleeper: &ThreadSleeper,
        control: &SystemGroup,
        progress: &|_| {},
        cpus: 4,
        tools: PriorityTools::detect(),
        jitter_seed: 1,
        sample_every: SAMPLE_EVERY,
    };
    let clear = gate::run_gate_throttled(
        data.path(),
        root,
        &cfg,
        GateMode::Release,
        &clock,
        &throttle,
    )
    .unwrap();
    assert_eq!(clear.status, GateStatus::Clear, "{:?}", clear.reasons);
    assert_eq!(clear.head_lock.status, LockStatus::Approved);
    assert_eq!(clear.config_hash, gate::config_hash(&cfg));

    let key = aaf_fp::load_key(data.path()).unwrap();
    let stored = aaf_fp::read_receipt(data.path(), &key, root).unwrap();
    assert_eq!(stored, clear);
    let now = aaf_fp::tree_fp(root, Duration::from_secs(30)).unwrap();
    assert_eq!(stored.tree_fp, now);
}
