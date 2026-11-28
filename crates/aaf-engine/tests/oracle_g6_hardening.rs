#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Hardening tests for G6 (Red->Green & Vacuity Oracle):
//! - Add-only new module compile-red stays Blocked under default policy (B1)
//! - Vacuous test riding on an already-red base is not Proven (B2)
//! - Flaky changed test is Unverified in block mode and never hits transient retry (B2/D1)
//! - Genuine red->green bugfix runs 3 oracle executions (initial + rerun + control) (B2)
//! - Skipped oracle runs record non-blocking `note: oracle skipped (...)` reasons (B3)

use std::fs;
use std::path::Path;
use std::process::Command;

use aaf_domain::config::{MachineConfig, OracleMode, RepoConfig};
use aaf_domain::gate::{ChangeKind, GateMode, GateStatus, Receipt};
use aaf_domain::ports::Clock;
use aaf_engine::config::{ConfigInputs, ConfigOverlay, REPO_CONFIG_REL, load};
use aaf_engine::gate::changes::Change;
use aaf_engine::gate::oracle;
use aaf_engine::gate::runner::HeavyContext;
use aaf_engine::gate::{run_gate_throttled, stats_repo_id};
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
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn has_reason(receipt: &Receipt, needle: &str) -> bool {
    receipt.reasons.iter().any(|r| r.contains(needle))
}

fn init_oracle_repo(oracle_mode: &str, runner_preamble: &str) -> TempDir {
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
    let runner = format!(
        "#!/bin/sh\nset -e\n{runner_preamble}\nfor f in tests/test_*.sh; do\n  [ -f \"$f\" ] || continue\n  sh \"$f\"\ndone\n"
    );
    fs::write(root.join("scripts/run_tests.sh"), runner).unwrap();
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
    .expect("config must load cleanly");

    let machine = MachineConfig::default();
    let throttle = calm_throttle(&machine);
    run_gate_throttled(
        data_dir,
        repo_root,
        &effective.repo,
        GateMode::Scoped,
        &FixedClock(3000),
        &throttle,
    )
    .expect("gate run should succeed")
}

#[test]
fn add_only_module_with_import_only_test_is_blocked_under_default_policy() {
    let repo = init_oracle_repo("block", "");
    let data = TempDir::new().unwrap();

    fs::write(repo.path().join("src/new_mod.sh"), "#!/bin/sh\necho 1\n").unwrap();
    fs::write(
        repo.path().join("tests/test_new_mod.sh"),
        "#!/bin/sh\nif [ ! -f src/new_mod.sh ]; then\n  echo \"error[E0425]: cannot find value new_mod\"\n  echo \"error: could not compile demo\"\n  exit 101\nfi\necho \"PASS: new_mod\"\n",
    )
    .unwrap();

    let receipt = run_gate(repo.path(), data.path());
    assert_eq!(
        receipt.status,
        GateStatus::Blocked,
        "add-only module compile_red must be Blocked under default policy, got {:?}",
        receipt.reasons
    );
    assert!(
        has_reason(&receipt, "compile_red"),
        "expected compile_red reason, got {:?}",
        receipt.reasons
    );
}

