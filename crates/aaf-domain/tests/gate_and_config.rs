#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Gate value rules and configuration defaults.

use aaf_domain::account::{AccountProfile, AccountState, ProviderKind};
use aaf_domain::config::{
    AccountsConfig, DEFAULT_MIN_ANDROID_TARGET_SDK, GateConfig, GlobalConfig, PipelineConfig,
    RepoConfig, RunConfig, StoreMode, UnblockerConfig,
};
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::gate::{
    Approval, CommandResult, GateStatus, Severity, StoreComplianceReport, StoreFinding, StoreStatus,
};
use serde_json::json;

fn finding(rule: &str, severity: Severity) -> StoreFinding {
    StoreFinding {
        rule: rule.to_string(),
        severity,
        platform: "android".to_string(),
        guideline: "g".to_string(),
        file: "AndroidManifest.xml".to_string(),
        line: Some(3),
        message: "m".to_string(),
        fix: "f".to_string(),
    }
}

#[test]
fn gate_exit_code_is_zero_only_for_clear() {
    assert_eq!(GateStatus::Clear.exit_code(), 0);
    assert_eq!(GateStatus::Blocked.exit_code(), 2);
    assert_eq!(GateStatus::Unverified.exit_code(), 2);
}

#[test]
fn severity_orders_info_below_warn_below_critical() {
    assert!(Severity::Info < Severity::Warn);
    assert!(Severity::Warn < Severity::Critical);
}

#[test]
fn report_without_findings_is_clear() {
    let report = StoreComplianceReport::from_findings(Vec::new(), Vec::new());
    assert_eq!(report.status, StoreStatus::Clear);
    assert_eq!((report.critical, report.warnings), (0, 0));
    assert!(report.findings.is_empty());
    assert!(report.unresolved.is_empty());
}

#[test]
fn warnings_and_info_never_block() {
    let findings = vec![
        finding("AND-101", Severity::Warn),
        finding("AND-103", Severity::Warn),
        finding("X-000", Severity::Info),
    ];
    let report = StoreComplianceReport::from_findings(findings.clone(), Vec::new());
    assert_eq!(report.status, StoreStatus::Clear);
    assert_eq!((report.critical, report.warnings), (0, 2));
    assert_eq!(report.findings, findings);
}

#[test]
fn one_critical_blocks_and_counts_are_per_severity() {
    let findings = vec![
        finding("AND-101", Severity::Warn),
        finding("AND-001", Severity::Critical),
        finding("X-000", Severity::Info),
        finding("AND-003", Severity::Critical),
    ];
    let report = StoreComplianceReport::from_findings(findings.clone(), Vec::new());
    assert_eq!(report.status, StoreStatus::Blocked);
    assert_eq!((report.critical, report.warnings), (2, 1));
    // Findings are kept as given, in order.
    assert_eq!(report.findings, findings);
}

#[test]
fn unresolved_values_are_carried_but_do_not_block_on_their_own() {
    let unresolved = vec!["flutter.targetSdkVersion".to_string()];
    let report = StoreComplianceReport::from_findings(Vec::new(), unresolved.clone());
    assert_eq!(report.status, StoreStatus::Clear);
    assert_eq!(report.unresolved, unresolved);
}

#[test]
fn skipped_and_not_applicable_are_empty_and_never_clear() {
    let skipped = StoreComplianceReport::skipped();
    let na = StoreComplianceReport::not_applicable();
    assert_eq!(skipped.status, StoreStatus::Skipped);
    assert_eq!(na.status, StoreStatus::NotApplicable);
    for report in [skipped, na] {
        assert_ne!(report.status, StoreStatus::Clear);
        assert_eq!((report.critical, report.warnings), (0, 0));
        assert!(report.findings.is_empty() && report.unresolved.is_empty());
    }
}

#[test]
fn approval_passes_only_with_pass_verdict_on_the_same_fingerprint() {
    let approval = |verdict: &str, lock_fp: &str| Approval {
        lock_fp: lock_fp.to_string(),
        reviewer: "r".to_string(),
        verdict: verdict.to_string(),
        same_account: false,
        created_at: 1,
    };
    assert!(approval("PASS", "abc").passes("abc"));
    // Wrong fingerprint: the approval covers some other set of changes.
    assert!(!approval("PASS", "abc").passes("abd"));
    assert!(!approval("PASS", "abc").passes(""));
    assert!(!approval("PASS", "abc").passes("ABC"));
    assert!(!approval("PASS", "abc").passes("abc "));
    // Anything but the exact verdict does not unlock.
    for verdict in [
        "FAIL", "pass", "Pass", "PASS ", " PASS", "", "PASSED", "APPROVED",
    ] {
        assert!(!approval(verdict, "abc").passes("abc"), "{verdict:?}");
    }
}

