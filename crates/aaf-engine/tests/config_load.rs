#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Tier precedence, defaults, tier-2-only fields and error reporting of the
//! three-tier configuration loader.

use std::fs;
use std::path::{Path, PathBuf};

use aaf_domain::config::{GlobalConfig, RepoConfig};
use aaf_engine::config::{
    ConfigError, ConfigInputs, ConfigOverlay, EffectiveConfig, LOCAL_CONFIG_REL, REPO_CONFIG_REL,
    global_config_path, load,
};
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("repo")).unwrap();
        Fixture { dir }
    }

    fn repo(&self) -> PathBuf {
        self.dir.path().join("repo")
    }

    fn global_path(&self) -> PathBuf {
        self.dir.path().join("cfg").join("config.toml")
    }

    fn global(&self, body: &str) -> &Fixture {
        write(&self.global_path(), body);
        self
    }

    fn tier2(&self, body: &str) -> &Fixture {
        write(&self.repo().join(REPO_CONFIG_REL), body);
        self
    }

    fn local(&self, body: &str) -> &Fixture {
        write(&self.repo().join(LOCAL_CONFIG_REL), body);
        self
    }

    fn load(
        &self,
        env: &[(&str, &str)],
        cli: ConfigOverlay,
    ) -> Result<EffectiveConfig, ConfigError> {
        let lookup = |key: &str| {
            env.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_string())
        };
        load(&ConfigInputs {
            global_config: Some(self.global_path()),
            repo_root: self.repo(),
            env: &lookup,
            cli,
        })
    }

    fn ok(&self, env: &[(&str, &str)]) -> EffectiveConfig {
        self.load(env, ConfigOverlay::default()).unwrap()
    }
}

fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

#[test]
fn missing_files_give_defaults() {
    let fx = Fixture::new();
    let cfg = fx.ok(&[]);
    assert_eq!(cfg.global, GlobalConfig::default());
    assert_eq!(cfg.repo, RepoConfig::default());
    assert!(cfg.sources.is_empty());
    assert!(cfg.warnings.is_empty());
}

#[test]
fn no_global_path_gives_default_global() {
    let fx = Fixture::new();
    let lookup = |_: &str| None;
    let cfg = load(&ConfigInputs {
        global_config: None,
        repo_root: fx.repo(),
        env: &lookup,
        cli: ConfigOverlay::default(),
    })
    .unwrap();
    assert_eq!(cfg.global, GlobalConfig::default());
}

#[test]
fn global_file_is_read_and_partial_sections_keep_defaults() {
    let fx = Fixture::new();
    fx.global(
        "[accounts]\nmax_concurrent_per_account = 3\n\n[safety]\nowner_github_allowlist = [\"octo\"]\n",
    );
    let cfg = fx.ok(&[]);
    assert_eq!(cfg.global.accounts.max_concurrent_per_account, 3);
    assert_eq!(
        cfg.global.accounts.discovery_globs,
        GlobalConfig::default().accounts.discovery_globs
    );
    assert_eq!(cfg.global.safety.owner_github_allowlist, vec!["octo"]);
    assert_eq!(cfg.sources, vec![fx.global_path().display().to_string()]);
}

#[test]
fn repo_file_overrides_defaults_field_by_field() {
    let fx = Fixture::new();
    fx.tier2(
        "[project]\nname = \"demo\"\nprofile = \"rust\"\n\n[commands]\ntest_full = \"cargo test\"\n\n[gate]\nbase_ref = \"origin/dev\"\n\n[unblocker]\nmax_unblock_cycles = 2\n",
    );
    let cfg = fx.ok(&[]);
    assert_eq!(cfg.repo.project.name, "demo");
    assert_eq!(cfg.repo.project.profile, "rust");
    assert_eq!(cfg.repo.commands.test_full, "cargo test");
    assert_eq!(cfg.repo.commands.build, "");
    assert_eq!(cfg.repo.gate.base_ref, "origin/dev");
    assert_eq!(
        cfg.repo.gate.head_lock_globs,
        RepoConfig::default().gate.head_lock_globs
    );
    assert_eq!(cfg.repo.unblocker.max_unblock_cycles, 2);
    assert_eq!(cfg.repo.run, RepoConfig::default().run);
}

#[test]
fn sources_list_every_file_read_in_tier_order() {
    let fx = Fixture::new();
    fx.global("").tier2("").local("");
    let cfg = fx.ok(&[]);
    assert_eq!(
        cfg.sources,
        vec![
            fx.global_path().display().to_string(),
            fx.repo().join(REPO_CONFIG_REL).display().to_string(),
            fx.repo().join(LOCAL_CONFIG_REL).display().to_string(),
        ]
    );
}

