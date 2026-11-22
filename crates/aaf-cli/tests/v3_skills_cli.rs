#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Pillar 5 CLI: `aaf skills list|sync`, multi-host `--host`, `aaf init --with-skills`, and `doctor` checks.

#[path = "cli_support.rs"]
mod support;

use serde_json::{Value, json};
use support::{Sandbox, write};

fn doctor_check<'a>(out: &'a Value, name: &str) -> &'a Value {
    out["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == json!(name))
        .unwrap_or_else(|| panic!("no check {name}: {out}"))
}

#[test]
fn skills_list_sync_and_init_with_skills_filter_by_profile() {
    let sb = Sandbox::new();
    write(&sb.repo.join("Cargo.toml"), "[package]\nname = \"demo\"\n");
    sb.commit_all("cargo");

    let (code, listed_all) = sb.json(&["skills", "list"]);
    assert_eq!(code, 0, "{listed_all}");
    assert_eq!(listed_all["count"], json!(15));

    let (code, listed_rust) = sb.json(&["skills", "list", "--profile", "rust"]);
    assert_eq!(code, 0, "{listed_rust}");
    assert_eq!(listed_rust["count"], json!(7));

    let (code, listed_android) = sb.json(&["skills", "list", "--profile", "android"]);
    assert_eq!(code, 0, "{listed_android}");
    assert_eq!(listed_android["count"], json!(12));
    let android_names: Vec<&str> = listed_android["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    for ui in [
        "ui-ux-pro-max",
        "taste-mobile-app",
        "impeccable",
        "qa-visual",
        "tdd-workflow",
        "merge-conflict-resolver",
        "qa-review",
    ] {
        assert!(android_names.contains(&ui), "{android_names:?}");
    }

    let (code, listed_web) = sb.json(&["skills", "list", "--profile", "web"]);
    assert_eq!(code, 0, "{listed_web}");
    assert_eq!(listed_web["count"], json!(10));
    let web_names: Vec<&str> = listed_web["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    for s in [
        "ui-ux-pro-max",
        "impeccable",
        "qa-visual",
        "web-frontend-qa",
    ] {
        assert!(web_names.contains(&s), "{web_names:?}");
    }

    // Invalid profile fails cleanly.
    let (code, bad_list) = sb.json(&["skills", "list", "--profile", "cobol"]);
    assert_eq!(code, 1, "{bad_list}");
    let (code, bad_sync) = sb.json(&["skills", "sync", "--profile", "cobol"]);
    assert_eq!(code, 1, "{bad_sync}");

    // Inside an agent run, `aaf skills sync` must refuse with exit 2.
    let inside = sb
        .command(&["--json", "skills", "sync"])
        .env("AAF_RUN_ID", "run-1")
        .output()
        .unwrap();
    assert_eq!(inside.status.code(), Some(2));

    // Plain `aaf init` does not write skills unless `--with-skills` is passed.
    let (code, init_default) = sb.json(&["init"]);
    assert_eq!(code, 0, "{init_default}");
    assert!(
        !sb.repo
            .join(".claude/skills/backend-service-gate/SKILL.md")
            .exists()
    );

    // `aaf init --force --with-skills` in a Rust repo writes ONLY Rust/core skills (7), NOT mobile,
    // and prints a human-readable `skills:` line in text mode.
    let init_text_out = sb.aaf(&["init", "--force", "--with-skills"]);
    assert_eq!(init_text_out.status.code(), Some(0));
    let init_text = String::from_utf8(init_text_out.stdout).unwrap();
    assert!(init_text.contains("skills: profile=rust"), "{init_text}");
    assert!(
        sb.repo
            .join(".claude/skills/backend-service-gate/SKILL.md")
            .is_file()
    );
    assert!(
        sb.repo
            .join(".claude/skills/tdd-workflow/SKILL.md")
            .is_file()
    );
    assert!(
        !sb.repo
            .join(".claude/skills/mobile-proof-qa/SKILL.md")
            .exists()
    );

    // Preserve user edits unless `--force`; `--all` syncs remaining 8 skills and distinguishes unchanged vs skipped_modified.
    std::fs::write(
        sb.repo.join(".claude/skills/security-gate/SKILL.md"),
        "keep-me\n",
    )
    .unwrap();
    let (code, sync_all) = sb.json(&["skills", "sync", "--all"]);
    assert_eq!(code, 0, "{sync_all}");
    assert_eq!(sync_all["written"].as_array().unwrap().len(), 8);
    assert_eq!(sync_all["skipped_existing"].as_array().unwrap().len(), 7);
    assert_eq!(sync_all["unchanged"].as_array().unwrap().len(), 6);
    assert_eq!(sync_all["skipped_modified"], json!(["security-gate"]));
    assert_eq!(
        std::fs::read_to_string(sb.repo.join(".claude/skills/security-gate/SKILL.md")).unwrap(),
        "keep-me\n"
    );

    // `--host agents` writes to `.agents/skills`.
    let (code, sync_agents) = sb.json(&["skills", "sync", "--host", "agents"]);
    assert_eq!(code, 0, "{sync_agents}");
    assert_eq!(sync_agents["host"], json!("agents"));
    assert_eq!(sync_agents["written"].as_array().unwrap().len(), 7);
    assert!(
        sb.repo
            .join(".agents/skills/backend-service-gate/SKILL.md")
            .is_file()
    );

    // `--host all` syncs both `.claude/skills` and `.agents/skills`.
    let (code, sync_both) = sb.json(&["skills", "sync", "--host", "all"]);
    assert_eq!(code, 0, "{sync_both}");
    assert_eq!(sync_both["host"], json!("all"));
    assert_eq!(sync_both["dirs"].as_array().unwrap().len(), 2);
    assert_eq!(sync_both["targets"].as_array().unwrap().len(), 2);

    // `doctor` reports skills_sync warning for the modified skill, while proof_readiness & github_board stay ok on Rust.
    let (code, doc) = sb.json(&["doctor"]);
    assert_eq!(code, 0, "{doc}");
    assert_eq!(doctor_check(&doc, "skills_sync")["ok"], json!(false));
    assert_eq!(doctor_check(&doc, "skills_sync")["required"], json!(false));
    assert_eq!(doctor_check(&doc, "proof_readiness")["ok"], json!(true));
    assert_eq!(doctor_check(&doc, "github_board")["ok"], json!(true));

    // Malformed `.autofactory/config.toml` must fail `aaf skills sync`, not silently fall back to `auto`.
    write(
        &sb.repo.join(".autofactory/config.toml"),
        "[project]\nunknown_key = 1\n",
    );
    let (code, bad_cfg) = sb.json(&["skills", "sync"]);
    assert_eq!(code, 1, "{bad_cfg}");
}