#[test]
fn vacuous_test_riding_on_red_base_is_not_proven() {
    let repo = init_oracle_repo("block", "");
    let data = TempDir::new().unwrap();
    let root = repo.path();

    // Base suite already fails without any new test because test_old.sh asserts app.sh == 42.
    git(root, &["checkout", "-q", "main"]);
    fs::write(
        root.join("tests/test_old.sh"),
        "#!/bin/sh\nval=$(sh src/app.sh)\nif [ \"$val\" != \"42\" ]; then\n  echo \"FAIL: old test expected 42\"\n  exit 1\nfi\necho \"PASS: old\"\n",
    )
    .unwrap();
    git(root, &["add", "tests/test_old.sh"]);
    git(root, &["commit", "-q", "-m", "red base test"]);
    git(root, &["checkout", "-q", "-B", "feat/work"]);

    // Branch fixes app.sh and adds a vacuous test that always passes.
    fs::write(root.join("src/app.sh"), "#!/bin/sh\necho 42\n").unwrap();
    fs::write(root.join("tests/test_new.sh"), "#!/bin/sh\necho ok\n").unwrap();

    let receipt = run_gate(root, data.path());
    assert_eq!(
        receipt.status,
        GateStatus::Clear,
        "fixing an existing red test stays Clear with behavioral_red_unproven, got {:?}",
        receipt.reasons
    );
    assert!(
        !has_reason(&receipt, "red_green=proven"),
        "vacuous test riding on red base must not be proven, got {:?}",
        receipt.reasons
    );
    assert!(
        has_reason(&receipt, "behavioral_red_unproven"),
        "expected behavioral_red_unproven warning, got {:?}",
        receipt.reasons
    );
}

#[test]
fn flaky_changed_test_is_inconclusive_in_block_mode() {
    let counter_dir = TempDir::new().unwrap();
    let counter_file = fs::canonicalize(counter_dir.path())
        .unwrap()
        .join("flaky_count.txt");
    let repo = init_oracle_repo("block", "");
    let data = TempDir::new().unwrap();

    fs::write(repo.path().join("src/app.sh"), "#!/bin/sh\necho 42\n").unwrap();
    let flaky_test = format!(
        "#!/bin/sh\nval=$(sh src/app.sh)\nif [ \"$val\" = \"42\" ]; then\n  echo \"PASS: live\"\n  exit 0\nfi\nf='{}'\nn=0\nif [ -f \"$f\" ]; then n=$(cat \"$f\"); fi\nn=$((n + 1))\necho \"$n\" > \"$f\"\nif [ \"$n\" -eq 1 ]; then\n  echo \"FAIL: flaky once\"\n  exit 1\nfi\necho \"PASS: flaky recovered\"\nexit 0\n",
        counter_file.display()
    );
    fs::write(repo.path().join("tests/test_flaky.sh"), flaky_test).unwrap();

    let receipt = run_gate(repo.path(), data.path());
    assert_eq!(
        receipt.status,
        GateStatus::Unverified,
        "flaky changed test must be Unverified in block mode, got {:?}",
        receipt.reasons
    );
    assert!(
        has_reason(&receipt, "flaky"),
        "expected flaky reason, got {:?}",
        receipt.reasons
    );
    for reason in &receipt.reasons {
        assert_ne!(
            reason, "oracle: inconclusive (timed out)",
            "flaky reason must not match transient timeout"
        );
        assert_ne!(
            reason, "oracle: inconclusive (killed by signal)",
            "flaky reason must not match transient signal"
        );
    }
}

#[test]
fn genuine_red_green_runs_control_and_stays_proven() {
    let log_dir = TempDir::new().unwrap();
    let cwd_log = fs::canonicalize(log_dir.path()).unwrap().join("cwds.txt");
    let preamble = format!("pwd -P >> '{}'", cwd_log.display());
    let repo = init_oracle_repo("block", &preamble);
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
        "genuine red->green bugfix must stay Clear, got {:?}",
        receipt.reasons
    );
    assert!(
        has_reason(&receipt, "oracle: red_green=proven"),
        "expected oracle: red_green=proven, got {:?}",
        receipt.reasons
    );

    let repo_canon = fs::canonicalize(repo.path()).unwrap();
    let cwds = fs::read_to_string(&cwd_log).unwrap();
    let oracle_runs = cwds
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| Path::new(line) != repo_canon.as_path())
        .count();
    assert_eq!(
        oracle_runs, 3,
        "expected exactly 3 oracle runs (initial + rerun + control) outside repo {repo_canon:?}, got {oracle_runs} in:\n{cwds}"
    );
}

