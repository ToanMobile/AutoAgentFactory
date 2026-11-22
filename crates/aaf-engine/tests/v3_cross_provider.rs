#![allow(clippy::unwrap_used, clippy::expect_used)]
//! V3 cross-provider adversarial audit (`[cross_audit]`), tier-3 tighten-only
//! enforcement, `resolve_audit_policy`, `build_audit_command`, full factory
//! approval flow and conservative stack detection.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::config::{CommandsConfig, CrossAuditConfig};
use aaf_domain::ports::{AccountStore, Clock};
use aaf_engine::config::{
    ConfigError, ConfigInputs, ConfigOverlay, EffectiveConfig, LOCAL_CONFIG_REL, REPO_CONFIG_REL,
    detect_stack, load,
};
use aaf_engine::db::Db;
use aaf_engine::factory::adapters::{RouterPool, provider_for};
use aaf_engine::factory::{
    AccountPool, build_audit_command, configured_audit_provider, resolve_audit_policy,
};
use tempfile::TempDir;

// ---- config tier helpers --------------------------------------------------

struct CfgFixture {
    dir: TempDir,
}

impl CfgFixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("repo")).unwrap();
        Self { dir }
    }

    fn repo(&self) -> PathBuf {
        self.dir.path().join("repo")
    }

    fn global_path(&self) -> PathBuf {
        self.dir.path().join("cfg/config.toml")
    }

    fn write(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    fn global(&self, body: &str) -> &Self {
        Self::write(&self.global_path(), body);
        self
    }

    fn tier2(&self, body: &str) -> &Self {
        Self::write(&self.repo().join(REPO_CONFIG_REL), body);
        self
    }

    fn local(&self, body: &str) -> &Self {
        Self::write(&self.repo().join(LOCAL_CONFIG_REL), body);
        self
    }

    fn load(&self) -> Result<EffectiveConfig, ConfigError> {
        let lookup = |_: &str| None;
        load(&ConfigInputs {
            global_config: Some(self.global_path()),
            repo_root: self.repo(),
            env: &lookup,
            cli: ConfigOverlay::default(),
        })
    }
}

#[test]
fn cross_audit_tier1_defaults_and_tier2_overrides() {
    let fx = CfgFixture::new();
    fx.global(
        "[cross_audit]\n\
         executor = \"gemini\"\n\
         model = \"gemini-2.5-pro\"\n\
         effort = \"high\"\n\
         require_different_provider = true\n",
    );
    let from_global = fx.load().unwrap();
    assert_eq!(from_global.repo.cross_audit.executor, "gemini");
    assert_eq!(from_global.repo.cross_audit.model, "gemini-2.5-pro");
    assert_eq!(from_global.repo.cross_audit.effort, "high");
    assert!(from_global.repo.cross_audit.require_different_provider);
    assert!(from_global.warnings.is_empty());

    // Tier 2 switching executor does NOT inherit tier-1 model/effort/cmd (prevents cross-provider flag bleed),
    // while require_different_provider is OR-ed across tier 1 and tier 2.
    fx.tier2(
        "[cross_audit]\n\
         executor = \"codex\"\n\
         model = \"o3\"\n\
         require_different_provider = false\n",
    );
    let merged = fx.load().unwrap();
    assert_eq!(merged.repo.cross_audit.executor, "codex");
    assert_eq!(merged.repo.cross_audit.model, "o3");
    assert_eq!(merged.repo.cross_audit.effort, "");
    assert!(merged.repo.cross_audit.require_different_provider);
    assert!(merged.warnings.is_empty());

    // Keeping the same executor in tier 2 still inherits tier-1 effort.
    fx.tier2("[cross_audit]\nexecutor = \"gemini\"\nmodel = \"gemini-2.5-flash\"\n");
    let same_exec = fx.load().unwrap();
    assert_eq!(same_exec.repo.cross_audit.effort, "high");
}

#[test]
fn cross_audit_tier3_is_tighten_only_and_rejects_executor_override() {
    let fx = CfgFixture::new();
    fx.tier2("[cross_audit]\nexecutor = \"gemini\"\nrequire_different_provider = false\n");
    fx.local("[cross_audit]\nrequire_different_provider = true\n");
    let tightened = fx.load().unwrap();
    assert!(tightened.repo.cross_audit.require_different_provider);
    assert!(tightened.warnings.is_empty(), "{:?}", tightened.warnings);

    // Attempting to weaken true -> false or swap executor/cmd in tier 3 is ignored with warnings.
    let fx2 = CfgFixture::new();
    fx2.tier2(
        "[cross_audit]\n\
         executor = \"gemini\"\n\
         executor_cmd = \"gemini -p {prompt}\"\n\
         require_different_provider = true\n",
    );
    fx2.local(
        "[cross_audit]\n\
         executor = \"custom\"\n\
         executor_cmd = \"true {prompt}\"\n\
         require_different_provider = false\n",
    );
    let resisted = fx2.load().unwrap();
    assert_eq!(resisted.repo.cross_audit.executor, "gemini");
    assert_eq!(resisted.repo.cross_audit.executor_cmd, "gemini -p {prompt}");
    assert!(resisted.repo.cross_audit.require_different_provider);
    assert!(
        resisted
            .warnings
            .iter()
            .any(|w| w.contains("cross_audit.executor")),
        "{:?}",
        resisted.warnings
    );
    assert!(
        resisted
            .warnings
            .iter()
            .any(|w| w.contains("cross_audit.executor_cmd")),
        "{:?}",
        resisted.warnings
    );
    assert!(
        resisted
            .warnings
            .iter()
            .any(|w| w.contains("cross_audit.require_different_provider")),
        "{:?}",
        resisted.warnings
    );
}

