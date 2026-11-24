#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Edge-case integration tests for G6 (Opt-in Red->Green & Vacuity Oracle).

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use aaf_domain::config::{CommandsConfig, MachineConfig, OracleMode, RepoConfig};
use aaf_domain::gate::{GateMode, GateStatus, Receipt};
use aaf_domain::oracle_config::OracleCompileRedPolicy;
use aaf_domain::ports::Clock;
use aaf_engine::config::{
    ConfigError, ConfigInputs, ConfigOverlay, Detected, LOCAL_CONFIG_REL, REPO_CONFIG_REL, load,
    render_repo_config,
};
use aaf_engine::gate::oracle_classify::{OracleOutcome, classify};
use aaf_engine::gate::{config_hash, run_gate_throttled};
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

fn has_reason(receipt: &Receipt, needle: &str) -> bool {
    receipt.reasons.iter().any(|r| r.contains(needle))
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
    init_repo_with_gate(oracle_mode, "", test_cmd)
}

fn init_repo_with_gate(oracle_mode: &str, extra_gate: &str, test_cmd: &str) -> TempDir {
    init_repo_with_cmds(oracle_mode, extra_gate, test_cmd, test_cmd)
}

fn init_repo_with_cmds(
    oracle_mode: &str,
    extra_gate: &str,
    scoped_cmd: &str,
    full_cmd: &str,
) -> TempDir {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    git_out(root, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(root.join(".autofactory")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("tests")).unwrap();
    let scoped_toml = toml::Value::String(scoped_cmd.to_string()).to_string();
    let full_toml = toml::Value::String(full_cmd.to_string()).to_string();
    let cfg = format!(
        "[commands]\ntest_scoped = {scoped_toml}\ntest_full = {full_toml}\n\n[gate]\nbase_ref = \"main\"\nred_green_oracle = \"{oracle_mode}\"\n{extra_gate}"
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
    let throttle = calm_throttle(&machine);
    run_gate_throttled(
        data,
        repo,
        &effective.repo,
        mode,
        &FixedClock(2000),
        &throttle,
    )
    .unwrap()
}

#[test]
fn off_mode_skips_oracle_and_preserves_config_hash_and_invalidates_when_enabled() {
    let default_cfg = RepoConfig::default();
    let json = serde_json::to_string(&default_cfg.gate).unwrap();
    assert!(
        !json.contains("red_green_oracle"),
        "off mode must be omitted from serialized GateConfig to preserve config_hash: {json}"
    );

    let mut warn_cfg = RepoConfig::default();
    warn_cfg.gate.red_green_oracle = OracleMode::Warn;
    let mut block_cfg = RepoConfig::default();
    block_cfg.gate.red_green_oracle = OracleMode::Block;
    assert_ne!(config_hash(&default_cfg), config_hash(&warn_cfg));
    assert_ne!(config_hash(&warn_cfg), config_hash(&block_cfg));

    let repo = init_repo("off", "sh tests/test_smoke.sh");
    let data = TempDir::new().unwrap();
    // Prod change with NO test change still passes cleanly when oracle is off.
    fs::write(repo.path().join("src/app.sh"), "#!/bin/sh\necho 42\n").unwrap();
    let receipt = run_gate(repo.path(), data.path());
    assert_eq!(receipt.status, GateStatus::Clear, "{:?}", receipt.reasons);
    assert!(receipt.reasons.is_empty(), "{:?}", receipt.reasons);
}

#[test]
fn warn_mode_reports_warning_for_vacuous_and_no_tests_without_blocking() {
    let repo = init_repo("warn", "sh tests/test_smoke.sh");
    let data = TempDir::new().unwrap();
    fs::write(repo.path().join("src/app.sh"), "#!/bin/sh\necho 42\n").unwrap();

    let no_tests = run_gate(repo.path(), data.path());
    assert_eq!(no_tests.status, GateStatus::Clear, "{:?}", no_tests.reasons);
    assert!(
        has_reason(&no_tests, "warning: oracle: no test"),
        "{:?}",
        no_tests.reasons
    );

    fs::write(repo.path().join("tests/test_vacuous.sh"), "echo PASS\n").unwrap();
    let vacuous = run_gate(repo.path(), data.path());
    assert_eq!(vacuous.status, GateStatus::Clear, "{:?}", vacuous.reasons);
    assert!(
        has_reason(&vacuous, "warning: oracle: vacuous"),
        "{:?}",
        vacuous.reasons
    );
}

#[test]
fn block_mode_marks_compile_red_and_zero_tests_as_unverified() {
    let run_case = |extra_gate: &str, cmd: &str, prod: (&str, &str), test: (&str, &str)| {
        let repo = init_repo_with_gate("block", extra_gate, cmd);
        let data = TempDir::new().unwrap();
        fs::write(repo.path().join(prod.0), prod.1).unwrap();
        fs::write(repo.path().join(test.0), test.1).unwrap();
        run_gate(repo.path(), data.path())
    };
    let compile_cmd = "if [ \"$(sh src/app.sh)\" = \"42\" ]; then exit 0; else echo \"error[E0425]: cannot find value new_api\"; echo \"error: could not compile demo\"; exit 101; fi";
    // 1a. Default OracleCompileRedPolicy::Blocked -> retryable GateStatus::Blocked.
    // CompileRed goes back to the agent (Blocked, with guidance) instead of
    // waiting for a human (O12-R10-7); it never lands as Clear.
    let r = run_case(
        "",
        compile_cmd,
        ("src/app.sh", "#!/bin/sh\necho 42\n"),
        ("tests/test_api.sh", "echo test\n"),
    );
    assert_eq!(r.status, GateStatus::Blocked, "{:?}", r.reasons);
    assert!(has_reason(&r, "oracle: compile_red ("), "{:?}", r.reasons);

    // 1b. Explicit OracleCompileRedPolicy::Unverified -> GateStatus::Unverified.
    let r_unv = run_case(
        "oracle_compile_red_policy = \"unverified\"\n",
        compile_cmd,
        ("src/app.sh", "#!/bin/sh\necho 42\n"),
        ("tests/test_api.sh", "echo test\n"),
    );
    assert_eq!(r_unv.status, GateStatus::Unverified, "{:?}", r_unv.reasons);
    assert!(
        has_reason(&r_unv, "oracle: inconclusive (compile_red:"),
        "{:?}",
        r_unv.reasons
    );

    // 1c. All-Added new prod module under default Blocked policy -> Warn + behavioral_red_unproven.
    let new_mod_cmd = "if [ -f src/new_mod.sh ]; then exit 0; else echo \"error[E0425]: cannot find value new_mod\"; echo \"error: could not compile demo\"; exit 101; fi";
    let r_new = run_case(
        "",
        new_mod_cmd,
        ("src/new_mod.sh", "#!/bin/sh\necho 1\n"),
        ("tests/test_new_mod.sh", "echo test\n"),
    );
    assert_eq!(r_new.status, GateStatus::Clear, "{:?}", r_new.reasons);
    assert!(
        has_reason(&r_new, "behavioral_red_unproven"),
        "{:?}",
        r_new.reasons
    );

    // 2. Zero tests ran during oracle run -> Unverified (NOT Clear!).
    let zero_cmd = "if [ \"$(sh src/app.sh)\" = \"42\" ]; then echo \"running 1 test\"; exit 0; else echo \"running 0 tests\"; exit 0; fi";
    let r2 = run_case(
        "",
        zero_cmd,
        ("src/app.sh", "#!/bin/sh\necho 42\n"),
        ("tests/test_zero.sh", "echo test\n"),
    );
    assert_eq!(r2.status, GateStatus::Unverified, "{:?}", r2.reasons);
    assert!(has_reason(&r2, "0 tests ran"), "{:?}", r2.reasons);
}

#[test]
fn oracle_populates_test_files_env_preserves_live_tree_and_cleans_scratch_worktree() {
    let out_dir = TempDir::new().unwrap();
    let capture = out_dir.path().join("oracle_test_files.txt");
    let leak_flag = out_dir.path().join("oracle_flag.txt");
    let test_cmd = format!(
        "if [ -n \"$AAF_TEST_FILES_FILE\" ]; then cp \"$AAF_TEST_FILES_FILE\" \"{}\" ; echo \"FLAG=$AAF_ORACLE_RUN\" > \"{}\" ; fi; val=$(sh src/app.sh); if [ \"$val\" != \"42\" ]; then echo \"FAIL: expected 42\"; exit 1; fi",
        capture.display(),
        leak_flag.display()
    );
    let repo = init_repo("block", &test_cmd);
    let data = TempDir::new().unwrap();

    // Uncommitted prod edit + untracked test file + extra untracked prod file.
    fs::write(repo.path().join("src/app.sh"), "#!/bin/sh\necho 42\n").unwrap();
    fs::write(repo.path().join("src/extra_new.sh"), "#!/bin/sh\necho hi\n").unwrap();
    fs::write(repo.path().join("tests/test_feature.sh"), "echo ok\n").unwrap();

    let fp_before = aaf_fp::tree_fp(repo.path(), Duration::from_secs(30)).unwrap();
    let receipt = run_gate(repo.path(), data.path());
    let fp_after = aaf_fp::tree_fp(repo.path(), Duration::from_secs(30)).unwrap();

    assert_eq!(receipt.status, GateStatus::Clear, "{:?}", receipt.reasons);
    assert_eq!(fp_before, fp_after, "live worktree tree_fp must not change");
    assert_eq!(
        fs::read_to_string(repo.path().join("src/app.sh")).unwrap(),
        "#!/bin/sh\necho 42\n"
    );
    assert!(repo.path().join("src/extra_new.sh").exists());
    assert!(repo.path().join("tests/test_feature.sh").exists());

    // AAF_TEST_FILES_FILE must have contained the changed test file, and AAF_ORACLE_RUN must not be set.
    let captured = fs::read_to_string(&capture).unwrap();
    assert!(
        captured.lines().any(|l| l == "tests/test_feature.sh"),
        "expected tests/test_feature.sh in AAF_TEST_FILES_FILE, got:\n{captured}"
    );
    assert_eq!(fs::read_to_string(&leak_flag).unwrap().trim(), "FLAG=");

    // No ephemeral scratch worktree may leak; only the main worktree and the
    // reusable, clean persistent oracle warm slot (`slot-0`) may be registered.
    let wt_list = git_out(repo.path(), &["worktree", "list", "--porcelain"]);
    let wt_paths: Vec<&str> = wt_list
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .collect();
    assert!(
        wt_paths.len() <= 2,
        "unexpected scratch worktrees leaked:\n{wt_list}"
    );
    for extra in wt_paths.iter().skip(1) {
        assert!(
            extra.contains("warm-slots") && extra.ends_with("slot-0"),
            "unexpected non-warm-slot scratch worktree leaked: {extra}\n{wt_list}"
        );
        let status = git_out(Path::new(extra), &["status", "--porcelain"]);
        assert!(
            status.trim().is_empty(),
            "persistent oracle warm slot was not wiped clean on drop: {status}"
        );
    }
}

#[test]
fn config_tier_monotonic_tightening_and_render_round_trip() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join(".autofactory")).unwrap();
    let load_with = |env: &dyn Fn(&str) -> Option<String>| {
        load(&ConfigInputs {
            global_config: None,
            repo_root: dir.path().to_path_buf(),
            env,
            cli: ConfigOverlay::default(),
        })
    };

    let write_cfg = |rel: &str, body: &str| fs::write(dir.path().join(rel), body).unwrap();

    // Tier 3 cannot downgrade block -> off, nor can env downgrade block -> warn.
    write_cfg(REPO_CONFIG_REL, "[gate]\nred_green_oracle = \"block\"\n");
    write_cfg(LOCAL_CONFIG_REL, "[gate]\nred_green_oracle = \"off\"\n");
    let loaded = load_with(&|k| (k == "AAF_RED_GREEN_ORACLE").then(|| "warn".to_string())).unwrap();
    assert_eq!(loaded.repo.gate.red_green_oracle, OracleMode::Block);
    assert_eq!(loaded.warnings.len(), 2, "{:?}", loaded.warnings);
    assert!(
        loaded
            .warnings
            .iter()
            .all(|w| w.contains("gate.red_green_oracle"))
    );

    // Tier 3 cannot downgrade oracle_compile_red_policy (blocked -> warn), only tighten.
    write_cfg(
        REPO_CONFIG_REL,
        "[gate]\nred_green_oracle = \"off\"\noracle_compile_red_policy = \"blocked\"\n",
    );
    write_cfg(
        LOCAL_CONFIG_REL,
        "[gate]\nred_green_oracle = \"warn\"\noracle_compile_red_policy = \"warn\"\n",
    );
    let env_block = |k: &str| (k == "AAF_RED_GREEN_ORACLE").then(|| "block".to_string());
    let raised = load_with(&env_block).unwrap();
    assert_eq!(raised.repo.gate.red_green_oracle, OracleMode::Block);
    assert_eq!(
        raised.repo.gate.oracle_compile_red_policy,
        OracleCompileRedPolicy::Blocked
    );
    assert_eq!(raised.warnings.len(), 1, "{:?}", raised.warnings);
    assert!(raised.warnings[0].contains("gate.oracle_compile_red_policy"));

    write_cfg(
        LOCAL_CONFIG_REL,
        "[gate]\noracle_compile_red_policy = \"unverified\"\n",
    );
    let tightened = load_with(&env_block).unwrap();
    assert_eq!(
        tightened.repo.gate.oracle_compile_red_policy,
        OracleCompileRedPolicy::Unverified
    );
    assert!(tightened.warnings.is_empty(), "{:?}", tightened.warnings);

    // Invalid env value is rejected.
    let err =
        load_with(&|k| (k == "AAF_RED_GREEN_ORACLE").then(|| "strict".to_string())).unwrap_err();
    assert!(matches!(err, ConfigError::InvalidEnv { .. }), "{err:?}");

    // render_repo_config includes red_green_oracle = "off" and parses back cleanly.
    let detected = Detected {
        profile: "auto".to_string(),
        commands: CommandsConfig::default(),
        evidence: Vec::new(),
    };
    let rendered = render_repo_config(&detected, "demo");
    assert!(rendered.contains("red_green_oracle = \"off\""));
    let parsed: RepoConfig = toml::from_str(&rendered).unwrap();
    assert_eq!(parsed.gate.red_green_oracle, OracleMode::Off);
}

