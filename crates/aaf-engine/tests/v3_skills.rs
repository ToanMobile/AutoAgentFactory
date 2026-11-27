#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Pillar 5: embedded domain & UI/UX skills pack (15 skills) with profile filtering & safe sync.

use aaf_domain::config::ProofConfig;
use aaf_engine::skills::{
    all_skills, inspect_skills_dir, skills_for_profile, sync_skills, sync_skills_for_profile,
};

#[test]
fn embedded_skills_pack_maps_profiles_and_syncs_safely() {
    let skills = all_skills();
    assert_eq!(skills.len(), 15);

    let names: Vec<&str> = skills.iter().map(|s| s.name).collect();
    for expected in [
        "aaf-workflow",
        "tdd-workflow",
        "merge-conflict-resolver",
        "qa-review",
        "mobile-proof-qa",
        "store-compliance",
        "crash-triage",
        "security-gate",
        "ui-ux-pro-max",
        "taste-mobile-app",
        "impeccable",
        "qa-visual",
        "unity-game-craft",
        "web-frontend-qa",
        "backend-service-gate",
    ] {
        assert!(names.contains(&expected), "missing skill {expected}");
    }
    assert!(!names.contains(&"ui-craft-mobile"));

    let mobile = skills.iter().find(|s| s.name == "mobile-proof-qa").unwrap();
    assert!(mobile.content.contains("mobile-mcp"));
    assert!(mobile.content.contains("mobile_save_screenshot"));
    assert!(mobile.content.contains("MOBILEMCP_DISABLE_TELEMETRY=1"));
    assert!(mobile.content.contains("aaf proof record"));
    assert!(mobile.content.contains("AAF_RUN_ID"));
    assert!(!mobile.content.contains("replicant-mcp"));
    assert!(!mobile.content.contains("adb-screencap"));
    assert!(!mobile.content.contains("simctl-screenshot"));

    let workflow = skills.iter().find(|s| s.name == "aaf-workflow").unwrap();
    assert!(workflow.content.contains("mobile-mcp"));
    assert!(workflow.content.contains("AAF_RUN_ID"));
    assert!(!workflow.content.contains("replicant-mcp"));

    let unity_skill = skills
        .iter()
        .find(|s| s.name == "unity-game-craft")
        .unwrap();
    assert!(unity_skill.content.contains("RaycastNonAlloc"));
    assert!(unity_skill.content.contains("WaitForSeconds"));
    assert!(unity_skill.content.contains("StringBuilder"));

    // ProofConfig standardizes exclusively on mobile-mcp:mobile_save_screenshot.
    let proof_cfg = ProofConfig::default();
    assert_eq!(
        proof_cfg.allowed_tools,
        vec!["mobile-mcp:mobile_save_screenshot".to_string()]
    );

    // Profile mapping checks
    let names_of = |p: &str| -> Vec<&'static str> {
        skills_for_profile(p).into_iter().map(|s| s.name).collect()
    };
    assert_eq!(names_of("auto").len(), 6);
    for core in [
        "aaf-workflow",
        "tdd-workflow",
        "merge-conflict-resolver",
        "qa-review",
        "crash-triage",
        "security-gate",
    ] {
        assert!(names_of("auto").contains(&core), "missing core {core}");
    }

    for p in ["rust", "backend"] {
        let backend = names_of(p);
        assert_eq!(backend.len(), 7, "{p}: {backend:?}");
        assert!(backend.contains(&"backend-service-gate"));
        assert!(!backend.contains(&"ui-ux-pro-max"));
        assert!(!backend.contains(&"taste-mobile-app"));
        assert!(!backend.contains(&"impeccable"));
        assert!(!backend.contains(&"qa-visual"));
        assert!(!backend.contains(&"mobile-proof-qa"));
    }

    for p in ["android", "ios", "flutter", "react-native", "expo"] {
        let mobile_p = names_of(p);
        assert_eq!(mobile_p.len(), 12, "{p}: {mobile_p:?}");
        assert!(mobile_p.contains(&"mobile-proof-qa"));
        assert!(mobile_p.contains(&"store-compliance"));
        assert!(mobile_p.contains(&"ui-ux-pro-max"));
        assert!(mobile_p.contains(&"taste-mobile-app"));
        assert!(mobile_p.contains(&"impeccable"));
        assert!(mobile_p.contains(&"qa-visual"));
        assert!(!mobile_p.contains(&"unity-game-craft"));
        assert!(!mobile_p.contains(&"web-frontend-qa"));
        assert!(!mobile_p.contains(&"backend-service-gate"));
    }

    let unity = names_of("unity");
    assert_eq!(unity.len(), 10, "{unity:?}");
    assert!(unity.contains(&"unity-game-craft"));
    assert!(unity.contains(&"mobile-proof-qa"));
    assert!(unity.contains(&"store-compliance"));
    assert!(unity.contains(&"ui-ux-pro-max"));
    assert!(!unity.contains(&"taste-mobile-app"));
    assert!(!unity.contains(&"impeccable"));
    assert!(!unity.contains(&"qa-visual"));
    assert!(!unity.contains(&"web-frontend-qa"));

    let web = names_of("web");
    assert_eq!(web.len(), 10, "{web:?}");
    assert!(web.contains(&"web-frontend-qa"));
    assert!(web.contains(&"ui-ux-pro-max"));
    assert!(web.contains(&"impeccable"));
    assert!(web.contains(&"qa-visual"));
    assert!(!web.contains(&"taste-mobile-app"));
    assert!(!web.contains(&"mobile-proof-qa"));
    assert!(!web.contains(&"unity-game-craft"));

    // Sync filtered for `rust` writes only 7 skills.
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join(".claude/skills");
    let rust_sync = sync_skills_for_profile(&dest, Some("rust"), None, false).unwrap();
    assert_eq!(rust_sync.written.len(), 7);
    assert!(!dest.join("mobile-proof-qa/SKILL.md").exists());
    assert!(dest.join("backend-service-gate/SKILL.md").is_file());
    assert!(dest.join("tdd-workflow/SKILL.md").is_file());

    // Reject unknown profile
    assert!(sync_skills_for_profile(&dest, Some("unknown-stack"), None, false).is_err());

    // Sync all 15 when profile is None
    let first = sync_skills(&dest, None, false).unwrap();
    assert_eq!(first.written.len(), 8);
    assert_eq!(first.skipped_existing.len(), 7);
    assert_eq!(first.unchanged.len(), 7);
    assert!(first.skipped_modified.is_empty());

    // Re-syncing unmodified files reports all 15 unchanged, 0 skipped_modified.
    let clean = sync_skills(&dest, None, false).unwrap();
    assert!(clean.written.is_empty());
    assert_eq!(clean.unchanged.len(), 15);
    assert!(clean.skipped_modified.is_empty());
    assert_eq!(clean.skipped_existing.len(), 15);

    // Custom edit survives when force = false and is tracked in skipped_modified.
    let target = dest.join("aaf-workflow/SKILL.md");
    std::fs::write(&target, "# custom user note\n").unwrap();
    let second = sync_skills(&dest, None, false).unwrap();
    assert_eq!(second.skipped_existing.len(), 15);
    assert_eq!(second.unchanged.len(), 14);
    assert_eq!(second.skipped_modified, vec!["aaf-workflow".to_string()]);
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "# custom user note\n"
    );

    let audit = inspect_skills_dir(&dest, "rust");
    assert_eq!(audit.modified, vec!["aaf-workflow".to_string()]);
    assert!(audit.missing.is_empty());
    assert_eq!(audit.synced.len(), 6);

    // force = true restores bundled template atomically.
    let forced = sync_skills(&dest, Some(&["aaf-workflow".to_string()]), true).unwrap();
    assert_eq!(forced.written, vec!["aaf-workflow".to_string()]);
    assert!(
        std::fs::read_to_string(&target)
            .unwrap()
            .contains("AutoAgentFactory Workflow")
    );

    // Rejects unknown skill filter or path traversal.
    assert!(sync_skills(&dest, Some(&["../evil".to_string()]), false).is_err());

    #[cfg(unix)]
    {
        // Rejects symlinked skill directory.
        let symlink_dest = dest.join("crash-triage");
        std::fs::remove_dir_all(&symlink_dest).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, &symlink_dest).unwrap();
        assert!(sync_skills(&dest, Some(&["crash-triage".to_string()]), true).is_err());

        // Rejects symlinked SKILL.md even with force = true.
        let secret = outside.join("victim.md");
        std::fs::write(&secret, "untouched\n").unwrap();
        let qa_file = dest.join("qa-review/SKILL.md");
        std::fs::remove_file(&qa_file).unwrap();
        std::os::unix::fs::symlink(&secret, &qa_file).unwrap();
        assert!(sync_skills(&dest, Some(&["qa-review".to_string()]), true).is_err());
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "untouched\n");

        // Rejects ancestor `.claude` symlink escape.
        let repo_escape = tmp.path().join("repo-escape");
        std::fs::create_dir_all(&repo_escape).unwrap();
        let escape_target = tmp.path().join("escape-target");
        std::fs::create_dir_all(&escape_target).unwrap();
        std::os::unix::fs::symlink(&escape_target, repo_escape.join(".claude")).unwrap();
        assert!(
            sync_skills(&repo_escape.join(".claude/skills"), None, true).is_err(),
            "ancestor .claude symlink must be rejected"
        );
        assert!(!escape_target.join("skills").exists());
    }
}