#[test]
fn project_and_run_precedence_local_over_repo() {
    let fx = Fixture::new();
    fx.tier2("[project]\nname = \"a\"\nprofile = \"web\"\n\n[run]\nexecutor = \"claude\"\nmax_parallel = 5\nrun_timeout_secs = 100\n");
    fx.local("[project]\nname = \"b\"\n\n[run]\nexecutor = \"custom\"\nexecutor_cmd = [\"x\", \"{prompt}\"]\nrun_timeout_secs = 7\n");
    let cfg = fx.ok(&[]);
    assert_eq!(cfg.repo.project.name, "b");
    // Absent in the local file: the tier 2 value survives.
    assert_eq!(cfg.repo.project.profile, "web");
    // The executor and its command belong to tier 2 (docs/PLAN.md C.4): the
    // local values are dropped, one warning each.
    assert_eq!(cfg.repo.run.executor, "claude");
    assert!(cfg.repo.run.executor_cmd.is_empty());
    assert_eq!(cfg.repo.run.run_timeout_secs, 7);
    assert_eq!(cfg.repo.run.max_parallel, 5);
    assert_eq!(cfg.warnings.len(), 2, "{:?}", cfg.warnings);
    assert!(cfg.warnings.iter().all(|w| w.contains("run.executor")));
}

#[test]
fn local_value_equal_to_the_default_still_overrides_tier_two() {
    let fx = Fixture::new();
    fx.tier2("[run]\nmax_parallel = 5\nauto_merge = true\n");
    // 1 and false are the built-in defaults; they must not be mistaken for "absent".
    fx.local("[run]\nmax_parallel = 1\nauto_merge = false\n");
    let cfg = fx.ok(&[]);
    assert_eq!(cfg.repo.run.max_parallel, 1);
    assert!(!cfg.repo.run.auto_merge);
}

#[test]
fn run_precedence_env_over_local_and_cli_over_env() {
    let fx = Fixture::new();
    fx.tier2("[run]\nmax_parallel = 2\n");
    fx.local("[run]\nmax_parallel = 3\nauto_merge = false\n");

    let cfg = fx.ok(&[]);
    assert_eq!(cfg.repo.run.max_parallel, 3);

    let env = [("AAF_MAX_PARALLEL", "4"), ("AAF_AUTO_MERGE", "true")];
    let cfg = fx.ok(&env);
    assert_eq!(cfg.repo.run.max_parallel, 4);
    // The environment cannot switch auto-merge on (docs/PLAN.md C.4).
    assert!(!cfg.repo.run.auto_merge);
    assert_eq!(cfg.warnings.len(), 1, "{:?}", cfg.warnings);

    let mut cli = ConfigOverlay::default();
    cli.run.max_parallel = Some(8);
    cli.run.auto_merge = Some(false);
    let cfg = fx.load(&env, cli).unwrap();
    assert_eq!(cfg.repo.run.max_parallel, 8);
    assert!(!cfg.repo.run.auto_merge);

    // The command-line flag is the one overlay that can.
    let mut cli = ConfigOverlay::default();
    cli.run.auto_merge = Some(true);
    assert!(fx.load(&env, cli).unwrap().repo.run.auto_merge);
}

#[test]
fn env_booleans_accept_common_spellings_and_empty_means_unset() {
    let fx = Fixture::new();
    fx.tier2("[run]\nauto_merge = false\n");
    // Parsed as "true": the environment may not switch auto-merge on, so the
    // value shows up as the warning that it was ignored.
    for raw in ["1", "TRUE", "yes", "on"] {
        let cfg = fx.ok(&[("AAF_AUTO_MERGE", raw)]);
        assert!(!cfg.repo.run.auto_merge, "{raw}");
        assert_eq!(cfg.warnings.len(), 1, "{raw}: {:?}", cfg.warnings);
        assert!(cfg.warnings[0].contains("run.auto_merge = true"), "{raw}");
    }
    fx.tier2("[run]\nauto_merge = true\n");
    for raw in ["0", "false", "No", "off"] {
        assert!(!fx.ok(&[("AAF_AUTO_MERGE", raw)]).repo.run.auto_merge);
    }
    assert!(fx.ok(&[("AAF_AUTO_MERGE", "")]).repo.run.auto_merge);
}

