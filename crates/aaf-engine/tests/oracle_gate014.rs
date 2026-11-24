#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::process::Command;

use aaf_domain::config::{GateConfig, MachineConfig, RepoConfig};
use aaf_domain::gate::{GateMode, GateStatus};
use aaf_domain::ports::Clock;
use aaf_engine::gate::docs_fast::DOCS_FAST_PATH_NOTE;
use aaf_engine::gate::run_gate_with;
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
}

#[test]
fn gate014_markdown_embedded_via_include_str_must_not_take_docs_fast_path() {
    let repo = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();

    git(repo.path(), &["init", "-b", "main"]);
    git(repo.path(), &["config", "user.email", "test@example.com"]);
    git(repo.path(), &["config", "user.name", "Test"]);

    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::create_dir_all(repo.path().join("templates/skills/mobile-proof-qa")).unwrap();
    fs::write(
        repo.path().join("src/skills.rs"),
        "pub const MOBILE_PROOF_QA: &str = include_str!(\"../templates/skills/mobile-proof-qa/SKILL.md\");\n",
    )
    .unwrap();
    fs::write(
        repo.path()
            .join("templates/skills/mobile-proof-qa/SKILL.md"),
        "# Mobile Proof QA\nRun `aaf proof record` before gate.\n",
    )
    .unwrap();

    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-m", "init"]);
    git(repo.path(), &["checkout", "-b", "feat/edit-embedded-skill"]);

    // Edit the embedded markdown file, breaking the invariant checked by test_scoped.
    fs::write(
        repo.path()
            .join("templates/skills/mobile-proof-qa/SKILL.md"),
        "# Mobile Proof QA\nRun proof before gate.\n",
    )
    .unwrap();

    let mut cfg = RepoConfig {
        gate: GateConfig::default(),
        ..RepoConfig::default()
    };
    cfg.gate.base_ref = "main".to_string();
    assert!(
        cfg.gate.docs_fast_path,
        "GateConfig::default() should enable docs_fast_path"
    );
    cfg.commands.lint = "true".to_string();
    cfg.commands.test_scoped =
        "grep -q 'aaf proof record' templates/skills/mobile-proof-qa/SKILL.md".to_string();
    cfg.commands.test_full = cfg.commands.test_scoped.clone();

    // The CPU-load wait is off: the host running the tests may be busy.
    let machine = MachineConfig {
        max_load_percent: 0,
        ..MachineConfig::default()
    };
    let receipt = run_gate_with(
        data.path(),
        repo.path(),
        &cfg,
        GateMode::Scoped,
        &FixedClock(100),
        &machine,
    )
    .unwrap();

    assert_eq!(
        receipt.status,
        GateStatus::Blocked,
        "embedded markdown must not bypass test_scoped via docs_fast_path; reasons={:?}, commands={:?}",
        receipt.reasons,
        receipt.commands
    );
    assert!(
        !receipt
            .reasons
            .iter()
            .any(|r| r.starts_with(DOCS_FAST_PATH_NOTE)),
        "must not take docs_fast_path for markdown referenced by include_str!; reasons={:?}",
        receipt.reasons
    );
    assert!(
        receipt
            .commands
            .iter()
            .any(|c| c.name == "test_scoped" && !c.passed()),
        "test_scoped must execute and fail; commands={:?}",
        receipt.commands
    );
}
