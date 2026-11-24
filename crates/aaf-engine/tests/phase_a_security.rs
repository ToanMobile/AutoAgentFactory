#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for Phase A security foundations:
//! - A1: Tier-1 + Tier-3 + env tighten-only enforcement on trusted base config.
//! - A2: Common-receipt poisoning guard for worktrees with gitignored files.
//! - A3: Compiler-cache sandbox confinement (sccache/warm-slots) & env stripping.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use aaf_domain::config::{
    AuditScope, GlobalConfig, MachineConfig, OracleMode, RepoConfig, StoreMode,
};
use aaf_domain::gate::{GateMode, GateStatus};
use aaf_domain::ports::Clock;
use aaf_engine::config::{
    ConfigError, LOCAL_CONFIG_REL, tighten_trusted_base, tighten_trusted_base_from_env,
};
use aaf_engine::executors::{BUILD_CACHE_DIRS, DEFAULT_WARM_SLOTS_REL};
use aaf_engine::gate::runner::{COMPILER_WRAPPER_DISABLE_ENV, COMPILER_WRAPPER_ENV_VARS, is_heavy};
use aaf_engine::gate::{
    GateRunOptions, HiddenPolicy, run_gate_reporting, run_gate_with_options, stats_repo_id,
};
use aaf_engine::throttle::{Progress, SystemProbe, Throttle};
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

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "dev@example.com"]);
    git(dir, &["config", "user.name", "Dev"]);
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join(".gitignore"), "target/\n").unwrap();
    fs::write(dir.join("src/lib.rs"), "pub fn v() -> i32 { 1 }\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

fn env_map(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    move |key: &str| map.get(key).cloned()
}

#[test]
fn tighten_trusted_base_enforces_tier1_tier3_and_env_tighten_only() {
    let local_dir = TempDir::new().unwrap();
    fs::create_dir_all(local_dir.path().join(".autofactory")).unwrap();

    // Trusted base Tier-2 config with proof required and strict baseline.
    let mut base = RepoConfig::default();
    base.project.profile = "rust".to_string();
    base.commands.test_scoped = "cargo test".to_string();
    base.proof.required = true;
    base.gate.head_lock_globs = vec!["tests/**".to_string()];
    base.gate.store_compliance = StoreMode::Auto;
    base.gate.min_android_target_sdk = 34;
    base.gate.red_green_oracle = OracleMode::Off;
    base.gate.docs_fast_path = true;
    base.run.max_open_review = 6;
    base.cross_audit.require_different_provider = false;
    base.cross_audit.audit_scope = AuditScope::HeadLock;
    base.cross_audit.score_gate = false;
    base.cross_audit.min_average_score = 90;
    base.cross_audit.min_criterion_score = 85;

    // Tier-1 global floors.
    let mut global = GlobalConfig::default();
    global.cross_audit.require_different_provider = Some(true);
    global.cross_audit.audit_scope = Some(AuditScope::Large);
    global.cross_audit.score_gate = Some(true);
    global.cross_audit.min_average_score = Some(95);
    global.cross_audit.min_criterion_score = Some(92);
    global.run.max_open_review = Some(4);

    // Tier-3 local file attempts BOTH valid tightenings AND forbidden loosenings.
    let local_toml = r#"
[commands]
test_scoped = "true"

[proof]
required = false

[gate]
head_lock_globs = ["security/**"]
store_compliance = "off"
min_android_target_sdk = 28
docs_fast_path = false
red_green_oracle = "warn"

[run]
max_open_review = 2

[cross_audit]
require_different_provider = false
audit_scope = "head_lock"
score_gate = false
min_average_score = 50
min_criterion_score = 40

[pipeline]
plan_stage = true
"#;
    fs::write(local_dir.path().join(LOCAL_CONFIG_REL), local_toml).unwrap();

    // Environment tightens oracle, store compliance, and min_android_target_sdk.
    let env = env_map(&[
        ("AAF_RED_GREEN_ORACLE", "block"),
        ("AAF_STORE_COMPLIANCE", "always"),
        ("AAF_MIN_ANDROID_TARGET_SDK", "35"),
    ]);

    let tightened = tighten_trusted_base(base, &global, Some(local_dir.path()), &env).unwrap();

    // Tier-1 floors held and could not be weakened by Tier-3.
    assert!(tightened.cross_audit.require_different_provider);
    assert_eq!(tightened.cross_audit.audit_scope, AuditScope::Large);
    assert!(tightened.cross_audit.score_gate);
    assert_eq!(tightened.cross_audit.min_average_score, 95);
    assert_eq!(tightened.cross_audit.min_criterion_score, 92);

    // Tier-3 forbidden loosenings were rejected.
    assert!(tightened.proof.required);
    assert_eq!(tightened.commands.test_scoped, "cargo test");
    assert!(
        tightened
            .gate
            .head_lock_globs
            .iter()
            .any(|g| g == "tests/**")
    );

    // Tier-3 valid tightenings took effect.
    assert!(
        tightened
            .gate
            .head_lock_globs
            .iter()
            .any(|g| g == "security/**")
    );
    assert!(!tightened.gate.docs_fast_path);
    assert_eq!(tightened.run.max_open_review, 2);
    assert!(tightened.pipeline.plan_stage);

    // Env tightenings took effect on top of Tier-2 + Tier-3.
    assert_eq!(tightened.gate.red_green_oracle, OracleMode::Block);
    assert_eq!(tightened.gate.store_compliance, StoreMode::Always);
    assert_eq!(tightened.gate.min_android_target_sdk, 35);

    // Attempting to lower via env is ignored.
    let loosen_env = env_map(&[
        ("AAF_RED_GREEN_ORACLE", "off"),
        ("AAF_STORE_COMPLIANCE", "off"),
        ("AAF_MIN_ANDROID_TARGET_SDK", "30"),
    ]);
    let kept = tighten_trusted_base(tightened.clone(), &global, None, &loosen_env).unwrap();
    assert_eq!(kept.gate.red_green_oracle, OracleMode::Block);
    assert_eq!(kept.gate.store_compliance, StoreMode::Always);
    assert_eq!(kept.gate.min_android_target_sdk, 35);

    // Process-env wrapper with no local file present.
    let clean_dir = TempDir::new().unwrap();
    let from_env =
        tighten_trusted_base_from_env(RepoConfig::default(), &global, Some(clean_dir.path()))
            .unwrap();
    assert!(from_env.cross_audit.require_different_provider);
    assert_eq!(from_env.run.max_open_review, 4);

    // Malformed Tier-3 file fails closed with Parse error.
    fs::write(local_dir.path().join(LOCAL_CONFIG_REL), "not valid [toml").unwrap();
    let err = tighten_trusted_base(
        RepoConfig::default(),
        &global,
        Some(local_dir.path()),
        &env_map(&[]),
    )
    .unwrap_err();
    assert!(matches!(err, ConfigError::Parse { .. }), "got {err:?}");
}

#[test]
fn common_receipt_guard_blocks_dirty_ignored_worktree_unless_explicitly_allowed() {
    let repo = TempDir::new().unwrap();
    init_repo(repo.path());

    let mut cfg = RepoConfig::default();
    cfg.commands.lint = "true".to_string();
    cfg.commands.test_scoped = "true".to_string();
    let machine = MachineConfig::default();
    let probe = SystemProbe::detect();
    let noop = |_: &Progress<'_>| {};
    let throttle = Throttle::system(&machine, &probe, &noop);
    let common_id = stats_repo_id(repo.path());
    let has_common = |data: &Path, fp: &str| {
        let key = aaf_fp::load_key(data).unwrap();
        aaf_fp::read_common_receipt(data, &key, &common_id, fp).is_ok()
    };

    // 1. Clean worktree (0 ignored files): writes BOTH local and common receipt.
    let data_clean = TempDir::new().unwrap();
    let clean_run = run_gate_reporting(
        data_clean.path(),
        repo.path(),
        &cfg,
        GateMode::Scoped,
        &FixedClock(100),
        &throttle,
    )
    .unwrap();
    assert_eq!(clean_run.receipt.status, GateStatus::Clear);
    let key_clean = aaf_fp::load_key(data_clean.path()).unwrap();
    assert!(aaf_fp::read_receipt(data_clean.path(), &key_clean, repo.path()).is_ok());
    assert!(
        has_common(data_clean.path(), &clean_run.receipt.tree_fp),
        "clean worktree should publish common_receipt"
    );

    // 2. Plant `.gitignore`d marker `target/.aaf_pass`: local receipt ONLY.
    fs::create_dir_all(repo.path().join("target")).unwrap();
    fs::write(repo.path().join("target/.aaf_pass"), "1\n").unwrap();
    let data_dirty = TempDir::new().unwrap();
    let dirty_run = run_gate_reporting(
        data_dirty.path(),
        repo.path(),
        &cfg,
        GateMode::Scoped,
        &FixedClock(200),
        &throttle,
    )
    .unwrap();
    assert_eq!(dirty_run.receipt.status, GateStatus::Clear);
    let key_dirty = aaf_fp::load_key(data_dirty.path()).unwrap();
    assert!(aaf_fp::read_receipt(data_dirty.path(), &key_dirty, repo.path()).is_ok());
    assert!(
        !has_common(data_dirty.path(), &dirty_run.receipt.tree_fp),
        "dirty-ignored worktree must NOT publish common_receipt"
    );

    // 3. Same dirty-ignored worktree WITH `allow_common_receipt_write: true` publishes.
    let data_allowed = TempDir::new().unwrap();
    let allowed_run = run_gate_with_options(
        data_allowed.path(),
        repo.path(),
        &cfg,
        GateMode::Scoped,
        &FixedClock(300),
        &throttle,
        GateRunOptions {
            policy: HiddenPolicy::Warn,
            allow_common_receipt_write: true,
            ..GateRunOptions::default()
        },
    )
    .unwrap();
    assert_eq!(allowed_run.receipt.status, GateStatus::Clear);
    assert!(
        has_common(data_allowed.path(), &allowed_run.receipt.tree_fp),
        "allow_common_receipt_write=true should publish common_receipt"
    );

    // 4. Self-deleting gitignored bypass file (`target/.aaf_pass` exists BEFORE
    //    gate runs; `test_scoped` deletes it and exits 0): MUST NOT publish `common_receipt`.
    assert!(repo.path().join("target/.aaf_pass").exists());
    let mut self_del_cfg = cfg.clone();
    self_del_cfg.commands.test_scoped = "test -f target/.aaf_pass && rm -rf target".to_string();
    let data_self_del = TempDir::new().unwrap();
    let self_del_run = run_gate_reporting(
        data_self_del.path(),
        repo.path(),
        &self_del_cfg,
        GateMode::Scoped,
        &FixedClock(400),
        &throttle,
    )
    .unwrap();
    assert_eq!(self_del_run.receipt.status, GateStatus::Clear);
    assert!(!repo.path().join("target").exists());
    assert!(
        !has_common(data_self_del.path(), &self_del_run.receipt.tree_fp),
        "self-deleting gitignored bypass file must NOT publish common_receipt"
    );
}

#[test]
fn build_cache_dirs_excludes_sccache_and_wrapper_var_list_is_complete() {
    assert!(!BUILD_CACHE_DIRS.iter().any(|dir| dir.contains("sccache")));
    assert_eq!(DEFAULT_WARM_SLOTS_REL, ".cache/autofactory/warm-slots");
    for expected in [
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_BUILD_RUSTC_WRAPPER",
        "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
        "CARGO_TARGET_DIR",
        "SCCACHE_DIR",
        "SCCACHE_CONF",
        "SCCACHE_SERVER_PORT",
        "SCCACHE_SERVER_UDS",
        "SCCACHE_ENDPOINT",
        "SCCACHE_BUCKET",
        "SCCACHE_REDIS",
        "SCCACHE_MEMCACHED",
        "SCCACHE_GCS_BUCKET",
        "CCACHE_DIR",
        "CCACHE_CONFIGPATH",
        "CCACHE_PREFIX",
        "CCACHE_REMOTE_STORAGE",
    ] {
        assert!(
            COMPILER_WRAPPER_ENV_VARS.contains(&expected),
            "missing {expected}"
        );
    }
    for pair in [
        ("RUSTC_WRAPPER", ""),
        ("RUSTC_WORKSPACE_WRAPPER", ""),
        ("CCACHE_DISABLE", "1"),
    ] {
        assert!(
            COMPILER_WRAPPER_DISABLE_ENV.contains(&pair),
            "missing {pair:?}"
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn sandbox_denies_sccache_writes_and_warm_slots_read_write() {
    use std::os::unix::fs::PermissionsExt;

    use aaf_domain::ports::{AgentRequest, AgentRunner, SandboxPolicy};
    use aaf_engine::executors::ExecEnv;
    use aaf_engine::executors::claude::ClaudeCodeRunner;

    if !Path::new("/usr/bin/sandbox-exec").exists() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let home = root.join("home");
    let wt = root.join("wt");
    let scratch = root.join("scratch");
    let custom_warm = scratch.join("custom-warm");
    let default_warm = home.join(DEFAULT_WARM_SLOTS_REL);
    let sccache = home.join(".cache/sccache");
    for dir in [&wt, &custom_warm, &default_warm, &sccache] {
        fs::create_dir_all(dir).unwrap();
    }
    fs::write(default_warm.join("slot.txt"), "DEFAULT-WARM-SECRET\n").unwrap();
    fs::write(custom_warm.join("slot.txt"), "CUSTOM-WARM-SECRET\n").unwrap();

    let script = format!(
        "#!/bin/sh\n\
         ( echo poison > '{0}/obj' ) 2>/dev/null && echo sccache-write-ok || echo sccache-write-denied\n\
         ( cat '{1}/slot.txt' ) 2>/dev/null && echo warm-default-read-ok || echo warm-default-read-denied\n\
         ( echo x > '{1}/evil' ) 2>/dev/null && echo warm-default-write-ok || echo warm-default-write-denied\n\
         ( cat '{2}/slot.txt' ) 2>/dev/null && echo warm-custom-read-ok || echo warm-custom-read-denied\n\
         ( echo x > '{2}/evil' ) 2>/dev/null && echo warm-custom-write-ok || echo warm-custom-write-denied\n",
        sccache.display(),
        default_warm.display(),
        custom_warm.display(),
    );
    let fake_claude = root.join("claude");
    fs::write(&fake_claude, script).unwrap();
    fs::set_permissions(&fake_claude, fs::Permissions::from_mode(0o755)).unwrap();

    let env = ExecEnv {
        home: Some(home.to_string_lossy().into_owned()),
        // Even with scratch in temp_dirs, custom_warm inside it stays closed.
        temp_dirs: vec![scratch.to_string_lossy().into_owned()],
        parent_env: vec![
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            (
                "AAF_WARM_SLOTS_DIR".to_string(),
                custom_warm.to_string_lossy().into_owned(),
            ),
        ],
        ..ExecEnv::default()
    };
    let out = ClaudeCodeRunner::new(fake_claude.to_string_lossy(), env)
        .run(&AgentRequest {
            run_id: format!("phase-a-{}", uuid::Uuid::new_v4()),
            prompt: "unused".to_string(),
            cwd: wt.to_string_lossy().into_owned(),
            account: None,
            timeout_secs: 60,
            sandbox: SandboxPolicy {
                enabled: true,
                ..SandboxPolicy::default()
            },
        })
        .unwrap();
    for expected in [
        "sccache-write-denied",
        "warm-default-read-denied",
        "warm-default-write-denied",
        "warm-custom-read-denied",
        "warm-custom-write-denied",
    ] {
        assert!(
            out.log_tail.contains(expected),
            "missing {expected}:\n{}",
            out.log_tail
        );
    }
    assert!(!out.log_tail.contains("DEFAULT-WARM-SECRET"));
    assert!(!out.log_tail.contains("CUSTOM-WARM-SECRET"));
}

#[test]
fn strip_compiler_wrappers_strips_light_and_heavy_commands() {
    if std::env::var_os("AAF_PHASE_A_STRIP_CHILD").is_none() {
        let exe = std::env::current_exe().expect("current test binary");
        let mut cmd = Command::new(exe);
        cmd.args([
            "--exact",
            "strip_compiler_wrappers_strips_light_and_heavy_commands",
            "--nocapture",
        ])
        .env("AAF_PHASE_A_STRIP_CHILD", "1");
        for var in COMPILER_WRAPPER_ENV_VARS {
            cmd.env(var, format!("/poison/{var}"));
        }
        cmd.env("CCACHE_DISABLE", "0");
        let out = cmd.output().expect("spawn child test");
        assert!(
            out.status.success(),
            "child test failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        return;
    }

    assert_eq!(
        std::env::var("RUSTC_WRAPPER").unwrap(),
        "/poison/RUSTC_WRAPPER"
    );
    assert_eq!(
        std::env::var("CARGO_TARGET_DIR").unwrap(),
        "/poison/CARGO_TARGET_DIR"
    );
    assert_eq!(std::env::var("SCCACHE_DIR").unwrap(), "/poison/SCCACHE_DIR");

    let repo = TempDir::new().unwrap();
    init_repo(repo.path());
    git(repo.path(), &["checkout", "-q", "-b", "feat/work"]);
    fs::write(repo.path().join("src/lib.rs"), "pub fn v() -> i32 { 2 }\n").unwrap();
    fs::create_dir_all(repo.path().join("tests")).unwrap();
    fs::write(repo.path().join("tests/smoke.rs"), "#[test]\nfn ok() {}\n").unwrap();

    let probe_dir = TempDir::new().unwrap();
    let oracle_out = probe_dir.path().join("oracle_env.txt");
    let probe_cmd = "printf 'RW=%s:%s RWW=%s:%s CTD=%s:%s CCD=%s CBRW=%s CBRWW=%s SD=%s SC=%s CD=%s\\n' \
        \"${RUSTC_WRAPPER+set}\" \"${RUSTC_WRAPPER:-}\" \
        \"${RUSTC_WORKSPACE_WRAPPER+set}\" \"${RUSTC_WORKSPACE_WRAPPER:-}\" \
        \"${CARGO_TARGET_DIR+set}\" \"${CARGO_TARGET_DIR:-}\" \
        \"${CCACHE_DISABLE:-}\" \"${CARGO_BUILD_RUSTC_WRAPPER:-}\" \
        \"${CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER:-}\" \
        \"${SCCACHE_DIR:-}\" \"${SCCACHE_CONF:-}\" \"${CCACHE_DIR:-}\"";
    let test_cmd = format!(
        "if [ -n \"${{AAF_TEST_FILES_FILE:-}}\" ]; then {probe_cmd} > '{}'; echo 'FAIL: oracle red' >&2; exit 1; fi; {probe_cmd}",
        oracle_out.display()
    );
    assert!(!is_heavy("lint", probe_cmd), "lint must take light path");
    assert!(
        is_heavy("test_scoped", &test_cmd),
        "test_scoped must take heavy path"
    );

    let mut cfg = RepoConfig::default();
    cfg.commands.lint = probe_cmd.to_string();
    cfg.commands.test_scoped = test_cmd;
    cfg.gate.base_ref = "main".to_string();
    cfg.gate.red_green_oracle = OracleMode::Block;
    cfg.gate.head_lock_globs.clear();
    let machine = MachineConfig::default();
    let probe = SystemProbe::detect();
    let noop = |_: &Progress<'_>| {};
    let throttle = Throttle::system(&machine, &probe, &noop);

    let run_case = |ts, opts| {
        let data = TempDir::new().unwrap();
        let run = run_gate_with_options(
            data.path(),
            repo.path(),
            &cfg,
            GateMode::Scoped,
            &FixedClock(ts),
            &throttle,
            opts,
        )
        .unwrap();
        assert_eq!(run.receipt.status, GateStatus::Clear);
        assert!(
            run.receipt
                .reasons
                .iter()
                .any(|r| r.contains("oracle: red_green=proven")),
            "oracle did not run: {:?}",
            run.receipt.reasons
        );
        let oracle_text = fs::read_to_string(&oracle_out).unwrap();
        fs::remove_file(&oracle_out).unwrap();
        let mut outputs: Vec<(String, String)> = run
            .receipt
            .commands
            .into_iter()
            .map(|c| (c.name, c.log_tail))
            .collect();
        outputs.push(("oracle".to_string(), oracle_text));
        outputs
    };

    // 1. strip_compiler_wrappers = false: light, heavy, and oracle inherit vars.
    for (label, text) in run_case(10, GateRunOptions::default()) {
        assert!(
            text.contains("RW=set:/poison/RUSTC_WRAPPER")
                && text.contains("CTD=set:/poison/CARGO_TARGET_DIR")
                && text.contains("CCD=0")
                && text.contains("SD=/poison/SCCACHE_DIR"),
            "{label} should inherit vars when stripping is off: {text:?}"
        );
    }

    // 2. strip_compiler_wrappers = true: light, heavy, and oracle neutralize all vars.
    let expected = "RW=set: RWW=set: CTD=: CCD=1 CBRW= CBRWW= SD= SC= CD=";
    for (label, text) in run_case(
        20,
        GateRunOptions {
            strip_compiler_wrappers: true,
            ..GateRunOptions::default()
        },
    ) {
        assert_eq!(
            text.trim(),
            expected,
            "{label} leaked or failed to neutralize wrapper env: {text:?}"
        );
    }
}