#[test]
fn init_with_skills_on_auto_repo_prints_hint_and_doctor_advisories_work() {
    let sb = Sandbox::new();
    let out = sb.aaf(&["init", "--with-skills", "--host", "all"]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("skills: profile=auto host=all"), "{text}");
    assert!(text.contains("aaf skills sync --profile <stack>"), "{text}");
    assert!(sb.repo.join(".claude/skills/qa-review/SKILL.md").is_file());
    assert!(sb.repo.join(".agents/skills/qa-review/SKILL.md").is_file());

    // Enable proof & github in config without denylist / mobile-mcp / allowlist -> doctor warns advisories only.
    write(
        &sb.repo.join(".autofactory/config.toml"),
        "[proof]\nrequired = true\n\n[github]\nenabled = true\n",
    );
    let (code, doc) = sb.json(&["doctor"]);
    assert_eq!(code, 0, "{doc}");
    assert_eq!(doctor_check(&doc, "skills_sync")["ok"], json!(true));
    assert_eq!(doctor_check(&doc, "proof_readiness")["ok"], json!(false));
    assert_eq!(doctor_check(&doc, "github_board")["ok"], json!(false));
}

#[test]
fn init_refused_inside_agent_run_skills_validates_profile_with_only_and_doctor_parses_mcp_json() {
    let sb = Sandbox::new();

    // `aaf init` inside an agent run (AAF_RUN_ID set) must refuse with exit 2.
    let refused_init = sb
        .command(&["--json", "init"])
        .env("AAF_RUN_ID", "run-agent-99")
        .output()
        .unwrap();
    assert_eq!(refused_init.status.code(), Some(2));
    let refused_json: Value = serde_json::from_slice(&refused_init.stdout).unwrap();
    assert_eq!(refused_json["status"], json!("refused"));

    let (init_code, init_out) = sb.json(&["init"]);
    assert_eq!(init_code, 0, "{init_out}");

    // `--profile` must still be validated even when `--only` is passed.
    let (code, bad_profile_only) = sb.json(&[
        "skills",
        "sync",
        "--profile",
        "bogus-stack",
        "--only",
        "aaf-workflow",
    ]);
    assert_eq!(code, 1, "{bad_profile_only}");

    // `doctor` proof_readiness must parse `.mcp.json` structurally, not via substring match.
    write(
        &sb.repo.join(".autofactory/config.toml"),
        "[proof]\nrequired = true\n\n[github]\nenabled = true\n",
    );
    write(&sb.repo.join(".adb-denylist"), "# devices\n");
    write(
        &sb.repo.join(".mcp.json"),
        r#"{"comment":"mobile-mcp MOBILEMCP_DISABLE_TELEMETRY=1","mcpServers":{}}"#,
    );
    // Whitespace-only owner_github_allowlist must still fail github_board check.
    write(
        &sb.home.join(".config/autofactory/config.toml"),
        "[safety]\nowner_github_allowlist = [\"   \"]\n",
    );
    let (code, doc_spoof) = sb.json(&["doctor"]);
    assert_eq!(code, 0, "{doc_spoof}");
    assert_eq!(
        doctor_check(&doc_spoof, "proof_readiness")["ok"],
        json!(false)
    );
    assert_eq!(doctor_check(&doc_spoof, "github_board")["ok"], json!(false));

    // Telemetry flag != "1" still warns; setting it to "1" passes proof_readiness.
    write(
        &sb.repo.join(".mcp.json"),
        r#"{"mcpServers":{"mobile-mcp":{"env":{"MOBILEMCP_DISABLE_TELEMETRY":"0"}}}}"#,
    );
    let (code, doc_zero) = sb.json(&["doctor"]);
    assert_eq!(code, 0, "{doc_zero}");
    assert_eq!(
        doctor_check(&doc_zero, "proof_readiness")["ok"],
        json!(false)
    );

    write(
        &sb.repo.join(".mcp.json"),
        r#"{"mcpServers":{"mobile-mcp":{"env":{"MOBILEMCP_DISABLE_TELEMETRY":"1"}}}}"#,
    );
    let (code, doc_ok) = sb.json(&["doctor"]);
    assert_eq!(code, 0, "{doc_ok}");
    assert_eq!(doctor_check(&doc_ok, "proof_readiness")["ok"], json!(true));
}
