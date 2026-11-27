#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

fn mk_profile(id: &str, provider: ProviderKind, name: &str, dir: &Path) -> AccountProfile {
    AccountProfile {
        id: id.into(),
        provider,
        config_dir: dir.to_string_lossy().into_owned(),
        name: name.into(),
    }
}

#[test]
fn sync_live_exhausted_cooldowns_only_syncs_live_anthropic_and_never_shortens() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    let p_claude_exhausted = mk_profile(
        "claude_code-a",
        ProviderKind::ClaudeCode,
        "finos_toan",
        tmp.path(),
    );
    let p_claude_long_cd = mk_profile(
        "claude_code-b",
        ProviderKind::ClaudeCode,
        "finos_max",
        tmp.path(),
    );
    let p_claude_ok = mk_profile(
        "claude_code-c",
        ProviderKind::ClaudeCode,
        "finos_linh",
        tmp.path(),
    );
    let p_google = mk_profile(
        "antigravity-g",
        ProviderKind::Antigravity,
        "gemini",
        tmp.path(),
    );
    for p in [
        &p_claude_exhausted,
        &p_claude_long_cd,
        &p_claude_ok,
        &p_google,
    ] {
        db.upsert_account(p).unwrap();
    }
    let now = 1_700_000_000i64;
    // Pre-existing longer cooldown (e.g. 24h) must not be shortened to 5h reset.
    db.set_cooldown(&p_claude_long_cd.id, Some(now + 86_400))
        .unwrap();

    let sw = json!({
        "profiles": [
            {
                "id": "finos_toan",
                "vendor": "anthropic",
                "remainingPct": 0,
                "weeklyRemainingPct": 55,
                "resetAtMs": (now + 3600) * 1000,
                "quota": { "live": true }
            },
            {
                "id": "finos_max",
                "vendor": "anthropic",
                "remainingPct": 0,
                "resetAtMs": (now + 1800) * 1000,
                "quota": { "live": true }
            },
            {
                "id": "finos_linh",
                "vendor": "anthropic",
                "remainingPct": 0,
                "resetAtMs": (now + 3600) * 1000,
                "quota": { "live": false }
            },
            {
                "id": "gemini",
                "vendor": "google",
                "remainingPct": 0,
                "resetAtMs": (now + 3600) * 1000,
                "quota": { "live": true }
            }
        ]
    });
    let discovered = vec![
        p_claude_exhausted.clone(),
        p_claude_long_cd.clone(),
        p_claude_ok.clone(),
        p_google.clone(),
    ];
    let synced = sync_live_exhausted_cooldowns(&db, &discovered, Some(&sw), now);
    assert_eq!(synced, 1);

    let rows: HashMap<_, _> = db
        .list_accounts()
        .unwrap()
        .into_iter()
        .map(|s| (s.profile.id, s.cooldown_until))
        .collect();
    assert_eq!(rows[&p_claude_exhausted.id], Some(now + 3600));
    assert_eq!(rows[&p_claude_long_cd.id], Some(now + 86_400));
    assert_eq!(rows[&p_claude_ok.id], None);
    assert_eq!(rows[&p_google.id], None);
}

#[test]
fn merge_repo_assignments_deduplicates_and_scopes() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_a = tmp.path().join("repo-a");
    let repo_b = tmp.path().join("repo-b");
    fs::create_dir_all(&repo_a).unwrap();
    fs::create_dir_all(&repo_b).unwrap();
    let sw = json!({
        "repoAssignments": [
            { "name": "repo-a", "path": repo_a.to_string_lossy(), "account": "finos_toan", "pinnedInFile": true },
            { "name": "repo-b", "path": repo_b.to_string_lossy(), "account": "", "pinnedInFile": false }
        ]
    });
    let all = merge_repo_assignments(None, None, Some(&sw), &[]);
    assert_eq!(all.len(), 2);
    let scoped = merge_repo_assignments(None, Some(&repo_a), Some(&sw), &[]);
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0]["name"].as_str(), Some("repo-a"));

    let sw_missing = json!({
        "repoAssignments": [
            { "name": "ghost", "path": tmp.path().join("nope").to_string_lossy(), "pathExists": false }
        ]
    });
    assert!(merge_repo_assignments(None, None, Some(&sw_missing), &[]).is_empty());

    // Live `.claude/settings.local.json` with non-Claude account (grok/gemini)
    // must override stale cc-switcher entries and attach `provider`.
    fs::create_dir_all(repo_a.join(".claude")).unwrap();
    fs::write(
        repo_a.join(".claude/settings.local.json"),
        r#"{"account":"grok"}"#,
    )
    .unwrap();
    let discovered = vec![
        mk_profile("grok-1", ProviderKind::Grok, "grok", &repo_b),
        mk_profile(
            "antigravity-1",
            ProviderKind::Antigravity,
            "gemini",
            &repo_b,
        ),
    ];
    let overridden = merge_repo_assignments(None, Some(&repo_a), Some(&sw), &discovered);
    assert_eq!(overridden.len(), 1);
    assert_eq!(overridden[0]["account"].as_str(), Some("grok"));
    assert_eq!(overridden[0]["provider"].as_str(), Some("grok"));
}