#[test]
fn command_result_passes_only_on_exit_zero_without_timeout() {
    let result = |exit_code: Option<i32>, timed_out: bool| CommandResult {
        name: "lint".to_string(),
        command: "true".to_string(),
        exit_code,
        timed_out,
        duration_ms: 1,
        log_tail: String::new(),
    };
    assert!(result(Some(0), false).passed());
    assert!(!result(Some(1), false).passed());
    assert!(!result(Some(-1), false).passed());
    assert!(!result(None, false).passed());
    assert!(!result(Some(0), true).passed());
    assert!(!result(None, true).passed());
}

#[test]
fn account_cooldown_ends_at_its_deadline() {
    let state = |cooldown_until| AccountState {
        profile: AccountProfile {
            id: "a".to_string(),
            provider: ProviderKind::ClaudeCode,
            config_dir: "/x".to_string(),
            name: "x".to_string(),
        },
        cooldown_until,
    };
    assert!(!state(None).in_cooldown(100));
    assert!(state(Some(101)).in_cooldown(100));
    assert!(!state(Some(100)).in_cooldown(100));
    assert!(!state(Some(99)).in_cooldown(100));
}

#[test]
fn factory_event_builders_attach_task_and_run() {
    let bare = FactoryEvent::new(EventKind::RunStarted, "{}", 7);
    assert_eq!((bare.id, &bare.task_id, &bare.run_id), (None, &None, &None));
    assert_eq!(
        (bare.kind, bare.payload_json.as_str(), bare.created_at),
        (EventKind::RunStarted, "{}", 7)
    );
    let full = bare.for_task("t1").for_run("r1");
    assert_eq!(full.task_id.as_deref(), Some("t1"));
    assert_eq!(full.run_id.as_deref(), Some("r1"));
}

#[test]
fn min_android_target_sdk_defaults_to_36() {
    assert_eq!(DEFAULT_MIN_ANDROID_TARGET_SDK, 36);
    assert_eq!(GateConfig::default().min_android_target_sdk, 36);
    assert_eq!(RepoConfig::default().gate.min_android_target_sdk, 36);
}

#[test]
fn store_mode_orders_off_below_auto_below_always_and_defaults_to_auto() {
    assert!(StoreMode::Off < StoreMode::Auto);
    assert!(StoreMode::Auto < StoreMode::Always);
    assert_eq!(StoreMode::Off.max(StoreMode::Always), StoreMode::Always);
    assert_eq!(StoreMode::default(), StoreMode::Auto);
    assert_eq!(GateConfig::default().store_compliance, StoreMode::Auto);
}

#[test]
fn gate_defaults() {
    let gate = GateConfig::default();
    assert_eq!(gate.base_ref, "auto");
    assert_eq!(gate.command_timeout_secs, 1800);
    assert!(!gate.head_lock_globs.is_empty());
}

#[test]
fn default_head_lock_globs_have_no_bare_substring_test_pattern() {
    let globs = GateConfig::default().head_lock_globs;
    for glob in &globs {
        // `*test*` would lock `src/latest.rs` and `src/contest.rs`.
        assert!(
            !glob.to_ascii_lowercase().contains("*test*"),
            "bare substring pattern: {glob}"
        );
        assert!(!glob.trim().is_empty());
        assert!(!glob.starts_with('/'), "globs are repo-relative: {glob}");
    }
    let mut unique = globs.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), globs.len(), "duplicate glob");
    // The files that define the gate itself must stay locked.
    for must in [
        "**/tests/**",
        ".github/workflows/**",
        ".autofactory/config.toml",
        "**/Cargo.toml",
        "**/package.json",
        "**/build.gradle",
        "**/build.gradle.kts",
    ] {
        assert!(globs.iter().any(|g| g == must), "missing {must}");
    }
}

#[test]
fn unblocker_and_run_defaults_are_conservative() {
    let unblocker = UnblockerConfig::default();
    assert_eq!(unblocker.max_unblock_cycles, 10);
    assert_eq!(unblocker.max_subtask_depth, 1);
    assert_eq!(unblocker.max_token_budget, 2_000_000);
    assert_eq!(unblocker.max_wall_clock_secs, 10_800);
    assert_eq!(unblocker.global_freeze_consecutive_fails, 3);
    assert_eq!(PipelineConfig::default().max_audit_reworks, 3);

    let run = RunConfig::default();
    assert_eq!(run.executor, "claude");
    assert!(run.executor_cmd.is_empty());
    assert!(!run.auto_merge, "auto-merge must be opt-in");
    assert_eq!(run.max_parallel, 1);
    assert_eq!(run.run_timeout_secs, 3600);
}