#[test]
fn cross_audit_rejects_unknown_fields() {
    let fx = CfgFixture::new();
    fx.tier2("[cross_audit]\nexecutr = \"gemini\"\n");
    let err = fx.load().unwrap_err();
    assert!(matches!(err, ConfigError::Parse { .. }), "{err:?}");
}

// ---- policy & command template unit tests ---------------------------------

#[test]
fn resolve_audit_policy_handles_different_and_same_provider() {
    let (reviewer, same) =
        resolve_audit_policy("claude", Some("acct-a"), "gemini", None, true).unwrap();
    assert_eq!(reviewer, "cross-audit:gemini/default");
    assert!(!same);

    let err =
        resolve_audit_policy("claude", Some("acct-a"), "claude", Some("acct-b"), true).unwrap_err();
    assert!(err.contains("require_different_provider"), "{err}");

    // Case-insensitive provider comparison still fails closed.
    assert!(resolve_audit_policy("claude", Some("acct-a"), "CLAUDE", None, true).is_err());

    let (rev_diff_acct, same_diff_acct) =
        resolve_audit_policy("claude", Some("acct-a"), "claude", Some("acct-b"), false).unwrap();
    assert_eq!(rev_diff_acct, "cross-audit:claude/acct-b");
    assert!(!same_diff_acct);

    let (rev_same_acct, same_same_acct) =
        resolve_audit_policy("claude", Some("acct-a"), "claude", Some("acct-a"), false).unwrap();
    assert_eq!(rev_same_acct, "cross-audit:claude/acct-a");
    assert!(same_same_acct);

    let (rev_unknown, same_unknown) =
        resolve_audit_policy("claude", None, "claude", Some("acct-b"), false).unwrap();
    assert_eq!(rev_unknown, "cross-audit:claude/acct-b");
    assert!(same_unknown);
}

