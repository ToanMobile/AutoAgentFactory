#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Phase 2 v3.2 tests: 3-tier monotonic `[cross_audit]` merge, render round-trip,
//! and `gate::config_hash` policy sensitivity.

use std::fs;

use aaf_domain::config::{AuditScope, CommandsConfig, RepoConfig};
use aaf_engine::config::{
    ConfigInputs, ConfigOverlay, Detected, EffectiveConfig, LOCAL_CONFIG_REL, REPO_CONFIG_REL,
    load, render_repo_config,
};
use aaf_engine::gate;
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
    load(&ConfigInputs {
        global_config: Some(global_path),
        repo_root: repo,
        env: &no_env,
        cli: ConfigOverlay::default(),
    })
    .unwrap()
}

#[test]
fn cross_audit_three_tier_monotonic_merge_and_clamping() {
    // Unspecified tier-1 fields (None) never clobber explicit tier-2 values.
    let t2 = load_tiers(
        Some("[cross_audit]\nexecutor = \"gemini\"\n"),
        Some(
            "[cross_audit]\naudit_scope = \"large\"\nscore_gate = true\nmin_average_score = 88\nmin_criterion_score = 82\n",
        ),
        None,
    );
    assert_eq!(t2.repo.cross_audit.audit_scope, AuditScope::Large);
    assert!(t2.repo.cross_audit.score_gate);
    assert_eq!(t2.repo.cross_audit.min_average_score, 88);
    assert_eq!(t2.repo.cross_audit.min_criterion_score, 82);
    assert!(t2.warnings.is_empty(), "{:?}", t2.warnings);

    // Higher tier-1 floors override weaker tier-2; > 100 clamps with warning.
    let t1 = load_tiers(
        Some(
            "[cross_audit]\naudit_scope = \"always\"\nscore_gate = true\nmin_average_score = 120\nmin_criterion_score = 92\n",
        ),
        Some(
            "[cross_audit]\naudit_scope = \"head_lock\"\nscore_gate = false\nmin_average_score = 80\nmin_criterion_score = 105\n",
        ),
        None,
    );
    assert_eq!(t1.repo.cross_audit.audit_scope, AuditScope::Always);
    assert!(t1.repo.cross_audit.score_gate);
    assert_eq!(t1.repo.cross_audit.min_average_score, 100);
    assert_eq!(t1.repo.cross_audit.min_criterion_score, 100);
    assert!(t1.warnings.iter().any(|w| w.contains("clamped to 100")));

    // Tier-3 tightens cleanly; attempted weakening is ignored with warnings.
    let tighten = load_tiers(
        None,
        Some(
            "[cross_audit]\naudit_scope = \"head_lock\"\nscore_gate = false\nmin_average_score = 90\nmin_criterion_score = 85\n",
        ),
        Some(
            "[cross_audit]\naudit_scope = \"large\"\nscore_gate = true\nmin_average_score = 96\nmin_criterion_score = 92\n",
        ),
    );
    assert_eq!(tighten.repo.cross_audit.audit_scope, AuditScope::Large);
    assert!(tighten.repo.cross_audit.score_gate);
    assert_eq!(tighten.repo.cross_audit.min_average_score, 96);
    assert_eq!(tighten.repo.cross_audit.min_criterion_score, 92);
    assert!(tighten.warnings.is_empty(), "{:?}", tighten.warnings);

    let weaken = load_tiers(
        None,
        Some(
            "[cross_audit]\naudit_scope = \"always\"\nscore_gate = true\nmin_average_score = 95\nmin_criterion_score = 90\n",
        ),
        Some(
            "[cross_audit]\naudit_scope = \"large\"\nscore_gate = false\nmin_average_score = 80\nmin_criterion_score = 70\n",
        ),
    );
    assert_eq!(weaken.repo.cross_audit.audit_scope, AuditScope::Always);
    assert!(weaken.repo.cross_audit.score_gate);
    assert_eq!(weaken.repo.cross_audit.min_average_score, 95);
    assert_eq!(weaken.repo.cross_audit.min_criterion_score, 90);
    for key in [
        "cross_audit.audit_scope",
        "cross_audit.score_gate",
        "cross_audit.min_average_score",
        "cross_audit.min_criterion_score",
    ] {
        assert!(
            weaken.warnings.iter().any(|w| w.contains(key)),
            "{key}: {:?}",
            weaken.warnings
        );
    }
}

#[test]
fn render_repo_config_round_trips_new_cross_audit_fields() {
    let detected = Detected {
        profile: "rust".to_string(),
        commands: CommandsConfig::default(),
        evidence: vec!["Cargo.toml".to_string()],
    };
    let text = render_repo_config(&detected, "demo");
    for needle in [
        "audit_scope = \"head_lock\"",
        "score_gate = false",
        "min_average_score = 95",
        "min_criterion_score = 90",
    ] {
        assert!(text.contains(needle), "missing {needle} in:\n{text}");
    }
    let parsed: RepoConfig = toml::from_str(&text).unwrap();
    let mut expected = RepoConfig::default();
    expected.project.name = "demo".to_string();
    expected.project.profile = "rust".to_string();
    assert_eq!(parsed, expected);
}

#[test]
fn gate_config_hash_tracks_active_cross_audit_policy_and_preserves_default() {
    let base = RepoConfig::default();
    let default_hash = gate::config_hash(&base);

    let mut inactive = base.clone();
    inactive.cross_audit.min_average_score = 80;
    inactive.cross_audit.min_criterion_score = 75;
    assert_eq!(gate::config_hash(&inactive), default_hash);

    let mut large = base.clone();
    large.cross_audit.audit_scope = AuditScope::Large;
    let large_hash = gate::config_hash(&large);
    assert_ne!(large_hash, default_hash);

    let mut always = base.clone();
    always.cross_audit.audit_scope = AuditScope::Always;
    assert_ne!(gate::config_hash(&always), large_hash);

    let mut scored = base.clone();
    scored.cross_audit.score_gate = true;
    let scored_hash = gate::config_hash(&scored);
    assert_ne!(scored_hash, default_hash);

    let mut stricter = scored.clone();
    stricter.cross_audit.min_average_score = 98;
    assert_ne!(gate::config_hash(&stricter), scored_hash);
}
