#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Wave 1 config security, 3-tier anti-weakening merge, `tighten_trusted_base`
//! parity, and `gate::config_hash` Dual-Review tests (PLAN_DUAL_FRONTIER.md §C.2, §F).

use std::fs;

use aaf_domain::config::{AuditScope, GlobalConfig, MAX_AUDIT_REWORKS_CAP, RepoConfig};
use aaf_domain::pipeline::{MAX_PLAN_ROUNDS_CAP, ReviewerConfig, StageModelConfig};
use aaf_engine::config::{
    ConfigError, ConfigInputs, ConfigOverlay, EffectiveConfig, LOCAL_CONFIG_REL, REPO_CONFIG_REL,
    RunOverlay, load, tighten_trusted_base,
};
use aaf_engine::gate;
use tempfile::TempDir;

fn try_load_tiers(
    global: Option<&str>,
    tier2: Option<&str>,
    local: Option<&str>,
    cli: ConfigOverlay,
) -> Result<EffectiveConfig, ConfigError> {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    let global_path = dir.path().join("cfg/config.toml");
    fs::create_dir_all(&repo).unwrap();
    for (path, body) in [
        (&global_path, global),
        (&repo.join(REPO_CONFIG_REL), tier2),
        (&repo.join(LOCAL_CONFIG_REL), local),
    ] {
        if let Some(body) = body {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }
    }
    let no_env = |_: &str| None;
    load(&ConfigInputs {
        global_config: Some(global_path),
        repo_root: repo,
        env: &no_env,
        cli,
    })
}

fn load_tiers(global: Option<&str>, tier2: Option<&str>, local: Option<&str>) -> EffectiveConfig {
    try_load_tiers(global, tier2, local, ConfigOverlay::default()).unwrap()
}

fn has_warn(cfg: &EffectiveConfig, needle: &str) -> bool {
    cfg.warnings.iter().any(|w| w.contains(needle))
}

#[test]
fn tier3_enabling_plan_debate_or_dual_review_same_file_blocks_run_model_and_effort() {
    // Same local file enables plan_debate = true AND tries to downgrade run.model/effort.
    let local = "[pipeline]\nplan_debate = true\nplan_stage = false\n\
                 [run]\nmodel = \"haiku\"\neffort = \"low\"\n";
    let cfg = load_tiers(
        None,
        Some("[run]\nmodel = \"sonnet\"\neffort = \"medium\"\n"),
        Some(local),
    );
    assert!(cfg.repo.pipeline.plan_debate);
    assert!(cfg.repo.pipeline.plan_stage);
    assert_eq!(cfg.repo.run.model, "sonnet");
    assert_eq!(cfg.repo.run.effort, "medium");
    assert!(has_warn(&cfg, "run.model = \"haiku\" ignored"));
    assert!(has_warn(&cfg, "run.effort = \"low\" ignored"));
    assert!(has_warn(&cfg, "pipeline.plan_stage = false ignored"));

    // Same local file enables dual_review = true AND tries to change run.model.
    let dual_local = "[cross_audit]\ndual_review = true\n[run]\nmodel = \"haiku\"\n";
    let cfg2 = load_tiers(None, Some("[run]\nmodel = \"opus\"\n"), Some(dual_local));
    assert!(cfg2.repo.cross_audit.dual_review);
    assert_eq!(cfg2.repo.run.model, "opus");
    assert!(has_warn(&cfg2, "run.model = \"haiku\" ignored"));

    // Explicit CLI flag still applies (OverlayOrigin::CommandLine).
    let cli = ConfigOverlay {
        run: RunOverlay {
            model: Some("opus".to_string()),
            ..RunOverlay::default()
        },
        ..ConfigOverlay::default()
    };
    let cfg3 = try_load_tiers(
        None,
        Some("[pipeline]\nplan_debate = true\n[run]\nmodel = \"sonnet\"\n"),
        None,
        cli,
    )
    .unwrap();
    assert_eq!(cfg3.repo.run.model, "opus");
}