#[test]
fn build_audit_command_supports_builtin_and_custom_executors() {
    let codex = build_audit_command(&CrossAuditConfig {
        executor: "codex".to_string(),
        model: "o3".to_string(),
        effort: "high".to_string(),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        codex,
        vec![
            "codex",
            "exec",
            "-m",
            "o3",
            "-c",
            "model_reasoning_effort=high",
            "{prompt}"
        ]
    );

    let gemini = build_audit_command(&CrossAuditConfig {
        executor: "gemini".to_string(),
        model: "gemini-2.5-pro".to_string(),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        gemini,
        vec!["gemini", "-m", "gemini-2.5-pro", "-p", "{prompt}"]
    );

    let opencode = build_audit_command(&CrossAuditConfig {
        executor: "opencode".to_string(),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(opencode, vec!["opencode", "run", "{prompt}"]);

    let custom_dash = build_audit_command(&CrossAuditConfig {
        executor: "codex".to_string(),
        executor_cmd: "codex exec -".to_string(),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(custom_dash, vec!["codex", "exec", "{prompt}"]);

    let custom = build_audit_command(&CrossAuditConfig {
        executor: "custom".to_string(),
        executor_cmd: "/usr/local/bin/audit --prompt {prompt}".to_string(),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(custom, vec!["/usr/local/bin/audit", "--prompt", "{prompt}"]);

    assert!(
        build_audit_command(&CrossAuditConfig {
            executor: "custom".to_string(),
            ..Default::default()
        })
        .is_err()
    );
    assert!(
        build_audit_command(&CrossAuditConfig {
            executor: "unknown-tool".to_string(),
            ..Default::default()
        })
        .is_err()
    );
}

#[test]
fn conservative_detect_flutter_and_web_rules() {
    let pubspec = "name: app\ndependencies:\n  flutter:\n    sdk: flutter\n";

    let dir_with_test = TempDir::new().unwrap();
    fs::write(dir_with_test.path().join("pubspec.yaml"), pubspec).unwrap();
    fs::create_dir_all(dir_with_test.path().join("test")).unwrap();
    let d_with = detect_stack(dir_with_test.path());
    assert_eq!(d_with.profile, "flutter");
    assert_eq!(d_with.commands.test_scoped, "flutter test");
    assert_eq!(d_with.commands.test_full, "flutter test");
    assert_eq!(d_with.commands.lint, "flutter analyze");

    let dir_no_test = TempDir::new().unwrap();
    fs::write(dir_no_test.path().join("pubspec.yaml"), pubspec).unwrap();
    let d_without = detect_stack(dir_no_test.path());
    assert_eq!(d_without.profile, "flutter");
    assert_eq!(d_without.commands.test_scoped, "");
    assert_eq!(d_without.commands.test_full, "");
    assert!(
        d_without
            .evidence
            .iter()
            .any(|e| e == "pubspec.yaml (no test/ directory)"),
        "{:?}",
        d_without.evidence
    );

    let web_dir = TempDir::new().unwrap();
    fs::write(
        web_dir.path().join("package.json"),
        r#"{"scripts":{"test":"vitest","build":"vite build","lint":"eslint ."}}"#,
    )
    .unwrap();
    fs::write(web_dir.path().join("pnpm-lock.yaml"), "").unwrap();
    let d_web = detect_stack(web_dir.path());
    assert_eq!(d_web.profile, "web");
    assert_eq!(d_web.commands, CommandsConfig::default());
}

struct ZeroClock;
impl Clock for ZeroClock {
    fn now(&self) -> i64 {
        1_000
    }
}

#[test]
fn gemini_maps_to_antigravity_and_provider_account_count_filters_by_provider() {
    assert_eq!(provider_for("gemini"), Some(ProviderKind::Antigravity));
    assert_eq!(provider_for("antigravity"), Some(ProviderKind::Antigravity));
    assert_eq!(provider_for("custom"), None);

    let dir = TempDir::new().unwrap();
    let db = Db::open(&dir.path().join("state.db")).unwrap();
    db.upsert_account(&AccountProfile {
        id: "c1".into(),
        provider: ProviderKind::ClaudeCode,
        config_dir: dir.path().join("c1").to_string_lossy().into_owned(),
        name: "c1".into(),
    })
    .unwrap();
    db.upsert_account(&AccountProfile {
        id: "g1".into(),
        provider: ProviderKind::Antigravity,
        config_dir: dir.path().join("g1").to_string_lossy().into_owned(),
        name: "g1".into(),
    })
    .unwrap();

    let pool_for = |provider| RouterPool {
        accounts: Arc::new(db.clone()),
        runs: Arc::new(db.clone()),
        clock: Arc::new(ZeroClock),
        leases_dir: dir.path().join("leases"),
        max_concurrent_per_account: 1,
        provider,
    };
    assert_eq!(
        pool_for(Some(ProviderKind::ClaudeCode))
            .provider_account_count()
            .unwrap(),
        1
    );
    assert_eq!(
        pool_for(Some(ProviderKind::Antigravity))
            .provider_account_count()
            .unwrap(),
        1
    );
    assert_eq!(
        pool_for(Some(ProviderKind::Codex))
            .provider_account_count()
            .unwrap(),
        0
    );
    assert_eq!(pool_for(None).provider_account_count().unwrap(), 0);
}

#[test]
fn audit_policy_canonicalizes_gemini_antigravity_and_hardens_custom_and_cli_flags() {
    // Pairing gemini and antigravity cannot bypass require_different_provider.
    assert!(resolve_audit_policy("gemini", None, "antigravity", None, true).is_err());
    assert!(resolve_audit_policy("antigravity", None, "gemini", None, true).is_err());

    // Unverified custom providers always record same_account = true.
    let (rev, same) = resolve_audit_policy("claude", Some("a"), "custom", None, false).unwrap();
    assert_eq!(rev, "cross-audit:custom/default");
    assert!(same);

    // Empty executor + non-empty executor_cmd resolves to "custom", or "claude" when argv[0] is claude.
    let custom_only = CrossAuditConfig {
        executor_cmd: "/usr/local/bin/my-audit {prompt}".into(),
        ..Default::default()
    };
    assert_eq!(configured_audit_provider("claude", &custom_only), "custom");

    let sneaky_claude = CrossAuditConfig {
        executor: "custom".into(),
        executor_cmd: "/usr/local/bin/claude -p {prompt}".into(),
        ..Default::default()
    };
    assert_eq!(
        configured_audit_provider("claude", &sneaky_claude),
        "claude"
    );

    // build_audit_command accepts antigravity alias and rejects flag/whitespace/control injection.
    let ag = build_audit_command(&CrossAuditConfig {
        executor: "antigravity".into(),
        model: "gemini-2.5-pro".into(),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(ag, vec!["gemini", "-m", "gemini-2.5-pro", "-p", "{prompt}"]);

    for bad_model in ["--exec=evil", "gpt 4", "bad\nmodel"] {
        assert!(
            build_audit_command(&CrossAuditConfig {
                executor: "codex".into(),
                model: bad_model.into(),
                ..Default::default()
            })
            .is_err()
        );
    }
    for bad_effort in ["-x", "high low", "hi\tgh"] {
        assert!(
            build_audit_command(&CrossAuditConfig {
                executor: "codex".into(),
                effort: bad_effort.into(),
                ..Default::default()
            })
            .is_err()
        );
    }
}