#[test]
fn invalid_env_value_is_an_error_naming_the_variable() {
    let fx = Fixture::new();
    for (var, value) in [
        ("AAF_AUTO_MERGE", "maybe"),
        ("AAF_MAX_PARALLEL", "many"),
        ("AAF_MIN_ANDROID_TARGET_SDK", "-1"),
        ("AAF_STORE_COMPLIANCE", "sometimes"),
    ] {
        let err = fx
            .load(&[(var, value)], ConfigOverlay::default())
            .unwrap_err();
        match &err {
            ConfigError::InvalidEnv {
                var: got, value: v, ..
            } => {
                assert_eq!(got, var);
                assert_eq!(v, value);
            }
            other => panic!("expected InvalidEnv, got {other:?}"),
        }
        assert!(err.to_string().contains(var));
    }
}

#[test]
fn commands_come_from_tier_two_only() {
    let fx = Fixture::new();
    fx.tier2("[commands]\ntest_full = \"cargo test\"\nlint = \"cargo clippy\"\n");
    fx.local("[commands]\ntest_full = \"true\"\nlint = \"\"\n");
    let cfg = fx.ok(&[("AAF_COMMANDS_TEST_FULL", "true")]);
    assert_eq!(cfg.repo.commands.test_full, "cargo test");
    assert_eq!(cfg.repo.commands.lint, "cargo clippy");
    assert_eq!(cfg.warnings.len(), 2, "{:?}", cfg.warnings);
    assert!(cfg.warnings[0].contains("[commands]"));
    assert!(cfg.warnings[0].contains("config.local.toml"));
    assert!(cfg.warnings[1].contains("AAF_COMMANDS_TEST_FULL"));
}

#[test]
fn commands_in_tier_two_alone_produce_no_warning() {
    let fx = Fixture::new();
    fx.tier2("[commands]\ntest_full = \"cargo test\"\n");
    assert!(fx.ok(&[]).warnings.is_empty());
}

#[test]
fn tier_two_only_sections_in_the_local_file_are_ignored_with_a_warning() {
    let fx = Fixture::new();
    fx.global("[safety]\nowner_github_allowlist = [\"octo\"]\n");
    fx.tier2("[unblocker]\nmax_unblock_cycles = 2\n");
    fx.local("[unblocker]\nmax_unblock_cycles = 99\n\n[safety]\nowner_github_allowlist = [\"mallory\"]\n");
    let cfg = fx.ok(&[]);
    assert_eq!(cfg.repo.unblocker.max_unblock_cycles, 2);
    assert_eq!(cfg.global.safety.owner_github_allowlist, vec!["octo"]);
    assert!(cfg.warnings.iter().any(|w| w.contains("[unblocker]")));
    assert!(cfg.warnings.iter().any(|w| w.contains("[safety]")));
}

#[test]
fn broken_file_is_an_error_with_file_and_position_for_every_tier() {
    let broken = "[project]\nname = \"ok\"\nprofile = = 1\n";
    for tier in 0..3 {
        let fx = Fixture::new();
        let path = match tier {
            0 => {
                fx.global(broken);
                fx.global_path()
            }
            1 => {
                fx.tier2(broken);
                fx.repo().join(REPO_CONFIG_REL)
            }
            _ => {
                fx.local(broken);
                fx.repo().join(LOCAL_CONFIG_REL)
            }
        };
        let err = fx.load(&[], ConfigOverlay::default()).unwrap_err();
        match &err {
            ConfigError::Parse {
                path: got,
                line,
                column,
                message,
            } => {
                assert_eq!(got, &path);
                assert_eq!((*line, *column), (3, 11));
                assert!(!message.is_empty());
            }
            other => panic!("expected Parse, got {other:?}"),
        }
        let shown = err.to_string();
        assert!(
            shown.contains(&format!("{}:3:11", path.display())),
            "{shown}"
        );
    }
}

#[test]
fn wrong_type_is_a_parse_error_not_a_default() {
    let fx = Fixture::new();
    fx.tier2("[gate]\nmin_android_target_sdk = \"thirty\"\n");
    let err = fx.load(&[], ConfigOverlay::default()).unwrap_err();
    assert!(matches!(err, ConfigError::Parse { line: 2, .. }), "{err:?}");

    let fx = Fixture::new();
    fx.local("[gate]\nstore_compliance = \"never\"\n");
    let err = fx.load(&[], ConfigOverlay::default()).unwrap_err();
    assert!(matches!(err, ConfigError::Parse { line: 2, .. }), "{err:?}");
}