#[test]
fn skip_reasons_are_recorded_without_changing_status() {
    let assert_skip_note = |reasons: &[String], expected: &str| {
        assert!(
            reasons.iter().any(|r| r == expected),
            "expected exact skip note {expected:?}, got {reasons:?}"
        );
        for r in reasons {
            if r.contains("oracle skipped") {
                assert!(
                    !r.starts_with("oracle:") && !r.starts_with("warning: oracle:"),
                    "skip note must not start with oracle:/warning: oracle:: {r:?}"
                );
            }
        }
    };

    // 1. No production changes.
    let no_prod_off = init_oracle_repo("off", "");
    fs::write(
        no_prod_off.path().join("tests/test_only.sh"),
        "#!/bin/sh\necho ok\n",
    )
    .unwrap();
    let r_no_prod_off = run_gate(no_prod_off.path(), TempDir::new().unwrap().path());

    let no_prod_blk = init_oracle_repo("block", "");
    fs::write(
        no_prod_blk.path().join("tests/test_only.sh"),
        "#!/bin/sh\necho ok\n",
    )
    .unwrap();
    let r_no_prod_blk = run_gate(no_prod_blk.path(), TempDir::new().unwrap().path());
    assert_eq!(r_no_prod_blk.status, r_no_prod_off.status);
    assert_eq!(r_no_prod_blk.status, GateStatus::Clear);
    assert_skip_note(
        &r_no_prod_blk.reasons,
        "note: oracle skipped (no production changes)",
    );

    // 2. Docs fast path.
    let docs_off = init_oracle_repo("off", "");
    fs::write(docs_off.path().join("GUIDE.md"), "# Guide\n").unwrap();
    let r_docs_off = run_gate(docs_off.path(), TempDir::new().unwrap().path());

    let docs_blk = init_oracle_repo("block", "");
    fs::write(docs_blk.path().join("GUIDE.md"), "# Guide\n").unwrap();
    let r_docs_blk = run_gate(docs_blk.path(), TempDir::new().unwrap().path());
    assert_eq!(r_docs_blk.status, r_docs_off.status);
    assert_eq!(r_docs_blk.status, GateStatus::Clear);
    assert_skip_note(&r_docs_blk.reasons, "note: oracle skipped (docs fast path)");

    // 3. No base commit (unborn HEAD).
    let make_unborn = |oracle_mode: &str| {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        git(root, &["init", "-q", "-b", "main"]);
        fs::create_dir_all(root.join(".autofactory")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("tests")).unwrap();
        let cfg = format!(
            "[commands]\ntest_scoped = \"sh tests/test_smoke.sh\"\n\n[gate]\nhead_lock_globs = []\nred_green_oracle = \"{oracle_mode}\"\n"
        );
        fs::write(root.join(REPO_CONFIG_REL), cfg).unwrap();
        fs::write(root.join("src/app.sh"), "#!/bin/sh\necho 1\n").unwrap();
        fs::write(root.join("tests/test_smoke.sh"), "#!/bin/sh\nexit 0\n").unwrap();
        dir
    };
    let unborn_off = make_unborn("off");
    let r_unborn_off = run_gate(unborn_off.path(), TempDir::new().unwrap().path());
    let unborn_blk = make_unborn("block");
    let r_unborn_blk = run_gate(unborn_blk.path(), TempDir::new().unwrap().path());
    assert_eq!(r_unborn_blk.status, r_unborn_off.status);
    assert_eq!(r_unborn_blk.status, GateStatus::Clear);
    assert_skip_note(
        &r_unborn_blk.reasons,
        "note: oracle skipped (no base commit)",
    );

    // 4. No test_scoped/test_full command configured (via oracle::evaluate + apply_verdict).
    let no_cmd_repo = init_oracle_repo("block", "");
    let base_sha = git(no_cmd_repo.path(), &["rev-parse", "main"])
        .trim()
        .to_string();
    fs::write(
        no_cmd_repo.path().join("src/app.sh"),
        "#!/bin/sh\necho 42\n",
    )
    .unwrap();
    fs::write(
        no_cmd_repo.path().join("tests/test_fix.sh"),
        "#!/bin/sh\necho ok\n",
    )
    .unwrap();
    let changes = vec![
        Change {
            kind: ChangeKind::Modified,
            path: "src/app.sh".into(),
            old_path: None,
        },
        Change {
            kind: ChangeKind::Added,
            path: "tests/test_fix.sh".into(),
            old_path: None,
        },
    ];
    let data = TempDir::new().unwrap();
    let repo_id = stats_repo_id(no_cmd_repo.path());
    let clock = FixedClock(3001);
    let machine = MachineConfig::default();
    let throttle = calm_throttle(&machine);
    let heavy = HeavyContext {
        data_dir: data.path(),
        stats_repo_id: &repo_id,
        clock: &clock,
        throttle: &throttle,
    };

    let mut cfg_off = RepoConfig::default();
    cfg_off.gate.red_green_oracle = OracleMode::Off;
    let v_off = oracle::evaluate(
        no_cmd_repo.path(),
        &base_sha,
        &cfg_off,
        GateMode::Scoped,
        &changes,
        &[],
        &heavy,
    )
    .unwrap();
    let (status_off, reasons_off) = oracle::apply_verdict(GateStatus::Clear, Vec::new(), v_off);
    assert_eq!(status_off, GateStatus::Clear);
    assert!(reasons_off.is_empty(), "{reasons_off:?}");

    let mut cfg_blk = RepoConfig::default();
    cfg_blk.gate.red_green_oracle = OracleMode::Block;
    let v_blk = oracle::evaluate(
        no_cmd_repo.path(),
        &base_sha,
        &cfg_blk,
        GateMode::Scoped,
        &changes,
        &[],
        &heavy,
    )
    .unwrap();
    let (status_blk, reasons_blk) = oracle::apply_verdict(GateStatus::Clear, Vec::new(), v_blk);
    assert_eq!(status_blk, status_off);
    assert_skip_note(
        &reasons_blk,
        "note: oracle skipped (no test_scoped/test_full command configured)",
    );
}

