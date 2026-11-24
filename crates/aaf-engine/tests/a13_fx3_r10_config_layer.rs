#![allow(clippy::unwrap_used, clippy::expect_used)]
//! A13 R10-L4-1: `config::load_over_repo` runs the tier-1 / tier-3 / env /
//! CLI layering over a tier-2 config the caller read elsewhere (`aaf merge`
//! reads it from the base commit) and never reads the working-tree tier 2.

use std::fs;

use aaf_domain::config::{OracleMode, RepoConfig};
use aaf_engine::config::{self, ConfigInputs, ConfigOverlay, LOCAL_CONFIG_REL};
use tempfile::TempDir;

#[test]
fn load_over_repo_layers_env_and_local_over_the_given_tier_two_only() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join(".autofactory")).unwrap();
    // The working-tree tier-2 file is NOT read: the caller supplies tier 2.
    fs::write(
        root.join(".autofactory/config.toml"),
        "[gate]\nhead_lock_globs = [\"from-working-tree/**\"]\n",
    )
    .unwrap();
    fs::write(
        root.join(LOCAL_CONFIG_REL),
        "[gate]\nhead_lock_globs = [\"local/**\"]\ndocs_fast_path = true\n",
    )
    .unwrap();
    let global_dir = root.join("global");
    fs::create_dir_all(&global_dir).unwrap();
    fs::write(
        global_dir.join("config.toml"),
        "[cross_audit]\nrequire_different_provider = true\n",
    )
    .unwrap();

    let mut base = RepoConfig::default();
    base.gate.head_lock_globs = vec!["base/**".to_string()];
    base.gate.docs_fast_path = false;
    base.cross_audit.require_different_provider = false;
    let env = |key: &str| (key == "AAF_RED_GREEN_ORACLE").then(|| "block".to_string());
    let cfg = config::load_over_repo(
        &ConfigInputs {
            global_config: Some(global_dir.join("config.toml")),
            repo_root: root.to_path_buf(),
            env: &env,
            cli: ConfigOverlay::default(),
        },
        base,
        "base:.autofactory/config.toml",
    )
    .unwrap();

    assert_eq!(cfg.repo.gate.head_lock_globs, ["base/**", "local/**"]);
    assert_eq!(cfg.repo.gate.red_green_oracle, OracleMode::Block);
    assert!(!cfg.repo.gate.docs_fast_path, "tighten-only: stays off");
    assert!(cfg.repo.cross_audit.require_different_provider);
    assert!(
        cfg.sources
            .iter()
            .any(|s| s == "base:.autofactory/config.toml"),
        "{:?}",
        cfg.sources
    );
    assert!(
        !cfg.sources.iter().any(|s| s.ends_with("/config.toml")
            && s.contains(".autofactory")
            && !s.starts_with("base:")),
        "{:?}",
        cfg.sources
    );
}
