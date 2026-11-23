#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Phase 0 Oracle tests for G6: Opt-in Red->Green & Vacuity Oracle in `aaf` Gate.
//!
//! Uses only existing public `aaf_engine::config::load` and `aaf_engine::gate::run_gate_throttled`
//! APIs on throwaway shell-script git repositories.

use std::fs;
use std::path::Path;
use std::process::Command;

use aaf_domain::config::MachineConfig;
use aaf_domain::gate::{GateMode, GateStatus, Receipt};
use aaf_domain::ports::Clock;
use aaf_engine::config::{ConfigInputs, ConfigOverlay, REPO_CONFIG_REL, load};
use aaf_engine::gate::run_gate_throttled;
use aaf_engine::throttle::{
    LoadProbe, Pressure, PriorityTools, SAMPLE_EVERY, SystemGroup, ThreadSleeper, Throttle,
};
use tempfile::TempDir;

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

struct FixedClock(i64);

impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "oracle-test")
        .env("GIT_AUTHOR_EMAIL", "oracle@example.invalid")
        .env("GIT_COMMITTER_NAME", "oracle-test")
        .env("GIT_COMMITTER_EMAIL", "oracle@example.invalid")
        .output()
        .expect("git invocation");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_oracle_repo(oracle_mode: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(root.join(".autofactory")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("scripts")).unwrap();
    fs::create_dir_all(root.join("tests")).unwrap();

    let cfg = format!(
        "[commands]\ntest_scoped = \"sh scripts/run_tests.sh\"\ntest_full = \"sh scripts/run_tests.sh\"\n\n[gate]\nbase_ref = \"main\"\nred_green_oracle = \"{oracle_mode}\"\n"
    );
    fs::write(root.join(REPO_CONFIG_REL), cfg).unwrap();
    fs::write(root.join("src/app.sh"), "#!/bin/sh\necho 0\n").unwrap();
    fs::write(
        root.join("scripts/run_tests.sh"),
        "#!/bin/sh\nset -e\nfor f in tests/test_*.sh; do\n  [ -f \"$f\" ] || continue\n  sh \"$f\"\ndone\n",
    )
    .unwrap();
    fs::write(
        root.join("tests/test_smoke.sh"),
        "#!/bin/sh\necho \"PASS: smoke\"\nexit 0\n",
    )
    .unwrap();

    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "base"]);
    git(root, &["checkout", "-q", "-b", "feat/work"]);
    dir
}

fn run_gate(repo_root: &Path, data_dir: &Path) -> Receipt {
    let no_env = |_: &str| None;
    let effective = load(&ConfigInputs {
        global_config: None,
        repo_root: repo_root.to_path_buf(),
        env: &no_env,
        cli: ConfigOverlay::default(),
    })
    .expect("config with [gate].red_green_oracle must load cleanly");

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
    run_gate_throttled(
        data_dir,
        repo_root,
        &effective.repo,
        GateMode::Scoped,
        &FixedClock(1000),
        &throttle,
    )
    .expect("gate run should succeed")
}

#[test]
fn oracle_proven_real_fix_and_red_to_green_test_is_clear() {
    let repo = init_oracle_repo("block");
    let data = TempDir::new().unwrap();

    fs::write(repo.path().join("src/app.sh"), "#!/bin/sh\necho 42\n").unwrap();
    fs::write(
        repo.path().join("tests/test_fix.sh"),
        "#!/bin/sh\nval=$(sh src/app.sh)\nif [ \"$val\" != \"42\" ]; then\n  echo \"FAIL: expected 42\"\n  exit 1\nfi\necho \"PASS: got 42\"\n",
    )
    .unwrap();

    let receipt = run_gate(repo.path(), data.path());
    assert_eq!(
        receipt.status,
        GateStatus::Clear,
        "proven red->green fix must be Clear, got {:?}",
        receipt.reasons
    );
    assert!(
        receipt
            .reasons
            .iter()
            .any(|r| r.contains("oracle: red_green=proven")),
        "expected proven oracle reason, got {:?}",
        receipt.reasons
    );
}

#[test]
fn oracle_vacuous_always_passing_test_is_blocked() {
    let repo = init_oracle_repo("block");
    let data = TempDir::new().unwrap();

    fs::write(repo.path().join("src/app.sh"), "#!/bin/sh\necho 42\n").unwrap();
    fs::write(
        repo.path().join("tests/test_vacuous.sh"),
        "#!/bin/sh\necho \"PASS: always green\"\nexit 0\n",
    )
    .unwrap();

    let receipt = run_gate(repo.path(), data.path());
    assert_eq!(
        receipt.status,
        GateStatus::Blocked,
        "vacuous test must be Blocked, got {:?}",
        receipt.reasons
    );
    assert!(
        receipt
            .reasons
            .iter()
            .any(|r| r.to_ascii_lowercase().contains("vacuous")),
        "expected vacuous reason, got {:?}",
        receipt.reasons
    );
}

#[test]
fn oracle_prod_change_without_test_change_is_blocked() {
    let repo = init_oracle_repo("block");
    let data = TempDir::new().unwrap();

    fs::write(repo.path().join("src/app.sh"), "#!/bin/sh\necho 42\n").unwrap();

    let receipt = run_gate(repo.path(), data.path());
    assert_eq!(
        receipt.status,
        GateStatus::Blocked,
        "prod change without test change must be Blocked, got {:?}",
        receipt.reasons
    );
    assert!(
        receipt
            .reasons
            .iter()
            .any(|r| r.to_ascii_lowercase().contains("no test")),
        "expected no-test reason, got {:?}",
        receipt.reasons
    );
}
