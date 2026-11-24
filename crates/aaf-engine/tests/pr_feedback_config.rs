#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Config tests for `pipeline.max_pr_rework_rounds` (B1/D7):
//! - Tier-2 clamping to `MAX_PR_REWORK_ROUNDS_CAP` (3) with warning;
//! - Tier-3 local override ignored with `TRUSTED_ONLY` warning;
//! - Tier-1 inheritance when Tier-2 omits the key;
//! - `render_repo_config` round-trip.

use std::fs;

use aaf_domain::config::{CommandsConfig, RepoConfig};
use aaf_domain::pipeline::{DEFAULT_MAX_PR_REWORK_ROUNDS, MAX_PR_REWORK_ROUNDS_CAP};
use aaf_engine::config::{
    ConfigInputs, ConfigOverlay, Detected, EffectiveConfig, LOCAL_CONFIG_REL, REPO_CONFIG_REL,
    load, render_repo_config,
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

fn has_warn(cfg: &EffectiveConfig, needle: &str) -> bool {
    cfg.warnings.iter().any(|w| w.contains(needle))
}

#[test]
fn tier2_clamps_max_pr_rework_rounds_to_cap_with_warning() {
    let cfg = load_tiers(None, Some("[pipeline]\nmax_pr_rework_rounds = 9"), None);
    assert_eq!(
        cfg.repo.pipeline.max_pr_rework_rounds,
        MAX_PR_REWORK_ROUNDS_CAP
    );
    assert_eq!(cfg.repo.pipeline.max_pr_rework_rounds, 3);
    assert!(
        has_warn(&cfg, "pipeline.max_pr_rework_rounds = 9 clamped to 3"),
        "warnings: {:?}",
        cfg.warnings
    );
}

#[test]
fn tier3_cannot_set_max_pr_rework_rounds() {
    let cfg = load_tiers(
        None,
        Some("[pipeline]\nmax_pr_rework_rounds = 2"),
        Some("[pipeline]\nmax_pr_rework_rounds = 1"),
    );
    assert_eq!(cfg.repo.pipeline.max_pr_rework_rounds, 2);
    assert!(
        has_warn(&cfg, "pipeline.max_pr_rework_rounds = 1 ignored"),
        "warnings: {:?}",
        cfg.warnings
    );
}

#[test]
fn tier1_fills_default_when_tier2_omits_and_allows_zero() {
    let from_t1 = load_tiers(Some("[pipeline]\nmax_pr_rework_rounds = 1"), None, None);
    assert_eq!(from_t1.repo.pipeline.max_pr_rework_rounds, 1);

    let t2_zero = load_tiers(
        Some("[pipeline]\nmax_pr_rework_rounds = 3"),
        Some("[pipeline]\nmax_pr_rework_rounds = 0"),
        None,
    );
    assert_eq!(t2_zero.repo.pipeline.max_pr_rework_rounds, 0);
    assert!(t2_zero.warnings.is_empty(), "{:?}", t2_zero.warnings);

    let defaults = load_tiers(None, None, None);
    assert_eq!(
        defaults.repo.pipeline.max_pr_rework_rounds,
        DEFAULT_MAX_PR_REWORK_ROUNDS
    );
}

#[test]
fn render_repo_config_round_trips_max_pr_rework_rounds() {
    let detected = Detected {
        profile: "rust".to_string(),
        commands: CommandsConfig::default(),
        evidence: vec!["Cargo.toml".to_string()],
    };
    let text = render_repo_config(&detected, "demo");
    assert!(
        text.contains("max_pr_rework_rounds = 2"),
        "missing key in rendered config:\n{text}"
    );
    let parsed: RepoConfig = toml::from_str(&text).unwrap();
    let mut want = RepoConfig::default();
    want.project.name = "demo".to_string();
    want.project.profile = "rust".to_string();
    assert_eq!(parsed, want);
}