#[test]
fn sync_skips_stale_mismatch_expired_reset_and_uses_fallback_when_reset_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    let p_stale = mk_profile(
        "claude_code-s",
        ProviderKind::ClaudeCode,
        "acc_s",
        tmp.path(),
    );
    let p_mis = mk_profile(
        "claude_code-m",
        ProviderKind::ClaudeCode,
        "acc_m",
        tmp.path(),
    );
    let p_exp = mk_profile(
        "claude_code-e",
        ProviderKind::ClaudeCode,
        "acc_e",
        tmp.path(),
    );
    let p_fb = mk_profile(
        "claude_code-f",
        ProviderKind::ClaudeCode,
        "acc_f",
        tmp.path(),
    );
    for p in [&p_stale, &p_mis, &p_exp, &p_fb] {
        db.upsert_account(p).unwrap();
    }
    let now = 1_700_000_000i64;
    let sw = json!({
        "profiles": [
            { "id": "acc_s", "vendor": "anthropic", "remainingPct": 0, "quotaStale": true, "resetAtMs": (now + 3600) * 1000, "quota": { "live": true } },
            { "id": "acc_m", "vendor": "anthropic", "remainingPct": 0, "accountMismatch": { "actual": "x", "expected": "y" }, "resetAtMs": (now + 3600) * 1000, "quota": { "live": true } },
            { "id": "acc_e", "vendor": "anthropic", "remainingPct": 0, "resetAtMs": (now - 10) * 1000, "quota": { "live": true } },
            { "id": "acc_f", "vendor": "anthropic", "remainingPct": 0, "quota": { "live": true } }
        ]
    });
    let synced = sync_live_exhausted_cooldowns(
        &db,
        &[p_stale.clone(), p_mis.clone(), p_exp.clone(), p_fb.clone()],
        Some(&sw),
        now,
    );
    assert_eq!(synced, 1);
    let rows: HashMap<_, _> = db
        .list_accounts()
        .unwrap()
        .into_iter()
        .map(|s| (s.profile.id, s.cooldown_until))
        .collect();
    assert_eq!(rows[&p_stale.id], None);
    assert_eq!(rows[&p_mis.id], None);
    assert_eq!(rows[&p_exp.id], None);
    assert_eq!(rows[&p_fb.id], Some(now + FALLBACK_EXHAUSTED_COOLDOWN_SECS));
}

#[test]
fn do_account_clear_all_clears_active_cooldowns_only() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    let dir_a = tmp.path().join("a");
    let dir_b = tmp.path().join("b");
    fs::create_dir_all(&dir_a).unwrap();
    fs::create_dir_all(&dir_b).unwrap();
    let pa = mk_profile("claude_code-a", ProviderKind::ClaudeCode, "a", &dir_a);
    let pb = mk_profile("grok-b", ProviderKind::Grok, "b", &dir_b);
    db.upsert_account(&pa).unwrap();
    db.upsert_account(&pb).unwrap();
    let future = SystemClock.now() + 3600;
    db.set_cooldown(&pa.id, Some(future)).unwrap();
    db.set_cooldown(&pb.id, Some(future)).unwrap();
    let ctx = Ctx {
        cwd: tmp.path().to_path_buf(),
        home: tmp.path().to_path_buf(),
        data_dir: tmp.path().join("data"),
        worktrees_dir: tmp.path().join("wt"),
        config_dir: tmp.path().join("cfg"),
        path_var: Default::default(),
        run_id: None,
        stdin_is_tty: false,
        aaf_env: Vec::new(),
    };
    let res = do_account_clear_all(&db, &ctx, tmp.path()).unwrap();
    assert_eq!(res["cleared"].as_u64(), Some(2));
    assert!(
        db.list_accounts()
            .unwrap()
            .iter()
            .all(|s| s.cooldown_until.is_none())
    );
}

#[test]
fn parses_live_agy_usage_tsv_and_utc_timestamps() {
    let sample = "\
Gemini Models\tWeekly Limit Remaining\t84%\t2026-12-03T07:07:12Z\n\
Gemini Models\tFive Hour Limit Remaining\t85%\t2026-11-27T03:07:12Z\n\
Claude and GPT models\tWeekly Limit Remaining\t100%\t1970-01-01T00:00:00Z\n";
    let now = live_quota::parse_rfc3339_utc_ms("2026-11-27T01:07:12Z").unwrap();
    let parsed = live_quota::parse_agy_usage_output(sample, now).unwrap();
    assert_eq!(parsed["live"].as_bool(), Some(true));
    assert_eq!(parsed["remainingPct"].as_i64(), Some(85));
    assert_eq!(parsed["weeklyRemainingPct"].as_i64(), Some(84));
    assert_eq!(parsed["resetRemainHuman"].as_str(), Some("2h 00m"));
    assert_eq!(parsed["weeklyResetFormatted"].as_str(), Some("6d 6h"));
}

