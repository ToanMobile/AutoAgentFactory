#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use aaf_domain::config::{MachineConfig, ProofConfig, RepoConfig};
use aaf_domain::gate::{GateMode, GateStatus};
use aaf_domain::ports::Clock;
use aaf_engine::cross_audit::compute_mechanical_score;
use aaf_engine::factory::adapters::RealGate;
use aaf_engine::factory::ports::GateRunner;
use aaf_engine::gate::proof_need::proof_needed_for_worktree;
use aaf_engine::gate::run_gate_observed;
use tempfile::TempDir;

struct FixedClock(i64);

impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_repo(dir: &Path) -> String {
    git(dir, &["init", "-b", "main"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test User"]);
    fs::write(dir.join("README.md"), "base\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-m", "init"]);
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn android_proof_cfg() -> ProofConfig {
    ProofConfig {
        required: true,
        trigger_globs: vec![
            "**/*.kt".to_string(),
            "**/*.xml".to_string(),
            "**/res/**".to_string(),
        ],
        ..ProofConfig::default()
    }
}

#[test]
fn proof_needed_for_worktree_respects_trigger_globs_and_fails_closed() {
    let repo = TempDir::new().unwrap();
    let base_sha = init_repo(repo.path());
    let cfg = android_proof_cfg();

    // Non-UI diff (web admin tool + Python test) must NOT require device proof.
    fs::create_dir_all(repo.path().join("tools")).unwrap();
    fs::create_dir_all(repo.path().join("scripts/xe")).unwrap();
    fs::write(repo.path().join("tools/admin-keys.html"), "<p>ok</p>\n").unwrap();
    fs::write(repo.path().join("scripts/xe/analyze.py"), "print('ok')\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-m", "non-ui changes"]);

    assert!(!proof_needed_for_worktree(
        repo.path(),
        &cfg,
        "main",
        Some(&base_sha)
    ));

    // Empty trigger_globs with required=true always requires proof.
    let always_cfg = ProofConfig {
        required: true,
        trigger_globs: Vec::new(),
        ..ProofConfig::default()
    };
    assert!(proof_needed_for_worktree(
        repo.path(),
        &always_cfg,
        "main",
        Some(&base_sha)
    ));

    // Non-git directory fails closed to `true` when required + trigger_globs set.
    let non_git = TempDir::new().unwrap();
    assert!(proof_needed_for_worktree(
        non_git.path(),
        &cfg,
        "main",
        Some(&base_sha)
    ));

    // Touching an Android layout/Kotlin file triggers proof requirement.
    fs::create_dir_all(repo.path().join("app/src/main/java")).unwrap();
    fs::write(
        repo.path().join("app/src/main/java/Screen.kt"),
        "class Screen\n",
    )
    .unwrap();
    assert!(proof_needed_for_worktree(
        repo.path(),
        &cfg,
        "main",
        Some(&base_sha)
    ));
}

#[test]
fn real_gate_verify_receipt_and_mechanical_score_pass_for_non_ui_diff_under_trigger_globs() {
    let repo = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    let worktrees = TempDir::new().unwrap();
    let base_sha = init_repo(repo.path());

    // Branch off main with only a non-UI script change (like GeelyEx2 admin/VHAL tasks).
    git(repo.path(), &["checkout", "-b", "aaf/non-ui-task"]);
    fs::create_dir_all(repo.path().join("scripts")).unwrap();
    fs::write(repo.path().join("scripts/check.py"), "assert 1 == 1\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-m", "script change"]);

    let mut cfg = RepoConfig::default();
    cfg.gate.base_ref = "main".to_string();
    cfg.commands.test_scoped = "true".to_string();
    cfg.commands.lint = "true".to_string();
    cfg.proof = android_proof_cfg();

    let clock = FixedClock(1_795_680_000);
    let machine = MachineConfig::default();
    let receipt = run_gate_observed(
        data.path(),
        repo.path(),
        &cfg,
        GateMode::Scoped,
        &clock,
        &machine,
        &|_| {},
    )
    .unwrap();
    assert_eq!(receipt.status, GateStatus::Clear, "{:?}", receipt.reasons);
    assert_eq!(receipt.base_sha.as_deref(), Some(base_sha.as_str()));

    // Proof is not triggered for this non-UI diff, so mechanical score must not hard-zero.
    let proof_ok = !proof_needed_for_worktree(
        repo.path(),
        &cfg.proof,
        &cfg.gate.base_ref,
        receipt.base_sha.as_deref(),
    );
    assert!(proof_ok);
    let mech = compute_mechanical_score(&receipt, proof_ok, false, false);
    assert_eq!(mech.score, 100, "{:?}", mech.reasons);

    // RealGate::verify_cached_receipt must accept the Clear receipt without requiring a PNG proof.
    let adapter = RealGate {
        data_dir: data.path().to_path_buf(),
        cfg: cfg.clone(),
        clock: Arc::new(FixedClock(1_795_680_000)),
        machine,
        progress: Box::new(|_, _| {}),
        worktrees_dir: worktrees.path().to_path_buf(),
    };
    assert!(
        adapter.verify_cached_receipt(repo.path(), &receipt),
        "verify_cached_receipt must honor proof.trigger_globs for non-UI diffs"
    );

    // Once a Kotlin file matching trigger_globs is added, proof becomes required
    // and verify_cached_receipt must fail closed without a recorded proof.
    fs::create_dir_all(repo.path().join("app/src")).unwrap();
    fs::write(repo.path().join("app/src/Ui.kt"), "class Ui\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-m", "ui change"]);
    assert!(proof_needed_for_worktree(
        repo.path(),
        &cfg.proof,
        &cfg.gate.base_ref,
        Some(&base_sha)
    ));
    assert!(!adapter.verify_cached_receipt(repo.path(), &receipt));
}