#[test]
fn inspect_skills_flags_symlinks_and_phase0_security_invariants_hold() {
    let tmp = tempfile::tempdir().unwrap();

    // 1. inspect_skills_dir flags symlinked ancestor/skill directories as modified.
    #[cfg(unix)]
    {
        let real = tmp.path().join("real-skills");
        sync_skills_for_profile(&real, Some("rust"), None, false).unwrap();
        let sym_root = tmp.path().join("sym-skills");
        std::os::unix::fs::symlink(&real, &sym_root).unwrap();
        let root_audit = inspect_skills_dir(&sym_root, "rust");
        assert!(root_audit.synced.is_empty(), "{root_audit:?}");
        assert_eq!(root_audit.modified.len(), 7, "{root_audit:?}");

        let target_skill = real.join("tdd-workflow");
        let relocated = tmp.path().join("relocated-tdd");
        std::fs::rename(&target_skill, &relocated).unwrap();
        std::os::unix::fs::symlink(&relocated, &target_skill).unwrap();
        let skill_audit = inspect_skills_dir(&real, "rust");
        assert!(
            skill_audit.modified.contains(&"tdd-workflow".to_string()),
            "{skill_audit:?}"
        );
        assert!(
            !skill_audit.synced.contains(&"tdd-workflow".to_string()),
            "{skill_audit:?}"
        );
    }

    // 2. plan_mode rejects iOS secret files and frames context file labels inside untrusted-data.
    let plan_repo = tmp.path().join("plan-repo");
    std::fs::create_dir_all(&plan_repo).unwrap();
    for secret in [
        "GoogleService-Info.plist",
        "dist.mobileprovision",
        "AuthKey_ABC123.p8",
    ] {
        std::fs::write(plan_repo.join(secret), "secret-material").unwrap();
        let err = aaf_engine::plan_mode::build_plan_prompt(
            &plan_repo,
            "Review app config",
            &[secret.to_string()],
        )
        .unwrap_err();
        assert!(err.to_string().contains("refused"), "{secret}: {err}");
    }
    std::fs::write(plan_repo.join("notes.txt"), "hello").unwrap();
    let built = aaf_engine::plan_mode::build_plan_prompt(
        &plan_repo,
        "Check framing",
        &["notes.txt".to_string()],
    )
    .unwrap();
    let label_pos = built.prompt.find("# Context file: notes.txt").unwrap();
    let before = &built.prompt[..label_pos];
    assert!(
        before.rfind("<untrusted-data id=\"").unwrap()
            > before.rfind("</untrusted-data id=\"").unwrap(),
        "context file header must be inside untrusted-data frame: {}",
        built.prompt
    );

    // 3. detect_stack does not misclassify Nuxt/Angular web app.config.ts as Expo.
    let web_repo = tmp.path().join("nuxt-web");
    std::fs::create_dir_all(&web_repo).unwrap();
    std::fs::write(
        web_repo.join("package.json"),
        r#"{"dependencies": {"nuxt": "^3.12.0"}}"#,
    )
    .unwrap();
    std::fs::write(
        web_repo.join("app.config.ts"),
        "export default defineAppConfig({});\n",
    )
    .unwrap();
    assert_eq!(aaf_engine::config::detect_stack(&web_repo).profile, "web");

    let expo_repo = tmp.path().join("expo-app");
    std::fs::create_dir_all(&expo_repo).unwrap();
    std::fs::write(
        expo_repo.join("app.config.ts"),
        "import { ExpoConfig } from 'expo/config';\nexport default {};\n",
    )
    .unwrap();
    assert_eq!(aaf_engine::config::detect_stack(&expo_repo).profile, "expo");

    // 4. merge_cross_audit does not leak global executor_cmd/model/effort across executors.
    let cfg_repo = tmp.path().join("cfg-repo");
    std::fs::create_dir_all(cfg_repo.join(".autofactory")).unwrap();
    let global_cfg = tmp.path().join("global.toml");
    std::fs::write(
        &global_cfg,
        "[cross_audit]\nexecutor = \"codex\"\nexecutor_cmd = \"codex exec {prompt}\"\nmodel = \"gpt-5\"\neffort = \"high\"\n",
    )
    .unwrap();
    std::fs::write(
        cfg_repo.join(".autofactory/config.toml"),
        "[cross_audit]\nexecutor = \"claude\"\n",
    )
    .unwrap();
    let loaded = aaf_engine::config::load(&aaf_engine::config::ConfigInputs {
        global_config: Some(global_cfg.clone()),
        repo_root: cfg_repo.clone(),
        env: &|_| None,
        cli: aaf_engine::config::ConfigOverlay::default(),
    })
    .unwrap();
    assert_eq!(loaded.repo.cross_audit.executor, "claude");
    assert_eq!(loaded.repo.cross_audit.executor_cmd, "");
    assert_eq!(loaded.repo.cross_audit.model, "");
    assert_eq!(loaded.repo.cross_audit.effort, "");

    // Same executor DOES inherit global model/effort/executor_cmd.
    std::fs::write(
        cfg_repo.join(".autofactory/config.toml"),
        "[cross_audit]\nexecutor = \"codex\"\n",
    )
    .unwrap();
    let same = aaf_engine::config::load(&aaf_engine::config::ConfigInputs {
        global_config: Some(global_cfg),
        repo_root: cfg_repo,
        env: &|_| None,
        cli: aaf_engine::config::ConfigOverlay::default(),
    })
    .unwrap();
    assert_eq!(same.repo.cross_audit.executor_cmd, "codex exec {prompt}");
    assert_eq!(same.repo.cross_audit.model, "gpt-5");
    assert_eq!(same.repo.cross_audit.effort, "high");

    // 5. ClaudeCodeRunner uses `--permission-mode plan` when `sandbox.read_only` is true.
    let runner = aaf_engine::executors::claude::ClaudeCodeRunner::new(
        "/usr/local/bin/claude",
        aaf_engine::executors::ExecEnv::default(),
    );
    let spec = runner
        .command(&aaf_domain::ports::AgentRequest {
            run_id: "ro-1".to_string(),
            prompt: "Review diff".to_string(),
            cwd: tmp.path().to_string_lossy().into_owned(),
            account: None,
            timeout_secs: 60,
            sandbox: aaf_domain::ports::SandboxPolicy {
                enabled: true,
                read_only: true,
                ..Default::default()
            },
            json_schema: None,
            max_turns: None,
        })
        .unwrap();
    let mode_idx = spec
        .args
        .iter()
        .position(|a| a == "--permission-mode")
        .unwrap();
    assert_eq!(spec.args[mode_idx + 1], "plan");
    assert!(!spec.args.iter().any(|a| a == "bypassPermissions"));
    assert!(!spec.args.iter().any(|a| a == "acceptEdits"));
}

#[test]
fn bundled_skills_pass_static_checks_without_findings() {
    let added: Vec<aaf_engine::gate::changes::AddedLines> = aaf_engine::skills::all_skills()
        .iter()
        .map(|skill| aaf_engine::gate::changes::AddedLines {
            path: format!(".claude/skills/{}/SKILL.md", skill.name),
            lines: skill
                .content
                .lines()
                .enumerate()
                .map(|(idx, line)| ((idx + 1) as u32, line.to_string()))
                .collect(),
        })
        .collect();
    let report = aaf_engine::gate::static_checks::scan(&added).unwrap();
    assert!(
        report.findings.is_empty(),
        "bundled skill templates must not trip aaf's own static checks: {:?}",
        report.findings
    );
}