#[test]
fn parses_live_agy_usage_json_with_groups() {
    let sample = r#"{
        "status": "SUCCESS",
        "command": {
            "name": "usage",
            "data": {
                "groups": [
                    {
                        "name": "Gemini Models",
                        "buckets": [
                            {"window": "weekly", "remaining_fraction": 0.836, "reset_time": "2026-12-03T07:07:12Z"},
                            {"window": "5h", "remaining_fraction": 0.824, "reset_time": "2026-11-27T03:07:12Z"}
                        ]
                    },
                    {
                        "name": "Claude and GPT models",
                        "buckets": [
                            {"window": "weekly", "remaining_fraction": 1, "reset_time": "1970-01-01T00:00:00Z"},
                            {"window": "5h", "remaining_fraction": 1, "reset_time": "1970-01-01T00:00:00Z"}
                        ]
                    }
                ]
            }
        }
    }"#;
    let now = live_quota::parse_rfc3339_utc_ms("2026-11-27T01:07:12Z").unwrap();
    let parsed = live_quota::parse_agy_usage_output(sample, now).unwrap();
    assert_eq!(parsed["live"].as_bool(), Some(true));
    assert_eq!(parsed["remainingPct"].as_i64(), Some(82));
    assert_eq!(parsed["weeklyRemainingPct"].as_i64(), Some(84));
    assert_eq!(parsed["groups"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        parsed["groups"][1]["name"].as_str(),
        Some("Claude and GPT models")
    );
    assert_eq!(parsed["groups"][1]["remainingPct"].as_i64(), Some(100));
}

#[test]
fn rejects_epoch_zero_sentinel_timestamp() {
    assert_eq!(
        live_quota::parse_rfc3339_utc_ms("1970-01-01T00:00:00Z"),
        None
    );
    assert!(live_quota::parse_rfc3339_utc_ms("2026-11-27T03:07:12Z").unwrap() > 0);
}

#[test]
fn parses_live_grok_pty_usage_output() {
    let sample = "Session usage: no model calls yet in this session.\n\
        minimal · /help ❯Grok4.7\n\
        Weekly limit: 6%\n\
        Next reset: December 2, 11:40\n";
    let parsed = live_quota::parse_grok_pty_usage(sample, 1_700_000_000_000).unwrap();
    assert_eq!(parsed["live"].as_bool(), Some(true));
    assert_eq!(parsed["weeklyRemainingPct"].as_i64(), Some(94));
    assert_eq!(
        parsed["weeklyResetFormatted"].as_str(),
        Some("December 2, 11:40")
    );
    assert!(parsed["remainingPct"].is_null());

    // Missing `%` must NOT be treated as a percentage (prevents false 0% cooldown).
    assert!(live_quota::parse_grok_pty_usage("Weekly limit: 500\n", 1_700_000_000_000).is_none());
    // Decimal `%` supported and 99.6% used does not prematurely report 0% remaining.
    let near_cap =
        live_quota::parse_grok_pty_usage("Weekly limit: 99.6%\n", 1_700_000_000_000).unwrap();
    assert_eq!(near_cap["weeklyRemainingPct"].as_i64(), Some(1));
}

#[test]
fn rejects_negative_rfc3339_fields_and_negative_remaining_fraction() {
    assert_eq!(
        live_quota::parse_rfc3339_utc_ms("2026-11-27T-5:-10:-20Z"),
        None
    );
    assert_eq!(
        live_quota::parse_rfc3339_utc_ms("2026-11-27T03:07:12:99Z"),
        None
    );
    let neg_frac = r#"{
        "command": { "data": { "groups": [{
            "name": "Gemini Models",
            "buckets": [
                {"window": "5h", "remaining_fraction": -1.0, "reset_time": "2026-11-27T03:07:12Z"}
            ]
        }]}}
    }"#;
    assert!(live_quota::parse_agy_usage_output(neg_frac, 1_700_000_000_000).is_none());
}

#[test]
fn merge_repo_assignments_clears_stale_switcher_entry_when_local_file_unpinned() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_a = tmp.path().join("repo-a");
    fs::create_dir_all(repo_a.join(".claude")).unwrap();
    // `.claude/settings.local.json` exists with no `account` key (after removal)
    fs::write(repo_a.join(".claude/settings.local.json"), "{}\n").unwrap();
    let sw = json!({
        "repoAssignments": [
            { "name": "repo-a", "path": repo_a.to_string_lossy(), "account": "finos_toan", "pinnedInFile": true }
        ]
    });
    let discovered = vec![mk_profile(
        "claude_code-1",
        ProviderKind::ClaudeCode,
        "finos_toan",
        &repo_a,
    )];
    let merged = merge_repo_assignments(None, Some(&repo_a), Some(&sw), &discovered);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0]["account"].as_str(), Some(""));
    assert_eq!(merged[0]["pinnedInFile"].as_bool(), Some(false));
    assert!(merged[0].get("provider").is_none());
}