#[test]
fn classifier_covers_multi_language_outcomes() {
    assert_eq!(
        classify(None, true, "FAIL: ignored on timeout"),
        OracleOutcome::Inconclusive("timed out".to_string())
    );
    assert_eq!(
        classify(Some(5), false, "collected 0 items\nno tests ran"),
        OracleOutcome::Inconclusive("0 tests ran".to_string())
    );
    assert_eq!(
        classify(Some(0), false, "running 0 tests\nrunning 3 tests\n3 passed"),
        OracleOutcome::Vacuous
    );
    for behavioral in [
        "test mod::case ... FAILED\npanicked at src/lib.rs:9",
        "FAILED tests/test_a.py::test_x - AssertionError",
        "FAIL src/a.test.ts\n  expect(received).toBe(expected)",
        "--- FAIL: TestSum (0.00s)",
        "MyTest > testFoo() FAILED\norg.opentest4j.AssertionFailedError",
        "Assert.Equal() Failure",
        "Test Case '-[DemoTests testSum]' failed (0.01s)",
        "FAIL: expected 42",
    ] {
        assert_eq!(classify(Some(1), false, behavioral), OracleOutcome::Proven);
    }
    for compile in [
        "error[E0425]: cannot find value `x`\nerror: could not compile `app`",
        "FAIL src/a.test.ts\nerror TS2305: Module has no exported member",
        "FAIL src/a.test.js\nCannot find module './missing'",
        "ImportError: cannot import name 'foo'",
        "FAIL\tpkg/demo [build failed]\nundefined: NewHelper",
        "> Task :compileTestKotlin FAILED\nUnresolved reference: newApi",
        "Program.cs(5,10): error CS0103: The name 'Foo' does not exist",
    ] {
        assert!(
            matches!(
                classify(Some(1), false, compile),
                OracleOutcome::CompileRed(_)
            ),
            "expected CompileRed for:\n{compile}"
        );
    }
    // Zero-tests must win over behavioral substrings; bare expect(/Assert. must not match.
    assert_eq!(
        classify(Some(1), false, "running 0 tests\nexpect(foo).toBe(1)"),
        OracleOutcome::Inconclusive("0 tests ran".to_string())
    );
    let echo_only = "12 | let _ = new_sym(); // running 1 test; assertion failed: x\n   | ^^^^^^^";
    for (code, log) in [
        (127, "sh: 1: missing: not found"),
        (1, "BUILD FAILED\nECONNREFUSED"),
        (1, "log: expect(config) loaded"),
        (1, "Assert.IsNotNull helper registered"),
        (1, echo_only),
    ] {
        assert!(matches!(
            classify(Some(code), false, log),
            OracleOutcome::Inconclusive(_)
        ));
    }
    // Comment injection inside rustc diagnostic echo must stay CompileRed, never Proven.
    let injected = "error[E0425]: cannot find value `new_sym` in this scope\n  --> src/lib.rs:12:13\n   |\n12 |     let _ = new_sym(); // running 1 test; assertion failed: x\n   |             ^^^^^^^ not found\nerror: could not compile `demo`";
    assert!(matches!(
        classify(Some(101), false, injected),
        OracleOutcome::CompileRed(_)
    ));
    // Ambiguous runtime exception names with an anchored runner summary prove behavioral failure.
    for anchored in [
        "FAILED tests/t.py::t - ImportError: bad\n=== 1 failed in 0.02s ===",
        "FAIL a.test.js\nReferenceError: x is not defined\nTests: 1 failed, 1 total",
    ] {
        assert_eq!(classify(Some(1), false, anchored), OracleOutcome::Proven);
    }
}