#[test]
fn unreadable_file_is_an_io_error() {
    let fx = Fixture::new();
    // A directory where the file should be: exists, but cannot be read as a file.
    fs::create_dir_all(fx.repo().join(REPO_CONFIG_REL)).unwrap();
    let err = fx.load(&[], ConfigOverlay::default()).unwrap_err();
    assert!(matches!(err, ConfigError::Io { .. }), "{err:?}");
}

#[test]
fn unknown_profile_is_rejected_and_known_ones_pass() {
    let fx = Fixture::new();
    fx.tier2("[project]\nprofile = \"andriod\"\n");
    let err = fx.load(&[], ConfigOverlay::default()).unwrap_err();
    assert!(matches!(err, ConfigError::InvalidProfile { .. }), "{err:?}");

    for profile in aaf_engine::config::PROFILES.iter().chain(["auto"].iter()) {
        let fx = Fixture::new();
        fx.tier2(&format!("[project]\nprofile = \"{profile}\"\n"));
        assert_eq!(&fx.ok(&[]).repo.project.profile, profile);
    }
}

#[test]
fn global_config_path_prefers_aaf_config_dir_then_home() {
    let both = |key: &str| match key {
        "AAF_CONFIG_DIR" => Some("/x/cfg".to_string()),
        "HOME" => Some("/home/u".to_string()),
        _ => None,
    };
    assert_eq!(
        global_config_path(&both),
        Some(PathBuf::from("/x/cfg/config.toml"))
    );
    let home = |key: &str| (key == "HOME").then(|| "/home/u".to_string());
    assert_eq!(
        global_config_path(&home),
        Some(PathBuf::from("/home/u/.config/autofactory/config.toml"))
    );
    let empty = |key: &str| (key == "AAF_CONFIG_DIR").then(String::new);
    assert_eq!(global_config_path(&empty), None);
}

fn parse_error(fx: &Fixture) -> (PathBuf, String) {
    match fx.load(&[], ConfigOverlay::default()) {
        Err(ConfigError::Parse { path, message, .. }) => (path, message),
        other => panic!("expected a parse error, got {other:?}"),
    }
}

#[test]
fn misspelt_key_is_an_error_naming_file_and_key_in_every_tier() {
    let fx = Fixture::new();
    fx.global("[safety]\nowner_github_allowlst = [\"me\"]\n");
    let (path, message) = parse_error(&fx);
    assert_eq!(path, fx.global_path());
    assert!(message.contains("owner_github_allowlst"), "{message}");

    let fx = Fixture::new();
    fx.global("[saftey]\n");
    let (path, message) = parse_error(&fx);
    assert_eq!(path, fx.global_path());
    assert!(message.contains("saftey"), "{message}");

    let fx = Fixture::new();
    fx.tier2("[gate]\nstore_complience = \"always\"\n");
    let (path, message) = parse_error(&fx);
    assert_eq!(path, fx.repo().join(REPO_CONFIG_REL));
    assert!(message.contains("store_complience"), "{message}");

    let fx = Fixture::new();
    fx.tier2("[commands]\ntest_ful = \"cargo test\"\n");
    let (_, message) = parse_error(&fx);
    assert!(message.contains("test_ful"), "{message}");

    // A typo in a tightening override must not silently leave the gate loose.
    let fx = Fixture::new();
    fx.local("[gate]\nstore_complience = \"always\"\n");
    let (path, message) = parse_error(&fx);
    assert_eq!(path, fx.repo().join(LOCAL_CONFIG_REL));
    assert!(message.contains("store_complience"), "{message}");

    let fx = Fixture::new();
    fx.local("[gatee]\n");
    let (_, message) = parse_error(&fx);
    assert!(message.contains("gatee"), "{message}");
}

#[test]
fn correctly_spelt_keys_in_every_section_still_load() {
    let fx = Fixture::new();
    fx.global(
        "[accounts]\nmax_concurrent_per_account = 2\n[safety]\nowner_github_allowlist = [\"me\"]\n[notifications]\nwebhook_url = \"\"\n",
    );
    fx.tier2(
        "[project]\nname = \"x\"\n[commands]\nlint = \"l\"\n[gate]\nbase_ref = \"main\"\n[unblocker]\nmax_unblock_cycles = 2\n[run]\nmax_parallel = 2\n",
    );
    fx.local(
        "[project]\nname = \"y\"\n[gate]\nstore_compliance = \"always\"\n[run]\nmax_parallel = 3\n",
    );
    let cfg = fx.ok(&[]);
    assert_eq!(cfg.global.accounts.max_concurrent_per_account, 2);
    assert_eq!(cfg.repo.project.name, "y");
    assert_eq!(cfg.repo.run.max_parallel, 3);
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
}