#[test]
fn warn_mode_skips_flake_rerun_on_anchored_failure_but_keeps_unanchored_rerun() {
    let run_case = |failure_echo: &str| -> usize {
        let log_dir = TempDir::new().unwrap();
        let cwd_log = fs::canonicalize(log_dir.path()).unwrap().join("cwds.txt");
        let preamble = format!("pwd -P >> '{}'", cwd_log.display());
        let repo = init_oracle_repo("warn", &preamble);
        let data = TempDir::new().unwrap();

        fs::write(repo.path().join("src/app.sh"), "#!/bin/sh\necho 42\n").unwrap();
        fs::write(
            repo.path().join("tests/test_fix.sh"),
            format!(
                "#!/bin/sh\nval=$(sh src/app.sh)\nif [ \"$val\" != \"42\" ]; then\n  echo \"{failure_echo}\"\n  exit 1\nfi\necho \"PASS: got 42\"\n"
            ),
        )
        .unwrap();

        let receipt = run_gate(repo.path(), data.path());
        assert_eq!(receipt.status, GateStatus::Clear, "{:?}", receipt.reasons);
        assert!(has_reason(&receipt, "oracle: red_green=proven"));
        let repo_canon = fs::canonicalize(repo.path()).unwrap();
        let log = fs::read_to_string(&cwd_log).unwrap();
        log.lines()
            .filter(|line| Path::new(line) != repo_canon)
            .count()
    };

    assert_eq!(
        run_case("test result: FAILED. 0 passed; 1 failed"),
        2,
        "Warn mode with anchored runner summary must skip redundant flake rerun (initial + control = 2)"
    );
    assert_eq!(
        run_case("FAIL: generic unanchored message"),
        3,
        "Warn mode with unanchored generic failure must keep flake rerun (3)"
    );
}
