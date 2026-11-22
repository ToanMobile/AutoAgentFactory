#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `gate.*` may only be tightened by the local file, the environment and the
//! command line: a loosening value is ignored with a warning, a tightening
//! one is applied.

use std::fs;
use std::path::Path;

use aaf_domain::config::StoreMode;
use aaf_engine::config::{
    ConfigInputs, ConfigOverlay, EffectiveConfig, LOCAL_CONFIG_REL, REPO_CONFIG_REL, load,
};
use tempfile::TempDir;

/// Where a tier-3-or-higher value comes from.
#[derive(Debug, Clone, Copy)]
enum Via {
    Local,
    Env,
    Cli,
}

const ALL: [Via; 3] = [Via::Local, Via::Env, Via::Cli];

fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn run(
    tier2: &str,
    local: Option<&str>,
    env: &[(&str, &str)],
    cli: ConfigOverlay,
) -> EffectiveConfig {
    let dir = TempDir::new().unwrap();
    write(&dir.path().join(REPO_CONFIG_REL), tier2);
    if let Some(body) = local {
        write(&dir.path().join(LOCAL_CONFIG_REL), body);
    }
    let lookup = |key: &str| {
        env.iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| (*value).to_string())
    };
    load(&ConfigInputs {
        global_config: None,
        repo_root: dir.path().to_path_buf(),
        env: &lookup,
        cli,
    })
    .unwrap()
}

fn mode_str(mode: StoreMode) -> &'static str {
    match mode {
        StoreMode::Off => "off",
        StoreMode::Auto => "auto",
        StoreMode::Always => "always",
    }
}

fn with_store(tier2: StoreMode, via: Via, wanted: StoreMode) -> EffectiveConfig {
    let tier2 = format!("[gate]\nstore_compliance = \"{}\"\n", mode_str(tier2));
    match via {
        Via::Local => run(
            &tier2,
            Some(&format!(
                "[gate]\nstore_compliance = \"{}\"\n",
                mode_str(wanted)
            )),
            &[],
            ConfigOverlay::default(),
        ),
        Via::Env => run(
            &tier2,
            None,
            &[("AAF_STORE_COMPLIANCE", mode_str(wanted))],
            ConfigOverlay::default(),
        ),
        Via::Cli => {
            let mut cli = ConfigOverlay::default();
            cli.gate.store_compliance = Some(wanted);
            run(&tier2, None, &[], cli)
        }
    }
}

fn with_sdk(tier2: u32, via: Via, wanted: u32) -> EffectiveConfig {
    let tier2 = format!("[gate]\nmin_android_target_sdk = {tier2}\n");
    match via {
        Via::Local => run(
            &tier2,
            Some(&format!("[gate]\nmin_android_target_sdk = {wanted}\n")),
            &[],
            ConfigOverlay::default(),
        ),
        Via::Env => run(
            &tier2,
            None,
            &[("AAF_MIN_ANDROID_TARGET_SDK", &wanted.to_string())],
            ConfigOverlay::default(),
        ),
        Via::Cli => {
            let mut cli = ConfigOverlay::default();
            cli.gate.min_android_target_sdk = Some(wanted);
            run(&tier2, None, &[], cli)
        }
    }
}

#[test]
fn store_compliance_can_be_raised_by_every_source() {
    use StoreMode::{Always, Auto, Off};
    for via in ALL {
        for (from, to) in [(Off, Auto), (Off, Always), (Auto, Always)] {
            let cfg = with_store(from, via, to);
            assert_eq!(
                cfg.repo.gate.store_compliance, to,
                "{via:?} {from:?}->{to:?}"
            );
            assert!(cfg.warnings.is_empty(), "{via:?}: {:?}", cfg.warnings);
        }
    }
}

#[test]
fn store_compliance_cannot_be_lowered_by_any_source() {
    use StoreMode::{Always, Auto, Off};
    for via in ALL {
        for (from, to) in [(Auto, Off), (Always, Off), (Always, Auto)] {
            let cfg = with_store(from, via, to);
            assert_eq!(
                cfg.repo.gate.store_compliance, from,
                "{via:?} {from:?}->{to:?}"
            );
            assert_eq!(cfg.warnings.len(), 1, "{via:?}: {:?}", cfg.warnings);
            assert!(cfg.warnings[0].contains("gate.store_compliance"));
        }
    }
}

#[test]
fn store_compliance_same_value_is_silent() {
    for via in ALL {
        let cfg = with_store(StoreMode::Auto, via, StoreMode::Auto);
        assert_eq!(cfg.repo.gate.store_compliance, StoreMode::Auto);
        assert!(cfg.warnings.is_empty());
    }
}

#[test]
fn min_android_target_sdk_takes_the_max() {
    for via in ALL {
        let raised = with_sdk(36, via, 37);
        assert_eq!(raised.repo.gate.min_android_target_sdk, 37, "{via:?}");
        assert!(raised.warnings.is_empty(), "{via:?}: {:?}", raised.warnings);

        let lowered = with_sdk(36, via, 34);
        assert_eq!(lowered.repo.gate.min_android_target_sdk, 36, "{via:?}");
        assert_eq!(lowered.warnings.len(), 1, "{via:?}: {:?}", lowered.warnings);
        assert!(lowered.warnings[0].contains("gate.min_android_target_sdk"));
    }
}