#[test]
fn global_defaults_trust_nobody_and_send_no_webhook() {
    let global = GlobalConfig::default();
    assert!(global.safety.owner_github_allowlist.is_empty());
    assert_eq!(global.notifications.webhook_url, "");
    assert_eq!(global.accounts.max_concurrent_per_account, 1);
    assert_eq!(
        AccountsConfig::default().discovery_globs,
        vec!["~/.claude".to_string(), "~/.claude-*".to_string()]
    );
    assert!(global.accounts.aliases.is_empty());
}

#[test]
fn repo_defaults_have_auto_profile_and_no_commands() {
    let repo = RepoConfig::default();
    assert_eq!(repo.project.name, "");
    assert_eq!(repo.project.profile, "auto");
    let c = &repo.commands;
    assert_eq!((c.build.as_str(), c.test_scoped.as_str()), ("", ""));
    assert_eq!((c.test_full.as_str(), c.lint.as_str()), ("", ""));
    assert_eq!(c.repro, "");
}

#[test]
fn missing_sections_and_fields_fall_back_to_defaults() {
    let repo: RepoConfig = serde_json::from_value(json!({})).unwrap();
    assert_eq!(repo, RepoConfig::default());
    let global: GlobalConfig = serde_json::from_value(json!({})).unwrap();
    assert_eq!(global, GlobalConfig::default());

    let partial: RepoConfig = serde_json::from_value(json!({
        "gate": {"store_compliance": "always"},
        "run": {"auto_merge": true},
    }))
    .unwrap();
    assert_eq!(partial.gate.store_compliance, StoreMode::Always);
    assert_eq!(partial.gate.min_android_target_sdk, 36);
    assert_eq!(
        partial.gate.head_lock_globs,
        GateConfig::default().head_lock_globs
    );
    assert!(partial.run.auto_merge);
    assert_eq!(partial.run.executor, "claude");
    assert_eq!(partial.unblocker, UnblockerConfig::default());
}

#[test]
fn config_round_trips_through_serde() {
    let repo = RepoConfig::default();
    let back: RepoConfig = serde_json::from_value(serde_json::to_value(&repo).unwrap()).unwrap();
    assert_eq!(back, repo);
}

#[test]
fn default_account_name_is_the_dir_name_without_its_dot_prefix() {
    use aaf_domain::account::default_account_name;
    for (dir, want) in [
        ("/home/u/.claude", "claude"),
        ("/home/u/.claude-work", "work"),
        ("/home/u/.claude-work/", "work"),
        ("/home/u/.codex", "codex"),
        ("/home/u/.config/opencode", "opencode"),
        ("/home/u/.gemini", "gemini"),
        ("/home/u/plain dir", "plain dir"),
        // Nothing would be left after stripping: keep the component.
        ("/home/u/.claude-", ".claude-"),
        ("/home/u/.", "."),
        ("/", "/"),
        ("", ""),
    ] {
        assert_eq!(default_account_name(dir), want, "{dir:?}");
    }
}

#[test]
fn account_profile_without_a_name_field_still_parses() {
    let old: AccountProfile = serde_json::from_value(json!({
        "id": "a", "provider": "codex", "config_dir": "/h/.codex",
    }))
    .unwrap();
    assert_eq!(old.name, "");
    let aliases: GlobalConfig = serde_json::from_value(json!({
        "accounts": {"aliases": {"~/.claude-work": "Work"}},
    }))
    .unwrap();
    assert_eq!(aliases.accounts.aliases["~/.claude-work"], "Work");
}

#[test]
fn account_selection_defaults_to_sticky_and_has_three_wire_names() {
    use aaf_domain::config::AccountSelection;
    assert_eq!(
        GlobalConfig::default().accounts.selection,
        AccountSelection::Sticky
    );
    for (wire, mode) in [
        ("pinned", AccountSelection::Pinned),
        ("sticky", AccountSelection::Sticky),
        ("least_used", AccountSelection::LeastUsed),
    ] {
        let global: GlobalConfig =
            serde_json::from_value(json!({ "accounts": { "selection": wire } })).unwrap();
        assert_eq!(global.accounts.selection, mode);
        assert_eq!(mode.as_str(), wire);
    }
    let unknown =
        serde_json::from_value::<GlobalConfig>(json!({ "accounts": { "selection": "rotate" } }));
    assert!(unknown.is_err());
}
