#![allow(clippy::unwrap_used, clippy::expect_used)]
//! W4c: `[accounts.billing]` tier-1 parsing, tier-3 ignore, and Claude-only API matching.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use aaf_domain::account::{AccountProfile, AccountState, ProviderKind};
use aaf_domain::config::{AccountsConfig, Billing};
use aaf_engine::accounts::discovery::{account_id, api_billed_account_ids, api_billed_dirs};
use aaf_engine::config::{ConfigInputs, ConfigOverlay, LOCAL_CONFIG_REL, load, load_global};
use aaf_engine::telemetry_cost::{CostRates, cost_policy_for};

fn make_account(provider: ProviderKind, dir: &Path) -> AccountState {
    let canonical = fs::canonicalize(dir).unwrap();
    AccountState {
        profile: AccountProfile {
            id: account_id(provider, &canonical),
            provider,
            config_dir: canonical.to_string_lossy().into_owned(),
            name: "acct".to_string(),
        },
        cooldown_until: None,
    }
}

#[test]
fn parses_tier1_billing_and_ignores_tier3_accounts_section() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&cfg_dir).unwrap();
    fs::create_dir_all(repo.join(".autofactory")).unwrap();

    fs::write(
        cfg_dir.join("config.toml"),
        r#"
[accounts.billing]
"~/.claude-api" = "api"
"~/.claude-max" = "subscription"
"#,
    )
    .unwrap();

    // `load_global` reads tier 1 directly without needing a git repo.
    let global = load_global(&cfg_dir).unwrap();
    assert_eq!(
        global.accounts.billing.get("~/.claude-api"),
        Some(&Billing::Api)
    );
    assert_eq!(
        global.accounts.billing.get("~/.claude-max"),
        Some(&Billing::Subscription)
    );

    // Tier 3 `[accounts]` (including billing) is ignored with a warning.
    fs::write(
        repo.join(LOCAL_CONFIG_REL),
        r#"
[accounts.billing]
"~/.claude-max" = "api"
"#,
    )
    .unwrap();
    let no_env = |_: &str| None;
    let eff = load(&ConfigInputs {
        global_config: Some(cfg_dir.join("config.toml")),
        repo_root: repo,
        env: &no_env,
        cli: ConfigOverlay::default(),
    })
    .unwrap();
    assert_eq!(
        eff.global.accounts.billing.get("~/.claude-max"),
        Some(&Billing::Subscription)
    );
    assert!(
        eff.warnings
            .iter()
            .any(|w| w.contains("[accounts] ignored")),
        "expected tier-3 [accounts] warning, got: {:?}",
        eff.warnings
    );

    // Missing config.toml returns default (empty billing map).
    let missing_dir = tmp.path().join("no-such-cfg");
    let def = load_global(&missing_dir).unwrap();
    assert!(def.accounts.billing.is_empty());
}

#[test]
fn api_billed_dirs_skips_missing_and_non_api_and_ids_only_match_claude() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let claude_api = home.join(".claude-api");
    let claude_sub = home.join(".claude-sub");
    let codex_dir = home.join(".codex");
    fs::create_dir_all(&claude_api).unwrap();
    fs::create_dir_all(&claude_sub).unwrap();
    fs::create_dir_all(&codex_dir).unwrap();

    let mut billing = BTreeMap::new();
    billing.insert("~/.claude-api".to_string(), Billing::Api);
    billing.insert("~/.claude-sub".to_string(), Billing::Subscription);
    billing.insert("~/.claude-missing".to_string(), Billing::Api);
    billing.insert("~/.codex".to_string(), Billing::Api);

    let cfg = AccountsConfig {
        billing,
        ..AccountsConfig::default()
    };

    let dirs = api_billed_dirs(&cfg, home);
    let canon_claude_api = fs::canonicalize(&claude_api).unwrap();
    let canon_codex = fs::canonicalize(&codex_dir).unwrap();
    assert!(dirs.contains(&canon_claude_api));
    assert!(dirs.contains(&canon_codex));
    assert_eq!(
        dirs.len(),
        2,
        "missing and subscription dirs must be skipped"
    );

    let acct_claude_api = make_account(ProviderKind::ClaudeCode, &claude_api);
    let acct_claude_sub = make_account(ProviderKind::ClaudeCode, &claude_sub);
    let acct_codex = make_account(ProviderKind::Codex, &codex_dir);
    let accounts = [acct_claude_api.clone(), acct_claude_sub, acct_codex];

    let ids = api_billed_account_ids(&dirs, &accounts);
    assert_eq!(ids.len(), 1);
    assert!(ids.contains(&acct_claude_api.profile.id));

    let policy = cost_policy_for(&cfg, home, &accounts, CostRates::default());
    assert_eq!(policy.api_accounts, ids);
}
