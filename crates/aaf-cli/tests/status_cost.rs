#![allow(clippy::unwrap_used, clippy::expect_used)]
//! W4c CLI integration tests: USD estimates only appear for Claude accounts marked `billing = "api"`.

#[path = "factory_support.rs"]
mod support;

use std::fs;
use std::process::Command;

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::ports::{AccountStore, RunStore};
use aaf_domain::run::{RunFinish, RunRecord, RunStatus};
use aaf_engine::accounts::discovery::account_id;
use serde_json::json;
use support::Fixture;

fn run_text(fx: &Fixture, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_aaf"))
        .args(args)
        .current_dir(&fx.repo)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", fx.root().join("home"))
        .env("AAF_DATA_DIR", &fx.data)
        .env("AAF_WORKTREE_DIR", &fx.worktrees)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "aaf {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn status_hides_usd_by_default_and_shows_usd_only_for_api_billed_account() {
    let fx = Fixture::new();
    let home = fx.root().join("home");
    let sub_dir = home.join(".claude-max");
    let api_dir = home.join(".claude-api");
    fs::create_dir_all(&sub_dir).unwrap();
    fs::create_dir_all(&api_dir).unwrap();
    let sub_canon = fs::canonicalize(&sub_dir).unwrap();
    let api_canon = fs::canonicalize(&api_dir).unwrap();
    let sub_id = account_id(ProviderKind::ClaudeCode, &sub_canon);
    let api_id = account_id(ProviderKind::ClaudeCode, &api_canon);

    let task_id = fx.add("Check billing cost display");
    {
        let db = fx.db();
        db.upsert_account(&AccountProfile {
            id: sub_id.clone(),
            provider: ProviderKind::ClaudeCode,
            config_dir: sub_canon.to_string_lossy().into_owned(),
            name: "max".to_string(),
        })
        .unwrap();
        db.upsert_account(&AccountProfile {
            id: api_id.clone(),
            provider: ProviderKind::ClaudeCode,
            config_dir: api_canon.to_string_lossy().into_owned(),
            name: "api".to_string(),
        })
        .unwrap();

        // Subscription run: 1M in + 1M out (would be $18.00 if billed as API).
        db.insert_run(&RunRecord {
            id: "r-sub".to_string(),
            task_id: task_id.clone(),
            account_id: Some(sub_id.clone()),
            executor: "claude".to_string(),
            worktree_path: fx.repo.to_string_lossy().into_owned(),
            branch: "aaf/t".to_string(),
            status: RunStatus::Running,
            exit_code: None,
            tokens_in: 0,
            tokens_out: 0,
            started_at: 1000,
            ended_at: None,
        })
        .unwrap();
        db.finish_run(
            "r-sub",
            &RunFinish {
                status: RunStatus::Succeeded,
                exit_code: Some(0),
                tokens_in: 1_000_000,
                tokens_out: 1_000_000,
                ended_at: 1010,
            },
        )
        .unwrap();
    }

    // 1. Default (no [accounts.billing]): estimated_cost_usd is null, cost_basis is "none", no "~$".
    let status_default = fx.aaf(&["status"]);
    assert_eq!(status_default.code, 0, "{}", status_default.stderr);
    assert_eq!(status_default.json["estimated_cost_usd"], json!(null));
    assert_eq!(status_default.json["cost_basis"], json!("none"));
    assert_eq!(status_default.json["tokens_in"], json!(1_000_000));
    assert_eq!(status_default.json["tokens_out"], json!(1_000_000));

    let text_default = run_text(&fx, &["status"]);
    assert!(
        text_default.contains("tokens: 1000000 in + 1000000 out"),
        "{text_default}"
    );
    assert!(
        !text_default.contains("~$"),
        "subscription-only status must omit USD: {text_default}"
    );

    let trace_default = fx.aaf(&["board", "trace", &task_id]);
    assert_eq!(trace_default.code, 0, "{}", trace_default.stderr);
    assert_eq!(
        trace_default.json["trace"]["estimated_cost_usd"],
        json!(null)
    );
    assert_eq!(trace_default.json["trace"]["cost_basis"], json!("none"));
    let trace_text_default = run_text(&fx, &["board", "trace", &task_id]);
    assert!(
        trace_text_default.contains("cost=n/a"),
        "{trace_text_default}"
    );

    // 2. Configure ~/.claude-api = "api", but before any API run exists -> still null / "none".
    let tier1 = home.join(".config/autofactory/config.toml");
    fs::write(
        &tier1,
        "[machine]\nmax_load_percent = 0\n\n[accounts.billing]\n\"~/.claude-api\" = \"api\"\n",
    )
    .unwrap();
    let status_no_api_runs = fx.aaf(&["status"]);
    assert_eq!(status_no_api_runs.code, 0, "{}", status_no_api_runs.stderr);
    assert_eq!(status_no_api_runs.json["estimated_cost_usd"], json!(null));
    assert_eq!(status_no_api_runs.json["cost_basis"], json!("none"));

    // 3. Add an API run: 1M in ($3) + 0.5M out ($7.50) = $10.50 (ignoring the subscription run).
    {
        let db = fx.db();
        db.insert_run(&RunRecord {
            id: "r-api".to_string(),
            task_id: task_id.clone(),
            account_id: Some(api_id),
            executor: "claude".to_string(),
            worktree_path: fx.repo.to_string_lossy().into_owned(),
            branch: "aaf/t".to_string(),
            status: RunStatus::Running,
            exit_code: None,
            tokens_in: 0,
            tokens_out: 0,
            started_at: 1020,
            ended_at: None,
        })
        .unwrap();
        db.finish_run(
            "r-api",
            &RunFinish {
                status: RunStatus::Succeeded,
                exit_code: Some(0),
                tokens_in: 1_000_000,
                tokens_out: 500_000,
                ended_at: 1030,
            },
        )
        .unwrap();
    }

    let status_api = fx.aaf(&["status"]);
    assert_eq!(status_api.code, 0, "{}", status_api.stderr);
    assert_eq!(status_api.json["tokens_in"], json!(2_000_000));
    assert_eq!(status_api.json["tokens_out"], json!(1_500_000));
    assert_eq!(status_api.json["cost_basis"], json!("api_accounts"));
    let usd = status_api.json["estimated_cost_usd"].as_f64().unwrap();
    assert!((usd - 10.5).abs() < 1e-9, "got {usd}");

    let text_api = run_text(&fx, &["status"]);
    assert!(
        text_api.contains("tokens: 2000000 in + 1500000 out (~$10.5000)"),
        "{text_api}"
    );

    let trace_api = fx.aaf(&["board", "trace", &task_id]);
    assert_eq!(trace_api.code, 0, "{}", trace_api.stderr);
    assert_eq!(trace_api.json["trace"]["cost_basis"], json!("api_accounts"));
    let trace_usd = trace_api.json["trace"]["estimated_cost_usd"]
        .as_f64()
        .unwrap();
    assert!((trace_usd - 10.5).abs() < 1e-9, "got {trace_usd}");

    let digest_api = fx.aaf(&["handover", "--summary", "--format", "json"]);
    assert_eq!(digest_api.code, 0, "{}", digest_api.stderr);
    assert_eq!(
        digest_api.json["digest"]["cost_basis"],
        json!("api_accounts")
    );
    let digest_usd = digest_api.json["digest"]["estimated_cost_usd"]
        .as_f64()
        .unwrap();
    assert!((digest_usd - 10.5).abs() < 1e-9, "got {digest_usd}");
}