#[test]
fn repro_command_is_read_from_tier_two_only() {
    let fx = Fixture::new();
    assert_eq!(fx.ok(&[]).repo.commands.repro, "");
    fx.tier2("[commands]\nrepro = \"./scripts/repro.sh\"\n");
    fx.local("[commands]\nrepro = \"true\"\n");
    let cfg = fx.ok(&[("AAF_COMMANDS_REPRO", "true")]);
    assert_eq!(cfg.repo.commands.repro, "./scripts/repro.sh");
    assert_eq!(cfg.warnings.len(), 2, "{:?}", cfg.warnings);
    assert!(cfg.warnings[0].contains("[commands]"));
    assert!(cfg.warnings[1].contains("AAF_COMMANDS_REPRO"));
}

#[test]
fn run_model_and_effort_are_optional_and_the_local_file_may_override_them() {
    let fx = Fixture::new();
    let cfg = fx.ok(&[]);
    assert_eq!(
        (cfg.repo.run.model.as_str(), cfg.repo.run.effort.as_str()),
        ("", "")
    );
    assert!(cfg.repo.run.env_pass.is_empty());

    fx.tier2("[run]\nmodel = \"opus\"\neffort = \"high\"\nenv_pass = [\"SENTRY_DSN\"]\n");
    let cfg = fx.ok(&[]);
    assert_eq!(
        (cfg.repo.run.model.as_str(), cfg.repo.run.effort.as_str()),
        ("opus", "high")
    );
    assert_eq!(cfg.repo.run.env_pass, ["SENTRY_DSN"]);

    fx.local("[run]\nmodel = \"sonnet\"\neffort = \"\"\n");
    let cfg = fx.ok(&[]);
    assert_eq!(
        (cfg.repo.run.model.as_str(), cfg.repo.run.effort.as_str()),
        ("sonnet", "")
    );
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
}

#[test]
fn run_env_pass_is_read_from_tier_two_only() {
    let fx = Fixture::new();
    fx.tier2("[run]\nenv_pass = [\"SENTRY_DSN\"]\n");
    fx.local("[run]\nenv_pass = [\"SENTRY_DSN\", \"GITHUB_TOKEN\"]\n");
    let cfg = fx.ok(&[]);
    assert_eq!(cfg.repo.run.env_pass, ["SENTRY_DSN"]);
    assert!(
        cfg.warnings
            .iter()
            .any(|w| w.contains("run.env_pass ignored")),
        "{:?}",
        cfg.warnings
    );
}

/// `executor = "auto"` (the built-in default) takes the machine's auditor.
#[test]
fn an_auto_auditor_takes_the_global_one() {
    let fx = Fixture::new();
    fx.global("[cross_audit]\nexecutor = \"codex\"\nmodel = \"o9\"\n");
    fx.tier2("[cross_audit]\nexecutor = \"auto\"\n");
    let cfg = fx.ok(&[]);
    assert_eq!(cfg.repo.cross_audit.executor, "codex");
    assert_eq!(cfg.repo.cross_audit.model, "o9");
    fx.tier2("[cross_audit]\nexecutor = \"gemini\"\n");
    assert_eq!(fx.ok(&[]).repo.cross_audit.executor, "gemini");
}

#[test]
fn proof_required_defaults_true_for_mobile_and_game_and_false_for_backend_and_rust() {
    use aaf_engine::config::{default_proof_required, default_proof_trigger_globs};

    for profile in ["android", "ios", "flutter", "react-native", "expo", "unity"] {
        assert!(
            default_proof_required(profile),
            "expected default_proof_required=true for {profile}"
        );
        assert!(
            !default_proof_trigger_globs(profile).is_empty(),
            "expected default trigger_globs for {profile}"
        );
    }
    for profile in ["backend", "rust", "web", "auto"] {
        assert!(
            !default_proof_required(profile),
            "expected default_proof_required=false for {profile}"
        );
    }
    // Explicit repo pin always wins over profile default even when built-in defaults are on.
    let fx = Fixture::new();
    fx.tier2("[project]\nprofile = \"android\"\n[proof]\nrequired = false\n");
    assert!(!fx.ok(&[]).repo.proof.required);
}