#[test]
fn a_higher_tier_cannot_undo_what_a_lower_override_tightened() {
    let mut cli = ConfigOverlay::default();
    cli.gate.min_android_target_sdk = Some(1);
    let cfg = run(
        "[gate]\nstore_compliance = \"off\"\nmin_android_target_sdk = 30\n",
        Some("[gate]\nstore_compliance = \"always\"\nmin_android_target_sdk = 40\n"),
        &[("AAF_STORE_COMPLIANCE", "off")],
        cli,
    );
    assert_eq!(cfg.repo.gate.store_compliance, StoreMode::Always);
    assert_eq!(cfg.repo.gate.min_android_target_sdk, 40);
    assert_eq!(cfg.warnings.len(), 2, "{:?}", cfg.warnings);
}

#[test]
fn head_lock_globs_are_a_union_and_tier_two_entries_survive() {
    let tier2 = "[gate]\nhead_lock_globs = [\"a/**\", \"b/**\"]\n";

    let added = run(
        tier2,
        Some("[gate]\nhead_lock_globs = [\"c/**\", \"a/**\"]\n"),
        &[],
        ConfigOverlay::default(),
    );
    assert_eq!(
        added.repo.gate.head_lock_globs,
        vec!["a/**", "b/**", "c/**"]
    );

    // An empty or shorter list is the only way to "ask" for removal; nothing is removed.
    let emptied = run(
        tier2,
        Some("[gate]\nhead_lock_globs = []\n"),
        &[],
        ConfigOverlay::default(),
    );
    assert_eq!(emptied.repo.gate.head_lock_globs, vec!["a/**", "b/**"]);

    let mut cli = ConfigOverlay::default();
    cli.gate.head_lock_globs = Some(vec!["d/**".to_string()]);
    let both = run(
        tier2,
        Some("[gate]\nhead_lock_globs = [\"c/**\"]\n"),
        &[],
        cli,
    );
    assert_eq!(
        both.repo.gate.head_lock_globs,
        vec!["a/**", "b/**", "c/**", "d/**"]
    );
}

#[test]
fn default_head_lock_globs_survive_a_local_list() {
    let cfg = run(
        "",
        Some("[gate]\nhead_lock_globs = [\"secrets/**\"]\n"),
        &[],
        ConfigOverlay::default(),
    );
    let defaults = aaf_domain::config::GateConfig::default().head_lock_globs;
    assert_eq!(cfg.repo.gate.head_lock_globs.len(), defaults.len() + 1);
    assert!(
        defaults
            .iter()
            .all(|g| cfg.repo.gate.head_lock_globs.contains(g))
    );
    assert!(
        cfg.repo
            .gate
            .head_lock_globs
            .contains(&"secrets/**".to_string())
    );
}

#[test]
fn command_timeout_may_move_freely_in_both_directions() {
    let tier2 = "[gate]\ncommand_timeout_secs = 600\n";
    for wanted in [60_u64, 9000] {
        let local = format!("[gate]\ncommand_timeout_secs = {wanted}\n");
        let cfg = run(tier2, Some(&local), &[], ConfigOverlay::default());
        assert_eq!(cfg.repo.gate.command_timeout_secs, wanted);
        assert!(cfg.warnings.is_empty());

        let mut cli = ConfigOverlay::default();
        cli.gate.command_timeout_secs = Some(wanted);
        let cfg = run(tier2, None, &[], cli);
        assert_eq!(cfg.repo.gate.command_timeout_secs, wanted);
    }
}

#[test]
fn base_ref_comes_from_tier_two_only() {
    let tier2 = "[gate]\nbase_ref = \"origin/main\"\n";
    let cfg = run(
        tier2,
        Some("[gate]\nbase_ref = \"HEAD\"\n"),
        &[],
        ConfigOverlay::default(),
    );
    assert_eq!(cfg.repo.gate.base_ref, "origin/main");
    assert_eq!(cfg.warnings.len(), 1, "{:?}", cfg.warnings);
    assert!(cfg.warnings[0].contains("gate.base_ref"));

    let mut cli = ConfigOverlay::default();
    cli.gate.base_ref = Some("HEAD".to_string());
    let cfg = run(tier2, None, &[], cli);
    assert_eq!(cfg.repo.gate.base_ref, "origin/main");
    assert_eq!(cfg.warnings.len(), 1);

    // Repeating the tier 2 value is not an override attempt.
    let same = run(
        tier2,
        Some("[gate]\nbase_ref = \"origin/main\"\n"),
        &[],
        ConfigOverlay::default(),
    );
    assert!(same.warnings.is_empty());
}

#[test]
fn tier_two_itself_may_set_any_gate_value() {
    let cfg = run(
        "[gate]\nstore_compliance = \"off\"\nmin_android_target_sdk = 21\nhead_lock_globs = []\n",
        None,
        &[],
        ConfigOverlay::default(),
    );
    assert_eq!(cfg.repo.gate.store_compliance, StoreMode::Off);
    assert_eq!(cfg.repo.gate.min_android_target_sdk, 21);
    assert!(cfg.repo.gate.head_lock_globs.is_empty());
    assert!(cfg.warnings.is_empty());
}