#[test]
fn cross_adjudicate_monotonicity_false_is_stricter_across_all_tiers() {
    // Tier 3 cannot weaken false -> true.
    let blocked = load_tiers(
        None,
        Some("[cross_audit]\ncross_adjudicate = false\n"),
        Some("[cross_audit]\ncross_adjudicate = true\n"),
    );
    assert!(!blocked.repo.cross_audit.cross_adjudicate);
    assert!(has_warn(
        &blocked,
        "cross_audit.cross_adjudicate = true ignored: false is stricter"
    ));

    // Tier 3 MAY tighten true -> false cleanly.
    let tightened = load_tiers(
        None,
        Some("[cross_audit]\ncross_adjudicate = true\n"),
        Some("[cross_audit]\ncross_adjudicate = false\n"),
    );
    assert!(!tightened.repo.cross_audit.cross_adjudicate);
    assert!(tightened.warnings.is_empty(), "{:?}", tightened.warnings);

    // Tier 1 Some(false) forces false over explicit Tier 2 true (with warning).
    let t1_forces = load_tiers(
        Some("[cross_audit]\ncross_adjudicate = false\n"),
        Some("[cross_audit]\ncross_adjudicate = true\n"),
        None,
    );
    assert!(!t1_forces.repo.cross_audit.cross_adjudicate);
    assert!(has_warn(&t1_forces, "cross_audit.cross_adjudicate = true"));

    // Tier 1 Some(true) NEVER weakens explicit Tier 2 false.
    let t2_kept = load_tiers(
        Some("[cross_audit]\ncross_adjudicate = true\n"),
        Some("[cross_audit]\ncross_adjudicate = false\n"),
        None,
    );
    assert!(!t2_kept.repo.cross_audit.cross_adjudicate);
    assert!(t2_kept.warnings.is_empty(), "{:?}", t2_kept.warnings);

    // Tier 1 Some(true) applies when Tier 2 omits the key.
    let t1_default = load_tiers(Some("[cross_audit]\ncross_adjudicate = true\n"), None, None);
    assert!(t1_default.repo.cross_audit.cross_adjudicate);
}

#[test]
fn allow_tier_s_skip_debate_monotonicity_and_pipeline_tier_merges() {
    // Tier 1 Some(false) forces false over Tier 2 true (with warning).
    let forced = load_tiers(
        Some("[pipeline]\nallow_tier_s_skip_debate = false\n"),
        Some("[pipeline]\nallow_tier_s_skip_debate = true\n"),
        None,
    );
    assert!(!forced.repo.pipeline.allow_tier_s_skip_debate);
    assert!(has_warn(
        &forced,
        "pipeline.allow_tier_s_skip_debate = true"
    ));

    // Tier 1 Some(true) does NOT weaken explicit Tier 2 false.
    let t2_explicit = load_tiers(
        Some("[pipeline]\nallow_tier_s_skip_debate = true\n"),
        Some("[pipeline]\nallow_tier_s_skip_debate = false\n"),
        None,
    );
    assert!(!t2_explicit.repo.pipeline.allow_tier_s_skip_debate);

    // Tier 1 Some(true) applies when Tier 2 omitted allow_tier_s_skip_debate.
    let t1_default = load_tiers(
        Some("[pipeline]\nallow_tier_s_skip_debate = true\n"),
        Some("[pipeline]\nplan_debate = true\n"),
        None,
    );
    assert!(t1_default.repo.pipeline.allow_tier_s_skip_debate);
    assert!(t1_default.repo.pipeline.plan_stage);

    // Tier 3 cannot flip false -> true, but CAN tighten true -> false.
    let t3_block = load_tiers(
        None,
        Some("[pipeline]\nallow_tier_s_skip_debate = false\n"),
        Some("[pipeline]\nallow_tier_s_skip_debate = true\n"),
    );
    assert!(!t3_block.repo.pipeline.allow_tier_s_skip_debate);
    assert!(has_warn(
        &t3_block,
        "pipeline.allow_tier_s_skip_debate = true ignored"
    ));

    let t3_tighten = load_tiers(
        None,
        Some("[pipeline]\nallow_tier_s_skip_debate = true\n"),
        Some("[pipeline]\nallow_tier_s_skip_debate = false\n"),
    );
    assert!(!t3_tighten.repo.pipeline.allow_tier_s_skip_debate);
    assert!(t3_tighten.warnings.is_empty(), "{:?}", t3_tighten.warnings);

    // Numeric caps clamp 1..=5 for max_plan_rounds & escalate_implementer_after_reworks,
    // and Tier 3 numeric/stage overrides are ignored with warnings.
    let clamped = load_tiers(
        Some(
            "[pipeline]\nmax_plan_rounds = 4\nescalate_implementer_after_reworks = 2\n\
             [pipeline.plan_author]\nexecutor = \"claude\"\nmodel = \"model-a\"\neffort = \"high\"\n",
        ),
        Some("[pipeline]\nmax_plan_rounds = 9\nescalate_implementer_after_reworks = 0\n"),
        Some(
            "[pipeline]\nmax_plan_rounds = 1\nescalate_implementer_after_reworks = 3\n\
             [pipeline.plan_author]\nexecutor = \"codex\"\n",
        ),
    );
    assert_eq!(clamped.repo.pipeline.max_plan_rounds, MAX_PLAN_ROUNDS_CAP);
    assert_eq!(clamped.repo.pipeline.escalate_implementer_after_reworks, 1);
    assert_eq!(clamped.repo.pipeline.plan_author.executor, "claude");
    assert!(has_warn(&clamped, "pipeline.max_plan_rounds = 9 clamped"));
    assert!(has_warn(
        &clamped,
        "pipeline.escalate_implementer_after_reworks = 0 clamped"
    ));
    assert!(has_warn(&clamped, "pipeline.max_plan_rounds = 1 ignored"));
    assert!(has_warn(
        &clamped,
        "pipeline.escalate_implementer_after_reworks = 3 ignored"
    ));
    assert!(has_warn(&clamped, "pipeline.plan_author ignored"));
}

