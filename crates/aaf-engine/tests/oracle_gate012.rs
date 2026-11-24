#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Oracle test for GATE-012:
//! A direct (non-symbolic) `refs/remotes/origin/HEAD` pointing to `HEAD` must
//! not let `gate.base_ref = "auto"` collapse the diff range to empty and
//! bypass head-lock on committed edits to locked test files.

use std::fs;
use std::path::Path;
use std::process::Command;

use aaf_domain::config::{MachineConfig, RepoConfig};
use aaf_domain::gate::{GateMode, GateStatus, LockStatus};
use aaf_domain::ports::Clock;
use aaf_engine::factory::{BaseRef, resolve_base};
use aaf_engine::gate;
use tempfile::TempDir;

#[path = "throttle_support.rs"]
mod throttle_support;
use throttle_support::calm_throttle;

struct At(i64);

impl Clock for At {
    fn now(&self) -> i64 {
        self.0
    }
}

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
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
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn forged_direct_origin_head_must_not_bypass_head_lock() {
    let origin = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    let root = repo.path();

    // Base commit on `main` pushed to bare `origin` (`origin/main`) containing a
    // head-locked test file.
    git(origin.path(), &["init", "-q", "--bare", "-b", "main"]);
    git(root, &["init", "-q", "-b", "main"]);
    git(
        root,
        &["remote", "add", "origin", origin.path().to_str().unwrap()],
    );
    fs::create_dir_all(root.join("tests")).unwrap();
    fs::write(
        root.join("tests/a_test.rs"),
        "fn checks() {\n    assert_eq!(1, 1);\n}\n",
    )
    .unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "base"]);
    git(root, &["push", "-q", "-u", "origin", "main"]);
    let base_sha = git(root, &["rev-parse", "origin/main"]);

    // Agent works on a branch, weakens the locked test file, and commits it.
    git(root, &["checkout", "-q", "-b", "feature"]);
    fs::write(
        root.join("tests/a_test.rs"),
        "fn checks() {\n    // weakened\n}\n",
    )
    .unwrap();
    git(root, &["add", "tests/a_test.rs"]);
    git(root, &["commit", "-q", "-m", "weaken locked test"]);
    let head_sha = git(root, &["rev-parse", "HEAD"]);
    assert_ne!(head_sha, base_sha);

    // Agent writes HEAD's SHA directly into `refs/remotes/origin/HEAD` (non-symref).
    git(
        root,
        &[
            "update-ref",
            "--no-deref",
            "refs/remotes/origin/HEAD",
            "HEAD",
        ],
    );

    // Factory's `resolve_base` uses `symbolic-ref`, so it ignores the direct
    // ref and falls back to `origin/main`.
    let factory_base = resolve_base(root, "auto").unwrap();
    assert_eq!(
        factory_base,
        BaseRef {
            remote: Some("origin".to_string()),
            branch: "main".to_string(),
        }
    );

    // Gate runs with `gate.base_ref = "auto"` and passing commands.
    let mut cfg = RepoConfig::default();
    cfg.gate.base_ref = "auto".to_string();
    cfg.commands.lint = "true".to_string();
    cfg.commands.test_scoped = "true".to_string();

    let machine = MachineConfig::default();
    let throttle = calm_throttle(&machine);
    let receipt = gate::run_gate_throttled(
        data.path(),
        root,
        &cfg,
        GateMode::Scoped,
        &At(1_000),
        &throttle,
    )
    .unwrap();

    // Expected secure behavior: gate must still diff against `origin/main` and
    // block on the unapproved edit to `tests/a_test.rs`.
    assert_eq!(
        receipt.status,
        GateStatus::Blocked,
        "gate must block on modified locked test; got status={:?}, head_lock={:?}, base_sha={:?} (real base={base_sha}, HEAD={head_sha})",
        receipt.status,
        receipt.head_lock.status,
        receipt.base_sha
    );
    assert_eq!(receipt.head_lock.status, LockStatus::Blocked);
    assert_eq!(receipt.base_sha.as_deref(), Some(base_sha.as_str()));
}