#[test]
fn rename_prod_to_docs_and_comment_cfg_test_do_not_bypass_g6a() {
    let repo = init_repo("block", "true");
    let data = TempDir::new().unwrap();
    fs::create_dir_all(repo.path().join("docs")).unwrap();
    git_out(repo.path(), &["mv", "src/app.sh", "docs/app.md"]);
    let r = run_gate(repo.path(), data.path());
    assert_eq!(r.status, GateStatus::Blocked, "{:?}", r.reasons);
    assert!(has_reason(&r, "no test"), "{:?}", r.reasons);

    let repo2 = init_repo("block", "true");
    let data2 = TempDir::new().unwrap();
    fs::write(
        repo2.path().join("src/mod.rs"),
        "// #[cfg(test)]\npub fn f() -> i32 { 1 }\n",
    )
    .unwrap();
    let r2 = run_gate(repo2.path(), data2.path());
    assert_eq!(r2.status, GateStatus::Blocked, "{:?}", r2.reasons);
    assert!(has_reason(&r2, "no test"), "{:?}", r2.reasons);
}

#[test]
fn inline_rust_tests_existing_block_raw_strings_and_trailing_prod() {
    let assert_proven = |cmd: &str, msg: &str, base_src: &str, new_src: &str| {
        let repo = init_repo("block", cmd);
        let data = TempDir::new().unwrap();
        git_out(repo.path(), &["checkout", "-q", "main"]);
        fs::write(repo.path().join("src/lib.rs"), base_src).unwrap();
        git_out(repo.path(), &["add", "-A"]);
        git_out(repo.path(), &["commit", "-q", "-m", msg]);
        git_out(repo.path(), &["checkout", "-q", "-B", "feat/work"]);
        fs::write(repo.path().join("src/lib.rs"), new_src).unwrap();
        let r = run_gate(repo.path(), data.path());
        assert_eq!(r.status, GateStatus::Clear, "{:?}", r.reasons);
        assert!(
            has_reason(&r, "oracle: red_green=proven"),
            "{:?}",
            r.reasons
        );
    };

    // 1. Adding #[test] inside an existing #[cfg(test)] mod tests is detected & proven Red->Green.
    let cmd1 = "grep -q 'fn new_case' src/lib.rs || exit 0; grep -q 'fn val() -> i32 { 42 }' src/lib.rs && exit 0; echo 'test tests::new_case ... FAILED'; echo 'test result: FAILED. 0 passed; 1 failed;'; exit 1";
    assert_proven(
        cmd1,
        "base lib",
        "pub fn val() -> i32 { 0 }\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn old() {}\n}\n",
        "pub fn val() -> i32 { 42 }\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn old() {}\n    #[test]\n    fn new_case() { assert_eq!(super::val(), 42); }\n}\n",
    );

    // 2. Raw string r#"#[cfg(test)]"# does not corrupt split, and prod AFTER #[cfg(test)] reverts.
    let cmd2 = "grep -Fq 'r#\"#[cfg(test)]\"#' src/lib.rs || { echo 'raw string broken'; exit 2; }; grep -q 'fn checks_after' src/lib.rs || exit 0; grep -q 'fn after() -> i32 { 42 }' src/lib.rs && exit 0; echo 'test tests::checks_after ... FAILED'; echo 'test result: FAILED. 0 passed; 1 failed;'; exit 1";
    assert_proven(
        cmd2,
        "base raw+after",
        "pub fn tmpl() -> &'static str { r#\"#[cfg(test)]\"# }\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn old() {}\n}\npub fn after() -> i32 { 0 }\n",
        "pub fn tmpl() -> &'static str { r#\"#[cfg(test)]\"# }\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn old() {}\n    #[test]\n    fn checks_after() { assert_eq!(super::after(), 42); }\n}\npub fn after() -> i32 { 42 }\n",
    );
}