#[test]
fn tier2_custom_or_same_provider_reviewers_cannot_bypass_tier1_dual_review() {
    let t1 = "[cross_audit]\ndual_review = true\n\
              [[cross_audit.reviewers]]\nid = \"rev-claude\"\nexecutor = \"claude\"\nmodel = \"m1\"\n\
              [[cross_audit.reviewers]]\nid = \"rev-agy\"\nexecutor = \"antigravity\"\nmodel = \"m2\"\n";

    // Tier 2 injects custom + claude (only 1 real canonical provider).
    let t2_custom = "[[cross_audit.reviewers]]\nid = \"r1\"\nexecutor = \"claude\"\n\
                     [[cross_audit.reviewers]]\nid = \"r2\"\nexecutor = \"custom\"\nexecutor_cmd = \"true\"\n";
    let cfg = load_tiers(Some(t1), Some(t2_custom), None);
    assert!(cfg.repo.cross_audit.dual_review);
    assert_eq!(cfg.repo.cross_audit.reviewers.len(), 2);
    assert_eq!(cfg.repo.cross_audit.reviewers[0].id, "rev-claude");
    assert_eq!(cfg.repo.cross_audit.reviewers[1].id, "rev-agy");
    assert!(has_warn(&cfg, "cross_audit.reviewers ignored"));

    // Tier 2 collapses gemini + antigravity to 1 canonical provider ("gemini").
    let t2_same_family = "[[cross_audit.reviewers]]\nid = \"g1\"\nexecutor = \"gemini\"\n\
                          [[cross_audit.reviewers]]\nid = \"g2\"\nexecutor = \"antigravity\"\n";
    let cfg2 = load_tiers(Some(t1), Some(t2_same_family), None);
    assert_eq!(cfg2.repo.cross_audit.reviewers[0].id, "rev-claude");
    assert!(has_warn(&cfg2, "cross_audit.reviewers ignored"));

    // Tier 2 with 2 distinct real providers wins cleanly.
    let t2_valid = "[[cross_audit.reviewers]]\nid = \"c1\"\nexecutor = \"claude\"\n\
                    [[cross_audit.reviewers]]\nid = \"x1\"\nexecutor = \"codex\"\n";
    let cfg3 = load_tiers(
        Some(t1),
        Some(t2_valid),
        Some("[[cross_audit.reviewers]]\nid = \"evil\"\nexecutor = \"custom\"\n"),
    );
    assert_eq!(cfg3.repo.cross_audit.reviewers[0].id, "c1");
    assert_eq!(cfg3.repo.cross_audit.reviewers[1].id, "x1");
    assert!(has_warn(&cfg3, "cross_audit.reviewers ignored"));
}

