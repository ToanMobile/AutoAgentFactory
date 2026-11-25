#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Config tests for `[unblocker.cost_guard]` (W4a):
//! - Tier-2 may only tighten (`min`) positive Tier-1 caps/factors, never loosen;
//! - Tier-1 off (`0` or omitted) allows Tier-2 to enable;
//! - Tier-3 (`config.local.toml`) `[unblocker]` is ignored with warning;
//! - `spike_factor` defaults to `DEFAULT_SPIKE_FACTOR` (4) when cap > 0 and
//!   neither tier explicitly sets `spike_factor`;
//! - `tighten_trusted_base` / `load_over_repo` (`all_explicit()`) tighten-only;
//! - `render_repo_config` round-trip.

use std::fs;

use aaf_domain::config::{CommandsConfig, GlobalConfig, RepoConfig};
use aaf_domain::pipeline::{CostGuardConfig, DEFAULT_SPIKE_FACTOR};
use aaf_engine::config::{
    ConfigInputs, ConfigOverlay, Detected, EffectiveConfig, LOCAL_CONFIG_REL, REPO_CONFIG_REL,
    load, load_over_repo, render_repo_config, tighten_trusted_base,
};
use tempfile::TempDir;

fn load_tiers(global: Option<&str>, tier2: Option<&str>, local: Option<&str>) -> EffectiveConfig {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    let global_path = dir.path().join("cfg/config.toml");
    fs::create_dir_all(&repo).unwrap();
    for (p, body) in [
        (&global_path, global),
        (&repo.join(REPO_CONFIG_REL), tier2),
        (&repo.join(LOCAL_CONFIG_REL), local),
    ] {
        if let Some(body) = body {
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        }
    }
    let no_env = |_: &str| None;
    let inputs = ConfigInputs {
        global_config: Some(global_path),
        repo_root: repo,
        env: &no_env,
        cli: ConfigOverlay::default(),
    };
    load(&inputs).unwrap()
}

#[test]
fn tier2_only_tightens_positive_tier1_cap() {
    // Tier 1 cap 1000 + repo 5000 -> 1000 (cannot loosen).
    let cfg = load_tiers(
        Some("[unblocker.cost_guard]\ndaily_token_cap = 1000\n"),
        Some("[unblocker.cost_guard]\ndaily_token_cap = 5000\n"),
        None,
    );
    assert_eq!(cfg.repo.unblocker.cost_guard.daily_token_cap, 1000);
    assert_eq!(
        cfg.repo.unblocker.cost_guard.spike_factor,
        DEFAULT_SPIKE_FACTOR
    );

    // Tier 1 cap 1000 + repo 500 -> 500 (may tighten).
    let cfg = load_tiers(
        Some("[unblocker.cost_guard]\ndaily_token_cap = 1000\n"),
        Some("[unblocker.cost_guard]\ndaily_token_cap = 500\n"),
        None,
    );
    assert_eq!(cfg.repo.unblocker.cost_guard.daily_token_cap, 500);

    // Tier 1 cap 1000 + repo 0 (attempt to turn off) -> 1000.
    let cfg = load_tiers(
        Some("[unblocker.cost_guard]\ndaily_token_cap = 1000\n"),
        Some("[unblocker.cost_guard]\ndaily_token_cap = 0\n"),
        None,
    );
    assert_eq!(cfg.repo.unblocker.cost_guard.daily_token_cap, 1000);

    // Tier 1 cap 0 + repo 500 -> 500 (repo may enable when Tier 1 is off).
    let cfg = load_tiers(
        Some("[unblocker.cost_guard]\ndaily_token_cap = 0\n"),
        Some("[unblocker.cost_guard]\ndaily_token_cap = 500\n"),
        None,
    );
    assert_eq!(cfg.repo.unblocker.cost_guard.daily_token_cap, 500);
    assert_eq!(
        cfg.repo.unblocker.cost_guard.spike_factor,
        DEFAULT_SPIKE_FACTOR
    );
}

#[test]
fn tier3_cost_guard_is_ignored_with_warning() {
    let cfg = load_tiers(
        Some("[unblocker.cost_guard]\ndaily_token_cap = 1000\n"),
        None,
        Some("[unblocker.cost_guard]\ndaily_token_cap = 999999\nspike_factor = 0\n"),
    );
    assert_eq!(cfg.repo.unblocker.cost_guard.daily_token_cap, 1000);
    assert_eq!(
        cfg.repo.unblocker.cost_guard.spike_factor,
        DEFAULT_SPIKE_FACTOR
    );
    assert!(
        cfg.warnings
            .iter()
            .any(|w| w.contains("[unblocker] ignored")),
        "warnings: {:?}",
        cfg.warnings
    );
}