#[test]
fn full_mode_runs_test_full_and_symlink_revert_never_escapes_scratch() {
    // 1. GateMode::Full selects test_full (not test_scoped) during oracle run.
    let marker_dir = TempDir::new().unwrap();
    let full_marker = marker_dir.path().join("ran_full.txt");
    let scoped_marker = marker_dir.path().join("ran_scoped.txt");
    let scoped_cmd = format!("touch \"{}\" && exit 0", scoped_marker.display());
    let full_cmd = format!(
        "if [ -n \"$AAF_TEST_FILES_FILE\" ]; then touch \"{}\" ; fi; val=$(sh src/app.sh); if [ \"$val\" != \"42\" ]; then echo \"FAIL: expected 42\"; exit 1; fi",
        full_marker.display()
    );
    let dir = init_repo_with_cmds("block", "", &scoped_cmd, &full_cmd);
    let root = dir.path();
    fs::write(root.join("src/app.sh"), "#!/bin/sh\necho 42\n").unwrap();
    fs::write(root.join("tests/test_full_case.sh"), "echo ok\n").unwrap();

    let data = TempDir::new().unwrap();
    let receipt = run_gate_mode(root, data.path(), GateMode::Full);
    assert_eq!(receipt.status, GateStatus::Clear, "{:?}", receipt.reasons);
    assert!(
        full_marker.exists(),
        "GateMode::Full oracle must run test_full"
    );
    assert!(
        !scoped_marker.exists(),
        "GateMode::Full oracle must not run test_scoped"
    );

    // 2. Committed symlink in HEAD pointing outside repo must never be overwritten on revert.
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let outside = TempDir::new().unwrap();
        let victim = outside.path().join("victim.txt");
        fs::write(&victim, "UNTOUCHED").unwrap();

        let sym_repo = init_repo(
            "block",
            "val=$(cat src/app.sh); [ \"$val\" = \"UNTOUCHED\" ] || { echo \"FAIL: reverted\"; exit 1; }",
        );
        let sym_data = TempDir::new().unwrap();
        fs::remove_file(sym_repo.path().join("src/app.sh")).unwrap();
        symlink(&victim, sym_repo.path().join("src/app.sh")).unwrap();
        fs::write(sym_repo.path().join("tests/test_sym.sh"), "echo ok\n").unwrap();
        git_out(sym_repo.path(), &["add", "-A"]);
        git_out(sym_repo.path(), &["commit", "-q", "-m", "symlink prod"]);

        let sym_receipt = run_gate(sym_repo.path(), sym_data.path());
        assert_eq!(
            sym_receipt.status,
            GateStatus::Clear,
            "{:?}",
            sym_receipt.reasons
        );
        assert_eq!(
            fs::read_to_string(&victim).unwrap(),
            "UNTOUCHED",
            "revert_prod_files must never follow symlink outside scratch"
        );
    }
}