#[test]
fn tier2_reviewers_with_executor_cmd_cannot_replace_tier1_global_reviewers() {
    let t1 = "[cross_audit]\ndual_review = true\n\
              [[cross_audit.reviewers]]\nid = \"rev-claude\"\nexecutor = \"claude\"\nmodel = \"m1\"\n\
              [[cross_audit.reviewers]]\nid = \"rev-agy\"\nexecutor = \"antigravity\"\nmodel = \"m2\"\n";

    // Both Tier-2 reviewers spoof real provider names with a custom executor_cmd.
    let t2_both_spoofed = "[[cross_audit.reviewers]]\nid = \"fake-c\"\nexecutor = \"claude\"\n\
                           executor_cmd = \"/bin/echo PASS\"\n\
                           [[cross_audit.reviewers]]\nid = \"fake-a\"\nexecutor = \"antigravity\"\n\
                           executor_cmd = \"/bin/echo PASS\"\n";
    let cfg = load_tiers(Some(t1), Some(t2_both_spoofed), None);
    assert!(cfg.repo.cross_audit.dual_review);
    assert_eq!(cfg.repo.cross_audit.reviewers.len(), 2);
    assert_eq!(cfg.repo.cross_audit.reviewers[0].id, "rev-claude");
    assert_eq!(cfg.repo.cross_audit.reviewers[1].id, "rev-agy");
    assert!(has_warn(&cfg, "cross_audit.reviewers ignored"));

    // One real reviewer + one reviewer with non-empty executor_cmd only counts as 1.
    let t2_one_spoofed = "[[cross_audit.reviewers]]\nid = \"real-c\"\nexecutor = \"claude\"\n\
                          [[cross_audit.reviewers]]\nid = \"fake-a\"\nexecutor = \"antigravity\"\n\
                          executor_cmd = \"/bin/echo PASS\"\n";
    let cfg2 = load_tiers(Some(t1), Some(t2_one_spoofed), None);
    assert_eq!(cfg2.repo.cross_audit.reviewers[0].id, "rev-claude");
    assert_eq!(cfg2.repo.cross_audit.reviewers[1].id, "rev-agy");
    assert!(has_warn(&cfg2, "cross_audit.reviewers ignored"));
}

#[test]
fn invalid_or_duplicate_reviewer_id_fails_config_load() {
    for bad in [
        "[[cross_audit.reviewers]]\nid = \"../evil\"\nexecutor = \"claude\"\n",
        "[[cross_audit.reviewers]]\nid = \"\"\nexecutor = \"claude\"\n",
        "[[cross_audit.reviewers]]\nid = \"dup\"\nexecutor = \"claude\"\n\
         [[cross_audit.reviewers]]\nid = \"dup\"\nexecutor = \"codex\"\n",
    ] {
        let err_t2 = try_load_tiers(None, Some(bad), None, ConfigOverlay::default()).unwrap_err();
        assert!(matches!(err_t2, ConfigError::Parse { .. }), "{err_t2:?}");
        let err_t1 = try_load_tiers(Some(bad), None, None, ConfigOverlay::default()).unwrap_err();
        assert!(matches!(err_t1, ConfigError::Parse { .. }), "{err_t1:?}");
    }
}