#[test]
fn spike_factor_defaults_to_four_and_respects_explicit_zero_and_tightening() {
    // Default off when no cap is configured.
    let off = load_tiers(None, None, None);
    assert_eq!(off.repo.unblocker.cost_guard, CostGuardConfig::default());

    // Cap > 0 with neither tier specifying spike_factor -> DEFAULT_SPIKE_FACTOR (4).
    let defaulted = load_tiers(
        Some("[unblocker.cost_guard]\ndaily_token_cap = 2000\n"),
        None,
        None,
    );
    assert_eq!(defaulted.repo.unblocker.cost_guard.spike_factor, 4);

    // Explicit Tier 1 spike_factor = 0 disables spike detection for a Tier 1 cap.
    let t1_off = load_tiers(
        Some("[unblocker.cost_guard]\ndaily_token_cap = 2000\nspike_factor = 0\n"),
        None,
        None,
    );
    assert_eq!(t1_off.repo.unblocker.cost_guard.spike_factor, 0);

    // Tier 2 CANNOT disable or loosen default spike when Tier 1 set daily_token_cap.
    let t2_cannot_disable_t1_default = load_tiers(
        Some("[unblocker.cost_guard]\ndaily_token_cap = 2000\n"),
        Some("[unblocker.cost_guard]\nspike_factor = 0\n"),
        None,
    );
    assert_eq!(
        t2_cannot_disable_t1_default
            .repo
            .unblocker
            .cost_guard
            .spike_factor,
        DEFAULT_SPIKE_FACTOR
    );

    let t2_cannot_loosen_t1_default = load_tiers(
        Some("[unblocker.cost_guard]\ndaily_token_cap = 2000\n"),
        Some("[unblocker.cost_guard]\nspike_factor = 10\n"),
        None,
    );
    assert_eq!(
        t2_cannot_loosen_t1_default
            .repo
            .unblocker
            .cost_guard
            .spike_factor,
        DEFAULT_SPIKE_FACTOR
    );

    let t2_tightens_t1_default = load_tiers(
        Some("[unblocker.cost_guard]\ndaily_token_cap = 2000\n"),
        Some("[unblocker.cost_guard]\nspike_factor = 2\n"),
        None,
    );
    assert_eq!(
        t2_tightens_t1_default
            .repo
            .unblocker
            .cost_guard
            .spike_factor,
        2
    );

    // Tier 2 MAY disable spike only for a cap it sets itself (Tier 1 has no cap).
    let t2_own_cap_off = load_tiers(
        None,
        Some("[unblocker.cost_guard]\ndaily_token_cap = 2000\nspike_factor = 0\n"),
        None,
    );
    assert_eq!(
        t2_own_cap_off.repo.unblocker.cost_guard.daily_token_cap,
        2000
    );
    assert_eq!(t2_own_cap_off.repo.unblocker.cost_guard.spike_factor, 0);

    let t2_own_cap_off_when_t1_zero = load_tiers(
        Some("[unblocker.cost_guard]\ndaily_token_cap = 0\n"),
        Some("[unblocker.cost_guard]\ndaily_token_cap = 2000\nspike_factor = 0\n"),
        None,
    );
    assert_eq!(
        t2_own_cap_off_when_t1_zero
            .repo
            .unblocker
            .cost_guard
            .spike_factor,
        0
    );

    // Positive Tier 1 spike_factor cannot be disabled or loosened by Tier 2, only tightened.
    let t2_cannot_disable = load_tiers(
        Some("[unblocker.cost_guard]\ndaily_token_cap = 2000\nspike_factor = 6\n"),
        Some("[unblocker.cost_guard]\nspike_factor = 0\n"),
        None,
    );
    assert_eq!(t2_cannot_disable.repo.unblocker.cost_guard.spike_factor, 6);

    let t2_tightens = load_tiers(
        Some("[unblocker.cost_guard]\ndaily_token_cap = 2000\nspike_factor = 6\n"),
        Some("[unblocker.cost_guard]\nspike_factor = 3\n"),
        None,
    );
    assert_eq!(t2_tightens.repo.unblocker.cost_guard.spike_factor, 3);
}

#[test]
fn tighten_trusted_base_and_load_over_repo_enforce_tier1_cap() {
    let global: GlobalConfig =
        toml::from_str("[unblocker.cost_guard]\ndaily_token_cap = 1000\n").unwrap();
    let mut repo = RepoConfig::default();
    repo.unblocker.cost_guard.daily_token_cap = 5000;
    let no_env = |_: &str| None;

    let tightened = tighten_trusted_base(repo.clone(), &global, None, &no_env).unwrap();
    assert_eq!(tightened.unblocker.cost_guard.daily_token_cap, 1000);
    assert_eq!(
        tightened.unblocker.cost_guard.spike_factor,
        DEFAULT_SPIKE_FACTOR
    );

    let dir = TempDir::new().unwrap();
    let global_path = dir.path().join("cfg/config.toml");
    fs::create_dir_all(global_path.parent().unwrap()).unwrap();
    fs::write(
        &global_path,
        "[unblocker.cost_guard]\ndaily_token_cap = 1000\n",
    )
    .unwrap();
    let inputs = ConfigInputs {
        global_config: Some(global_path),
        repo_root: dir.path().to_path_buf(),
        env: &no_env,
        cli: ConfigOverlay::default(),
    };
    let loaded = load_over_repo(&inputs, repo, "base:.autofactory/config.toml").unwrap();
    assert_eq!(loaded.repo.unblocker.cost_guard.daily_token_cap, 1000);
    assert_eq!(
        loaded.repo.unblocker.cost_guard.spike_factor,
        DEFAULT_SPIKE_FACTOR
    );
}

#[test]
fn render_repo_config_round_trips_with_cost_guard_comments() {
    let detected = Detected {
        profile: "rust".to_string(),
        commands: CommandsConfig::default(),
        evidence: vec!["Cargo.toml".to_string()],
    };
    let text = render_repo_config(&detected, "demo");
    assert!(
        text.contains("# [unblocker.cost_guard]"),
        "missing cost_guard comment in rendered config:\n{text}"
    );
    let parsed: RepoConfig = toml::from_str(&text).unwrap();
    let mut want = RepoConfig::default();
    want.project.name = "demo".to_string();
    want.project.profile = "rust".to_string();
    assert_eq!(parsed, want);
}
