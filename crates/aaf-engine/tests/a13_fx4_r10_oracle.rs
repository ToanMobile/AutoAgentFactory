#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Round-10 oracle fixes (fixer 4): inline Rust tests added inside an
//! existing `mod tests` (R10-L2-1), fixture/golden directories on the test
//! side (R10-L2-2), and gitlinks / scratch-preparation errors (R10-L2-3).

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

fn git_out(repo: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn init_repo(oracle_mode: &str, test_cmd: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    git_out(root, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(root.join(".autofactory")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("tests")).unwrap();
    let cmd_toml = toml::Value::String(test_cmd.to_string()).to_string();
    let cfg = format!(
        "[commands]\ntest_scoped = {cmd_toml}\ntest_full = {cmd_toml}\n\n[gate]\nbase_ref = \"main\"\nred_green_oracle = \"{oracle_mode}\"\n"
    );
    fs::write(root.join(REPO_CONFIG_REL), cfg).unwrap();
    fs::write(root.join("src/app.sh"), "#!/bin/sh\necho 0\n").unwrap();
    fs::write(root.join("tests/test_smoke.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    git_out(root, &["add", "-A"]);
    git_out(root, &["commit", "-q", "-m", "base"]);
    git_out(root, &["checkout", "-q", "-b", "feat/work"]);
    dir
}

fn run_gate(repo: &Path, data: &Path) -> Receipt {
    run_gate_mode(repo, data, GateMode::Scoped)
}

fn run_gate_mode(repo: &Path, data: &Path, mode: GateMode) -> Receipt {
    let no_env = |_: &str| None;
    let effective = load(&ConfigInputs {
        global_config: None,
        repo_root: repo.to_path_buf(),
        env: &no_env,
        cli: ConfigOverlay::default(),
    })
    .unwrap();
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
        data,
        repo,
        &effective.repo,
        mode,
        &FixedClock(2000),
        &throttle,
    )
    .unwrap_or_else(|err| panic!("run_gate returned Err (no receipt): {err}"))
}

fn has(r: &Receipt, needle: &str) -> bool {
    r.reasons.iter().any(|m| m.contains(needle))
}

const LIB_BASE: &str = "pub fn sub(a: i32, b: i32) -> i32 {\n    a - b\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn zero() {}\n}\n";
const LIB_HEAD: &str = "pub fn sub(a: i32, b: i32) -> i32 {\n    a + b\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn zero() {}\n\n    #[test]\n    fn two() {}\n}\n";

#[test]
fn new_test_fn_inside_existing_mod_tests_counts_and_survives_revert() {
    let cmd = "if grep -q 'a - b' src/lib.rs && grep -q 'fn two' src/lib.rs; then echo 'FAIL: two'; exit 1; fi; echo ok";
    let repo = init_repo("block", cmd);
    let data = TempDir::new().unwrap();
    fs::write(repo.path().join("src/lib.rs"), LIB_BASE).unwrap();
    git_out(repo.path(), &["add", "-A"]);
    git_out(repo.path(), &["commit", "-q", "-m", "lib base"]);
    // The branch must be based on the commit holding LIB_BASE.
    git_out(repo.path(), &["branch", "-f", "main", "HEAD"]);
    fs::write(repo.path().join("src/lib.rs"), LIB_HEAD).unwrap();

    let r = run_gate(repo.path(), data.path());
    assert_eq!(r.status, GateStatus::Clear, "{:?}", r.reasons);
    assert!(has(&r, "oracle: red_green=proven"), "{:?}", r.reasons);
    assert!(!has(&r, "no test changes"), "{:?}", r.reasons);
}

#[test]
fn prod_only_edit_above_existing_mod_tests_is_still_no_test_change() {
    let repo = init_repo("block", "echo ok");
    let data = TempDir::new().unwrap();
    fs::write(repo.path().join("src/lib.rs"), LIB_BASE).unwrap();
    git_out(repo.path(), &["add", "-A"]);
    git_out(repo.path(), &["commit", "-q", "-m", "lib base"]);
    git_out(repo.path(), &["branch", "-f", "main", "HEAD"]);
    fs::write(
        repo.path().join("src/lib.rs"),
        LIB_BASE
            .replace("a - b", "a + b")
            .replace("pub fn sub", "// #[cfg(test)]\npub fn sub"),
    )
    .unwrap();
    let r = run_gate(repo.path(), data.path());
    assert_eq!(r.status, GateStatus::Blocked, "{:?}", r.reasons);
    assert!(has(&r, "no test changes"), "{:?}", r.reasons);
}

#[test]
fn updated_golden_fixture_is_test_side_and_never_reverted() {
    let cmd = "[ \"$(sh src/app.sh)\" = \"$(cat testdata/x.golden)\" ] || { echo 'FAIL: golden mismatch'; exit 1; }; echo ok";
    let repo = init_repo("block", cmd);
    let data = TempDir::new().unwrap();
    fs::create_dir_all(repo.path().join("testdata")).unwrap();
    fs::write(repo.path().join("testdata/x.golden"), "0\n").unwrap();
    git_out(repo.path(), &["add", "-A"]);
    git_out(repo.path(), &["commit", "-q", "-m", "golden base"]);
    git_out(repo.path(), &["branch", "-f", "main", "HEAD"]);
    fs::write(repo.path().join("src/app.sh"), "#!/bin/sh\necho 42\n").unwrap();
    fs::write(repo.path().join("testdata/x.golden"), "42\n").unwrap();
    // A real test file too, so the only question is whether the golden is
    // reverted along with the production code (which made it vacuous).
    fs::write(repo.path().join("tests/test_golden.sh"), "echo ok\n").unwrap();

    let r = run_gate(repo.path(), data.path());
    assert_eq!(r.status, GateStatus::Clear, "{:?}", r.reasons);
    assert!(has(&r, "oracle: red_green=proven"), "{:?}", r.reasons);
}

#[test]
fn newly_added_snapshot_fixture_is_kept_in_the_reverted_tree() {
    // The test reads a fixture that only exists on the branch: reverting it
    // as production code would make the test fail for the wrong reason.
    let cmd = "for f in tests/test_*.sh; do [ -f \"$f\" ] || continue; sh \"$f\" || exit $?; done";
    let repo = init_repo("block", cmd);
    let data = TempDir::new().unwrap();
    fs::write(repo.path().join("src/app.sh"), "#!/bin/sh\necho 42\n").unwrap();
    fs::create_dir_all(repo.path().join("src/__snapshots__")).unwrap();
    fs::write(repo.path().join("src/__snapshots__/app.snap"), "42\n").unwrap();
    fs::write(
        repo.path().join("tests/test_snap.sh"),
        "#!/bin/sh\n[ -f src/__snapshots__/app.snap ] || { echo 'missing fixture'; exit 3; }\n[ \"$(sh src/app.sh)\" = \"$(cat src/__snapshots__/app.snap)\" ] || { echo 'FAIL: snapshot'; exit 1; }\necho ok\n",
    )
    .unwrap();
    let r = run_gate(repo.path(), data.path());
    assert_eq!(r.status, GateStatus::Clear, "{:?}", r.reasons);
    assert!(has(&r, "oracle: red_green=proven"), "{:?}", r.reasons);
}

#[test]
fn gitlink_change_never_errors_the_gate() {
    let cmd = "for f in tests/test_*.sh; do [ -f \"$f\" ] || continue; sh \"$f\" || exit 1; done";
    let repo = init_repo("block", cmd);
    let data = TempDir::new().unwrap();
    let root = repo.path();
    let sha1 = "1111111111111111111111111111111111111111";
    let sha2 = "2222222222222222222222222222222222222222";
    git_out(
        root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{sha1},sub"),
        ],
    );
    git_out(root, &["commit", "-q", "-m", "submodule base"]);
    git_out(root, &["branch", "-f", "main", "HEAD"]);
    git_out(
        root,
        &["update-index", "--cacheinfo", &format!("160000,{sha2},sub")],
    );
    fs::write(root.join("src/app.sh"), "#!/bin/sh\necho 42\n").unwrap();
    fs::write(
        root.join("tests/test_answer.sh"),
        "#!/bin/sh\n[ \"$(sh src/app.sh)\" = 42 ] || { echo 'FAIL: expected 42'; exit 1; }\necho ok\n",
    )
    .unwrap();
    git_out(root, &["add", "src/app.sh", "tests/test_answer.sh"]);
    git_out(root, &["commit", "-q", "-m", "bump submodule"]);

    // The gitlink is skipped, so the reverted tree still proves red->green.
    let r = run_gate(root, data.path());
    assert_eq!(r.status, GateStatus::Clear, "{:?}", r.reasons);
    assert!(has(&r, "oracle: red_green=proven"), "{:?}", r.reasons);
}

#[test]
fn scratch_preparation_failure_is_inconclusive_not_a_gate_error() {
    let cmd = "[ \"$(sh src/app.sh)\" = 42 ] || { echo 'FAIL: expected 42'; exit 1; }; echo ok";
    for (mode, status, prefix) in [
        (
            "block",
            GateStatus::Unverified,
            "oracle: inconclusive (could not prepare",
        ),
        (
            "warn",
            GateStatus::Clear,
            "warning: oracle: inconclusive (could not prepare",
        ),
    ] {
        let repo = init_repo(mode, cmd);
        let data = TempDir::new().unwrap();
        let root = repo.path();
        fs::write(root.join("src/app.sh"), "#!/bin/sh\necho 42\n").unwrap();
        fs::write(root.join("tests/test_answer.sh"), "echo ok\n").unwrap();
        // `git worktree add` cannot register the scratch tree.
        let admin = Path::new(".git").join("worktrees");
        fs::write(root.join(admin), "not a directory\n").unwrap();
        let r = run_gate(root, data.path());
        assert_eq!(r.status, status, "{mode}: {:?}", r.reasons);
        assert!(has(&r, prefix), "{mode}: {:?}", r.reasons);
    }
}

/// Fixture directories hold data, not code: a golden/snapshot data file is
/// test-side, but source code in a module named like a fixture dir stays
/// production, so the oracle still checks it (fail closed).
#[test]
fn fixture_dirs_cover_data_files_only() {
    use aaf_engine::gate::oracle::is_oracle_test_path;
    for data in [
        "testdata/render.golden",
        "pkg/fixtures/user.json",
        "__snapshots__/app.test.js.snap",
        "golden/out.txt",
    ] {
        assert!(is_oracle_test_path(data), "{data} should be test-side");
    }
    for code in [
        "src/snapshots/mod.rs",
        "pkg/golden/render.go",
        "app/fixtures/loader.py",
        "web/snapshots/index.ts",
    ] {
        assert!(!is_oracle_test_path(code), "{code} must stay production");
    }
}