#[test]
fn global_unblocker_floor_applies_when_dual_or_debate_active() {
    let t1 = "[pipeline]\nplan_debate = true\n\
              [unblocker]\nmax_token_budget = 1500000\nmax_wall_clock_secs = 5400\nmax_unblock_cycles = 8\n";
    let t2_init = "[unblocker]\nmax_token_budget = 500000\nmax_wall_clock_secs = 3600\nmax_unblock_cycles = 5\n";

    // Active debate: Tier 1 floor raises explicit aaf-init defaults.
    let active = load_tiers(Some(t1), Some(t2_init), None);
    assert_eq!(active.repo.unblocker.max_token_budget, 1_500_000);
    assert_eq!(active.repo.unblocker.max_wall_clock_secs, 5_400);
    assert_eq!(active.repo.unblocker.max_unblock_cycles, 8);

    // Higher explicit Tier 2 values are preserved above the Tier 1 floor.
    let t2_higher = "[unblocker]\nmax_token_budget = 3000000\nmax_wall_clock_secs = 9000\nmax_unblock_cycles = 12\n";
    let kept = load_tiers(Some(t1), Some(t2_higher), None);
    assert_eq!(kept.repo.unblocker.max_token_budget, 3_000_000);
    assert_eq!(kept.repo.unblocker.max_wall_clock_secs, 9_000);
    assert_eq!(kept.repo.unblocker.max_unblock_cycles, 12);

    // Inactive dual/debate: explicit Tier 2 wins; omitted Tier 2 inherits Tier 1.
    let t1_only_unblocker = "[unblocker]\nmax_token_budget = 1500000\nmax_wall_clock_secs = 5400\nmax_unblock_cycles = 8\n";
    let inactive = load_tiers(Some(t1_only_unblocker), Some(t2_init), None);
    assert_eq!(inactive.repo.unblocker.max_token_budget, 500_000);
    assert_eq!(inactive.repo.unblocker.max_wall_clock_secs, 3_600);
    assert_eq!(inactive.repo.unblocker.max_unblock_cycles, 5);

    let omitted = load_tiers(Some(t1_only_unblocker), None, None);
    assert_eq!(omitted.repo.unblocker.max_token_budget, 1_500_000);
    assert_eq!(omitted.repo.unblocker.max_wall_clock_secs, 5_400);
    assert_eq!(omitted.repo.unblocker.max_unblock_cycles, 8);
}

#[test]
fn tighten_trusted_base_matches_load_pipeline_dual_and_unblocker_merges() {
    let mut global = GlobalConfig::default();
    global.pipeline.plan_debate = Some(true);
    global.pipeline.allow_tier_s_skip_debate = Some(true);
    global.pipeline.plan_author = Some(StageModelConfig {
        executor: "claude".to_string(),
        executor_cmd: String::new(),
        model: "m-author".to_string(),
        effort: "high".to_string(),
    });
    global.cross_audit.dual_review = Some(true);
    global.cross_audit.cross_adjudicate = Some(true);
    global.cross_audit.reviewers = vec![
        ReviewerConfig {
            id: "r-claude".to_string(),
            executor: "claude".to_string(),
            ..ReviewerConfig::default()
        },
        ReviewerConfig {
            id: "r-agy".to_string(),
            executor: "antigravity".to_string(),
            ..ReviewerConfig::default()
        },
    ];
    global.unblocker.max_token_budget = Some(1_500_000);
    global.unblocker.max_wall_clock_secs = Some(5_400);
    global.unblocker.max_unblock_cycles = Some(8);

    let mut base = RepoConfig::default();
    base.pipeline.allow_tier_s_skip_debate = false;
    base.pipeline.max_plan_rounds = 9;
    base.pipeline.max_audit_reworks = 9;
    base.cross_audit.cross_adjudicate = false;
    base.unblocker.max_token_budget = 500_000;
    base.unblocker.max_wall_clock_secs = 3_600;
    base.unblocker.max_unblock_cycles = 5;

    let no_env = |_: &str| None;
    let tightened = tighten_trusted_base(base, &global, None, &no_env).unwrap();
    assert!(tightened.pipeline.plan_debate);
    assert!(tightened.pipeline.plan_stage);
    assert!(!tightened.pipeline.allow_tier_s_skip_debate);
    assert_eq!(tightened.pipeline.max_plan_rounds, MAX_PLAN_ROUNDS_CAP);
    assert_eq!(tightened.pipeline.max_audit_reworks, MAX_AUDIT_REWORKS_CAP);
    assert_eq!(tightened.pipeline.plan_author.executor, "claude");
    assert!(tightened.cross_audit.dual_review);
    assert!(!tightened.cross_audit.cross_adjudicate);
    assert_eq!(tightened.cross_audit.reviewers.len(), 2);
    assert_eq!(tightened.unblocker.max_token_budget, 1_500_000);
    assert_eq!(tightened.unblocker.max_wall_clock_secs, 5_400);
    assert_eq!(tightened.unblocker.max_unblock_cycles, 8);

    // Invalid reviewer ID also fails closed in tighten_trusted_base.
    let mut bad_base = RepoConfig::default();
    bad_base.cross_audit.reviewers.push(ReviewerConfig {
        id: "../escape".to_string(),
        executor: "claude".to_string(),
        ..ReviewerConfig::default()
    });
    assert!(tighten_trusted_base(bad_base, &global, None, &no_env).is_err());
}

