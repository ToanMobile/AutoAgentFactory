#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::process::Command;

use aaf_domain::config::{MachineConfig, RepoConfig};
use aaf_domain::gate::{GateMode, GateStatus, Receipt, Severity};
use aaf_domain::ports::Clock;
use aaf_engine::gate;
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
}

fn init_repo_on_feature() -> (TempDir, TempDir) {
    let repo = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    let root = repo.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn ok() -> u32 {\n    1\n}\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "base"]);
    git(root, &["checkout", "-q", "-b", "feature"]);
    (repo, data)
}

fn run_gate(repo_root: &Path, data_dir: &Path) -> Receipt {
    let mut cfg = RepoConfig::default();
    cfg.commands.lint = "true".to_string();
    cfg.commands.test_scoped = "true".to_string();
    let machine = MachineConfig::default();
    let throttle = calm_throttle(&machine);
    gate::run_gate_throttled(
        data_dir,
        repo_root,
        &cfg,
        GateMode::Scoped,
        &FixedClock(1_000),
        &throttle,
    )
    .unwrap()
}

fn lazy_pay_source() -> String {
    ["pub fn charge() {\n    to", "do!()\n}\n"].concat()
}

#[test]
fn gate011_case_a_committed_gitattributes_minus_diff_must_still_block_critical_static_finding() {
    let (repo, data) = init_repo_on_feature();
    let root = repo.path();

    fs::write(root.join("src/pay.rs"), lazy_pay_source()).unwrap();
    fs::write(root.join(".gitattributes"), "* -diff\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "add pay and gitattributes"]);

    let receipt = run_gate(root, data.path());
    assert_eq!(
        receipt.status,
        GateStatus::Blocked,
        "expected Blocked when src/pay.rs adds a critical LAZY-001 finding despite .gitattributes `* -diff`, got {:?} (findings: {:?}, reasons: {:?})",
        receipt.status,
        receipt.static_findings,
        receipt.reasons
    );
    assert!(
        receipt.static_findings.iter().any(|f| {
            f.severity == Severity::Critical && f.rule == "LAZY-001" && f.file == "src/pay.rs"
        }),
        "expected Critical LAZY-001 finding on src/pay.rs, got {:?}",
        receipt.static_findings
    );
}

#[test]
fn gate011_case_b_git_info_attributes_minus_diff_must_still_block_critical_static_finding() {
    let (repo, data) = init_repo_on_feature();
    let root = repo.path();

    fs::write(root.join("src/pay.rs"), lazy_pay_source()).unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "add pay"]);
    fs::write(root.join(".git/info/attributes"), "* -diff\n").unwrap();

    let receipt = run_gate(root, data.path());
    assert_eq!(
        receipt.status,
        GateStatus::Blocked,
        "expected Blocked when src/pay.rs adds a critical LAZY-001 finding despite .git/info/attributes `* -diff`, got {:?} (findings: {:?}, reasons: {:?})",
        receipt.status,
        receipt.static_findings,
        receipt.reasons
    );
    assert!(
        receipt.static_findings.iter().any(|f| {
            f.severity == Severity::Critical && f.rule == "LAZY-001" && f.file == "src/pay.rs"
        }),
        "expected Critical LAZY-001 finding on src/pay.rs, got {:?}",
        receipt.static_findings
    );
}
