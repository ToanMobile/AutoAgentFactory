#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Phase 1 v3.2 3-tier `[pipeline]` & `max_open_review` config merge,
//! config render round-trip, and prompt partitioning tests.

use std::fs;
use std::path::{Path, PathBuf};

use aaf_domain::config::{CommandsConfig, MAX_AUDIT_REWORKS_CAP, RepoConfig};
use aaf_domain::task::{ImmutableSpec, Priority, Task};
use aaf_engine::config::{
    ConfigInputs, ConfigOverlay, Detected, EffectiveConfig, LOCAL_CONFIG_REL, REPO_CONFIG_REL,
    load, render_repo_config,
};
use aaf_engine::factory::Settings;
use aaf_engine::factory::prompt::{
    PromptContext, build_plan_prompt, build_prompt, plan_sandbox_policy,
};
use tempfile::TempDir;

fn sample_spec() -> ImmutableSpec {
    ImmutableSpec {
        description: "Implement feature".to_string(),
        acceptance: vec!["Tests pass".to_string()],
    }
}

fn make_settings(repo: RepoConfig, data_dir: PathBuf, home: &Path, sandboxed: bool) -> Settings {
    Settings {
        repo_root: "/fake/repo".to_string(),
        repo,
        max_parallel: 1,
        auto_merge: false,
        sandboxed,
        hook_required: false,
        agent_path: std::ffi::OsString::new(),
        mobile: false,
        data_dir,
        home: home.to_path_buf(),
        owner_github_allowlist: Vec::new(),
    }
}

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
fn pipeline_tier2_clamps_max_audit_reworks_and_tier3_tighten_only() {
    let t2 = "[pipeline]\nplan_stage = true\nrequire_plan_approval = false\nmax_audit_reworks = 9";
    let t3 = "[pipeline]\nplan_stage = false\nrequire_plan_approval = true\nmax_audit_reworks = 1";
    let cfg = load_tiers(None, Some(t2), Some(t3));
    assert_eq!(cfg.repo.pipeline.max_audit_reworks, MAX_AUDIT_REWORKS_CAP);
    assert!(cfg.repo.pipeline.plan_stage && cfg.repo.pipeline.require_plan_approval);
    assert!(has_warn(&cfg, "clamped to 5"));
    assert!(has_warn(&cfg, "pipeline.plan_stage = false ignored"));
    assert!(has_warn(&cfg, "pipeline.max_audit_reworks = 1 ignored"));

    let same = load_tiers(
        None,
        Some("[pipeline]\nmax_audit_reworks = 2"),
        Some("[pipeline]\nmax_audit_reworks = 2"),
    );
    assert_eq!(same.repo.pipeline.max_audit_reworks, 2);
    assert!(has_warn(&same, "pipeline.max_audit_reworks = 2 ignored"));
}

#[test]
fn max_open_review_three_tier_merge_and_tighten_only() {
    let cfg = load_tiers(
        Some("[run]\nmax_open_review = 4"),
        Some("[run]\nmax_open_review = 0"),
        Some("[run]\nmax_open_review = 0"),
    );
    assert_eq!(cfg.repo.run.max_open_review, 4);
    assert!(has_warn(&cfg, "run.max_open_review = 0 ignored"));

    let tighter = load_tiers(
        Some("[run]\nmax_open_review = 5"),
        Some("[run]\nmax_open_review = 3"),
        Some("[run]\nmax_open_review = 4"),
    );
    assert_eq!(tighter.repo.run.max_open_review, 3);
    assert!(has_warn(&tighter, "run.max_open_review = 4 ignored"));

    let lower = load_tiers(
        Some("[run]\nmax_open_review = 5"),
        Some("[run]\nmax_open_review = 4"),
        Some("[run]\nmax_open_review = 2"),
    );
    assert_eq!(lower.repo.run.max_open_review, 2);
    assert!(lower.warnings.is_empty(), "{:?}", lower.warnings);

    let from_zero = load_tiers(None, None, Some("[run]\nmax_open_review = 2"));
    assert_eq!(from_zero.repo.run.max_open_review, 2);
    assert!(from_zero.warnings.is_empty(), "{:?}", from_zero.warnings);
}

#[test]
fn render_repo_config_round_trips_pipeline_and_max_open_review() {
    let detected = Detected {
        profile: "rust".to_string(),
        commands: CommandsConfig::default(),
        evidence: vec!["Cargo.toml".to_string()],
    };
    let text = render_repo_config(&detected, "demo");
    for needle in [
        "max_open_review = 0",
        "[pipeline]",
        "plan_stage = false",
        "require_plan_approval = false",
        "max_audit_reworks = 3",
    ] {
        assert!(text.contains(needle), "missing {needle} in:\n{text}");
    }
    let parsed: RepoConfig = toml::from_str(&text).unwrap();
    let mut want = RepoConfig::default();
    want.project.name = "demo".to_string();
    want.project.profile = "rust".to_string();
    assert_eq!(parsed, want);
}

#[test]
fn prompt_partitions_plan_steer_and_failure_notes_strictly() {
    let mut task = Task::new_root("t1", "/repo", "Feature X", sample_spec(), Priority::P1, 1);
    task.context_notes = vec![
        "plan_approved: Touch src/x.rs and add unit test".into(),
        "steer: Keep public signature unchanged".into(),
    ];

    let clean = build_prompt(&task, "aaf/t1");
    assert!(clean.contains("# Approved Plan\n") && clean.contains("Touch src/x.rs"));
    assert!(clean.contains("# Operator Steering\n") && clean.contains("- Keep public signature"));
    assert!(!clean.contains("# Notes from earlier attempts"));
    assert!(!clean.contains("plan_approved:") && !clean.contains("steer:"));

    task.context_notes
        .push("audit_rework: fix missing check".into());
    let mixed = build_prompt(&task, "aaf/t1");
    let (_, fail_part) = mixed.split_once("# Notes from earlier attempts").unwrap();
    assert!(fail_part.contains("audit_rework: fix missing check"));
    assert!(!fail_part.contains("Touch src/x.rs") && !fail_part.contains("Keep public signature"));
}

#[test]
fn plan_prompt_and_read_only_sandbox_policy() {
    let dir = TempDir::new().unwrap();
    let mut task = Task::new_root("t1", "/repo", "Refactor", sample_spec(), Priority::P2, 1);
    task.context_notes.push("steer: Avoid extra allocs".into());
    let ctx = PromptContext {
        repo_root: dir.path(),
        profile: "rust",
        proof_required: false,
    };
    let prompt = build_plan_prompt(&task, &ctx);
    for needle in [
        "READ-ONLY",
        "1. Goal:",
        "2. Files:",
        "3. Checks:",
        "Avoid extra allocs",
    ] {
        assert!(prompt.contains(needle), "missing {needle}");
    }
    let settings = make_settings(
        RepoConfig::default(),
        dir.path().join("data"),
        dir.path(),
        true,
    );
    let policy = plan_sandbox_policy(&settings, None, &[]);
    assert!(policy.enabled && policy.read_only);
    let denied = settings.data_dir.to_string_lossy().into_owned();
    assert!(policy.read_denied_paths.contains(&denied));
}