fn reviewer(id: &str, executor: &str, cmd: &str, model: &str, effort: &str) -> ReviewerConfig {
    ReviewerConfig {
        id: id.to_string(),
        executor: executor.to_string(),
        executor_cmd: cmd.to_string(),
        model: model.to_string(),
        effort: effort.to_string(),
    }
}

#[test]
fn config_hash_preserves_dual_review_false_and_tracks_dual_review_fields() {
    let base = RepoConfig::default();
    let default_hash = gate::config_hash(&base);

    // dual_review = false: bit-for-bit identical even if cross_adjudicate/reviewers differ.
    let mut inactive = base.clone();
    inactive.cross_audit.cross_adjudicate = false;
    inactive.cross_audit.reviewers = vec![reviewer("r1", "claude", "", "m1", "high")];
    assert_eq!(gate::config_hash(&inactive), default_hash);

    // Also bit-for-bit identical when score_gate = true and dual_review = false.
    let mut scored = base.clone();
    scored.cross_audit.score_gate = true;
    let scored_hash = gate::config_hash(&scored);
    let mut scored_diff_adj = scored.clone();
    scored_diff_adj.cross_audit.cross_adjudicate = false;
    scored_diff_adj.cross_audit.reviewers = vec![reviewer("r1", "claude", "", "m1", "high")];
    assert_eq!(gate::config_hash(&scored_diff_adj), scored_hash);

    // dual_review = true breaks early-return even under default HeadLock + !score_gate.
    let mut dual = base.clone();
    dual.cross_audit.audit_scope = AuditScope::HeadLock;
    dual.cross_audit.dual_review = true;
    dual.cross_audit.reviewers = vec![
        reviewer("a", "claude", "", "m-a", "high"),
        reviewer("b", "antigravity", "", "m-b", "high"),
    ];
    let h_dual = gate::config_hash(&dual);
    assert_ne!(h_dual, default_hash);

    // Order-independent on reviewers.
    let mut reordered = dual.clone();
    reordered.cross_audit.reviewers.reverse();
    assert_eq!(gate::config_hash(&reordered), h_dual);

    // Sensitive to cross_adjudicate.
    let mut no_adj = dual.clone();
    no_adj.cross_audit.cross_adjudicate = false;
    assert_ne!(gate::config_hash(&no_adj), h_dual);

    // Sensitive to executor (antigravity vs gemini), executor_cmd, model, effort, id.
    let mut diff_exec = dual.clone();
    diff_exec.cross_audit.reviewers[1].executor = "gemini".to_string();
    assert_ne!(gate::config_hash(&diff_exec), h_dual);

    let mut diff_cmd = dual.clone();
    diff_cmd.cross_audit.reviewers[1].executor_cmd = "/usr/local/bin/agy".to_string();
    assert_ne!(gate::config_hash(&diff_cmd), h_dual);

    let mut diff_model = dual.clone();
    diff_model.cross_audit.reviewers[1].model = "m-c".to_string();
    assert_ne!(gate::config_hash(&diff_model), h_dual);

    let mut diff_effort = dual.clone();
    diff_effort.cross_audit.reviewers[1].effort = "medium".to_string();
    assert_ne!(gate::config_hash(&diff_effort), h_dual);

    let mut diff_id = dual.clone();
    diff_id.cross_audit.reviewers[1].id = "b2".to_string();
    assert_ne!(gate::config_hash(&diff_id), h_dual);
}
