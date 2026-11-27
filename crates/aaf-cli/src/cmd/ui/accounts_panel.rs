//! Accounts panel backend: native AAF account pool (Claude, Google/Antigravity,
//! Grok, Codex, OpenCode), live Claude 5h/7d OAuth quota sync into AAF cooldowns,
//! native clear-all cooldowns, and merged Repo <-> Account assignment.

mod live_quota;
mod switcher;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::Path;

use aaf_domain::account::{AccountProfile, ProviderKind, default_account_name};
use aaf_domain::ports::{AccountStore, Clock};
use aaf_engine::accounts::{DiscoveryEnv, Lease, Router, binary_on_path_in, discover};
use aaf_engine::db::Db;
use serde_json::{Value, json};

use super::extras::{known_repos, read_repo_local_account};
use crate::cmd::board::store::open_db;
use crate::ctx::{Ctx, SystemClock};

const FIVE_HOURS_SECS: i64 = 5 * 3600;
const SEVEN_DAYS_SECS: i64 = 7 * 24 * 3600;
const FALLBACK_EXHAUSTED_COOLDOWN_SECS: i64 = 3600;

fn native_repo_assignments(db: Option<&Db>, scoped_repo: Option<&Path>) -> Vec<Value> {
    let canon_scope = scoped_repo.map(|s| fs::canonicalize(s).unwrap_or_else(|_| s.to_path_buf()));
    known_repos(db, scoped_repo)
        .into_iter()
        .filter_map(|v| {
            let path_str = v["repo_root"].as_str()?.to_string();
            let p = Path::new(&path_str);
            if let Some(ref sc) = canon_scope {
                let cp = fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
                if cp != *sc {
                    return None;
                }
            }
            let name = v["name"].as_str().unwrap_or(&path_str).to_string();
            let account = read_repo_local_account(p);
            Some(json!({
                "name": name,
                "path": path_str,
                "account": account.clone().unwrap_or_default(),
                "pinnedInFile": account.is_some(),
            }))
        })
        .collect()
}

fn merge_repo_assignments(
    db: Option<&Db>,
    scoped_repo: Option<&Path>,
    sw: Option<&Value>,
) -> Vec<Value> {
    let canon_scope = scoped_repo.map(|s| fs::canonicalize(s).unwrap_or_else(|_| s.to_path_buf()));
    let mut by_key: BTreeMap<String, Value> = BTreeMap::new();
    let canon_key = |raw: &str| {
        let p = Path::new(raw);
        fs::canonicalize(p)
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_else(|_| raw.trim_end_matches('/').to_string())
    };
    if let Some(arr) = sw
        .and_then(|v| v.get("repoAssignments"))
        .and_then(Value::as_array)
    {
        for row in arr {
            let Some(path_str) = row["path"].as_str() else {
                continue;
            };
            if row["pathExists"].as_bool() == Some(false) || !Path::new(path_str).exists() {
                continue;
            }
            let key = canon_key(path_str);
            if let Some(ref sc) = canon_scope
                && Path::new(&key) != sc.as_path()
            {
                continue;
            }
            by_key.insert(key, row.clone());
        }
    }
    for row in native_repo_assignments(db, scoped_repo) {
        let Some(path_str) = row["path"].as_str() else {
            continue;
        };
        let key = canon_key(path_str);
        by_key.entry(key).or_insert(row);
    }
    let mut rows: Vec<Value> = by_key.into_values().collect();
    rows.sort_by(|a, b| {
        let ka = (
            a["source"].as_str() == Some("app"),
            a["name"].as_str().unwrap_or("").to_ascii_lowercase(),
        );
        let kb = (
            b["source"].as_str() == Some("app"),
            b["name"].as_str().unwrap_or("").to_ascii_lowercase(),
        );
        ka.cmp(&kb)
    });
    rows
}

fn find_unique_claude_account<'a>(
    discovered: &'a [AccountProfile],
    sw_id: &str,
) -> Option<&'a AccountProfile> {
    let key = sw_id.trim();
    if key.is_empty() {
        return None;
    }
    let mut matches = discovered.iter().filter(|a| {
        a.provider == ProviderKind::ClaudeCode
            && (a.id.eq_ignore_ascii_case(key)
                || a.name.eq_ignore_ascii_case(key)
                || default_account_name(&a.config_dir).eq_ignore_ascii_case(key))
    });
    let first = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    Some(first)
}

/// Synchronizes live-exhausted Anthropic OAuth profiles (and live Antigravity
/// `/usage` when 0%) into `aaf.db` cooldowns so workers skip 0% accounts.
pub(crate) fn sync_live_exhausted_cooldowns(
    db: &Db,
    discovered: &[AccountProfile],
    sw: Option<&Value>,
    now_secs: i64,
) -> usize {
    let Some(profiles) = sw.and_then(|v| v.get("profiles")).and_then(Value::as_array) else {
        return 0;
    };
    let Ok(accounts) = db.list_accounts() else {
        return 0;
    };
    let mut existing: HashMap<String, Option<i64>> = accounts
        .into_iter()
        .map(|s| (s.profile.id, s.cooldown_until))
        .collect();
    let now_ms = now_secs.saturating_mul(1000);
    let mut synced = 0usize;
    for p in profiles {
        if p["vendor"].as_str() != Some("anthropic")
            || p["quota"]["live"].as_bool() != Some(true)
            || p["quotaStale"].as_bool() == Some(true)
            || !p["accountMismatch"].is_null()
        {
            continue;
        }
        let rem5_zero = p["remainingPct"].as_i64().is_some_and(|v| v <= 0);
        let rem7_zero = p["weeklyRemainingPct"].as_i64().is_some_and(|v| v <= 0);
        let zero_quota = rem5_zero || rem7_zero;
        let frozen = p["isFrozen"].as_bool() == Some(true);
        if !zero_quota && !frozen {
            continue;
        }
        let r5 = if rem5_zero {
            p["resetAtMs"].as_i64().unwrap_or(0)
        } else {
            0
        };
        let r7 = if rem7_zero {
            p["weeklyResetAtMs"].as_i64().unwrap_or(0)
        } else {
            0
        };
        let rf = if frozen {
            p["frozenUntilMs"].as_i64().unwrap_or(0)
        } else {
            0
        };
        let until_ms = rf.max(r5).max(r7);
        let missing_reset = (rem5_zero && r5 <= 0) || (rem7_zero && r7 <= 0);
        let until_secs = if until_ms > now_ms {
            until_ms.saturating_add(999) / 1000
        } else if missing_reset {
            now_secs.saturating_add(FALLBACK_EXHAUSTED_COOLDOWN_SECS)
        } else {
            continue;
        };
        let sw_id = p["id"].as_str().unwrap_or("");
        let Some(acct) = find_unique_claude_account(discovered, sw_id) else {
            continue;
        };
        let cur_until = existing.get(&acct.id).copied().flatten().unwrap_or(0);
        if until_secs > cur_until && db.set_cooldown(&acct.id, Some(until_secs)).is_ok() {
            existing.insert(acct.id.clone(), Some(until_secs));
            synced += 1;
        }
    }
    synced
}

/// Pure read-only account pool snapshot: never creates `leases/` or acquires
/// exclusive locks, never silently zeros DB errors, and preserves DB-only
/// leased/cooldown accounts whose config directories still exist.
fn build_aaf_account_rows(
    db: Option<&Db>,
    ctx: &Ctx,
    discovered: &[AccountProfile],
    max_concurrent: u32,
    now: i64,
) -> Result<Vec<Value>, String> {
    let leases_dir = ctx.data_dir.join("leases");
    let (db_states, tok_5h, tok_7d) = if let Some(db) = db {
        let states = db.list_accounts().map_err(|e| e.to_string())?;
        let t5 = db
            .tokens_since_all(now.saturating_sub(FIVE_HOURS_SECS))
            .map_err(|e| e.to_string())?;
        let t7 = db
            .tokens_since_all(now.saturating_sub(SEVEN_DAYS_SECS))
            .map_err(|e| e.to_string())?;
        (states, t5, t7)
    } else {
        (Vec::new(), HashMap::new(), HashMap::new())
    };
    let state_by_id: HashMap<String, _> = db_states
        .iter()
        .map(|s| (s.profile.id.clone(), s))
        .collect();
    let mut seen_ids = HashSet::new();
    let eff_conc_for = |provider: ProviderKind| -> u32 {
        if provider == ProviderKind::Antigravity {
            1
        } else {
            max_concurrent
        }
    };
    let row_for = |p: &AccountProfile, cooldown_until: Option<i64>, leased: Option<bool>| -> Value {
        let in_cd = cooldown_until.is_some_and(|u| u > now);
        let (st, until) = if in_cd {
            ("cooldown", cooldown_until)
        } else if leased.unwrap_or_else(|| {
            Lease::all_slots_leased_ro(&leases_dir, &p.id, eff_conc_for(p.provider))
        }) {
            ("leased", None)
        } else {
            ("available", None)
        };
        json!({
            "id": p.id,
            "name": p.name,
            "provider": p.provider,
            "config_dir": p.config_dir,
            "state": st,
            "cooldown_until": until,
            "tokens_5h": tok_5h.get(&p.id).copied().unwrap_or(0),
            "tokens_7d": tok_7d.get(&p.id).copied().unwrap_or(0),
        })
    };
    let mut rows = Vec::with_capacity(discovered.len());
    for p in discovered {
        seen_ids.insert(p.id.clone());
        let cd = state_by_id.get(&p.id).and_then(|s| s.cooldown_until);
        rows.push(row_for(p, cd, None));
    }
    for s in &db_states {
        if seen_ids.contains(&s.profile.id) || !Path::new(&s.profile.config_dir).is_dir() {
            continue;
        }
        let in_cd = s.in_cooldown(now);
        let leased = if in_cd {
            false
        } else {
            Lease::all_slots_leased_ro(&leases_dir, &s.profile.id, eff_conc_for(s.profile.provider))
        };
        if in_cd || leased {
            rows.push(row_for(&s.profile, s.cooldown_until, Some(leased)));
        }
    }
    Ok(rows)
}

/// Clear cooldowns for every discovered/existing account currently in cooldown in `aaf.db`.
pub(super) fn do_account_clear_all(
    db: &Db,
    ctx: &Ctx,
    config_base: &Path,
) -> Result<Value, String> {
    let cfg = ctx.load_config(config_base).map_err(|e| e.message)?;
    let lookup = |name: &str| binary_on_path_in(&ctx.path_var, name);
    let discovered = discover(
        &cfg.global.accounts,
        &DiscoveryEnv {
            home: ctx.home.clone(),
            binary_on_path: &lookup,
        },
    );
    let visible_ids: HashSet<&str> = discovered.iter().map(|p| p.id.as_str()).collect();
    let clock = SystemClock;
    let router = Router::new(
        db,
        db,
        &clock,
        ctx.data_dir.join("leases"),
        cfg.global.accounts.max_concurrent_per_account,
    );
    router.sync(&discovered).map_err(|e| e.to_string())?;
    let now = clock.now();
    let mut cleared_ids = Vec::new();
    for state in db.list_accounts().map_err(|e| e.to_string())? {
        let visible = visible_ids.contains(state.profile.id.as_str())
            || Path::new(&state.profile.config_dir).is_dir();
        if visible && state.in_cooldown(now) {
            db.set_cooldown(&state.profile.id, None)
                .map_err(|e| e.to_string())?;
            cleared_ids.push(state.profile.id);
        }
    }
    Ok(json!({
        "action": "account_clear_all",
        "cleared": cleared_ids.len(),
        "account_ids": cleared_ids,
    }))
}

/// Build combined accounts panel response for `GET /api/accounts`.
pub(super) fn read_accounts_panel(
    db: Option<&Db>,
    ctx: &Ctx,
    config_base: &Path,
    scoped_repo: Option<&Path>,
    read_only: bool,
    refresh: bool,
) -> Result<Value, String> {
    if refresh && read_only {
        return Err("refused: live quota refresh is disabled in read-only mode".into());
    }
    let cfg = ctx.load_config(config_base).map_err(|e| e.message)?;
    let lookup = |name: &str| binary_on_path_in(&ctx.path_var, name);
    let discovered = discover(
        &cfg.global.accounts,
        &DiscoveryEnv {
            home: ctx.home.clone(),
            binary_on_path: &lookup,
        },
    );
    let (sw_installed, sw_val, sw_err) = switcher::fetch_switcher_state(ctx, read_only, refresh);
    let provider_quotas = live_quota::fetch_provider_quotas(ctx, read_only, refresh);
    let clock = SystemClock;
    let now = clock.now();
    let max_conc = cfg.global.accounts.max_concurrent_per_account;
    let mut synced_cooldowns = 0usize;
    let rw_db = if refresh && !read_only {
        Some(open_db(ctx).map_err(|e| e.message)?)
    } else {
        None
    };
    if let Some(ref rw) = rw_db {
        let router = Router::new(rw, rw, &clock, ctx.data_dir.join("leases"), max_conc);
        router.sync(&discovered).map_err(|e| e.to_string())?;
        if sw_err.is_empty() {
            synced_cooldowns += sync_live_exhausted_cooldowns(rw, &discovered, sw_val.as_ref(), now);
        }
        synced_cooldowns += live_quota::sync_live_provider_cooldown(
            rw,
            &discovered,
            ProviderKind::Antigravity,
            provider_quotas.get("antigravity"),
            now,
        );
        synced_cooldowns += live_quota::sync_live_provider_cooldown(
            rw,
            &discovered,
            ProviderKind::Grok,
            provider_quotas.get("grok"),
            now,
        );
    }
    let effective_db = rw_db.as_ref().or(db);
    let aaf_accounts = build_aaf_account_rows(effective_db, ctx, &discovered, max_conc, now)?;
    let repo_assignments = merge_repo_assignments(effective_db, scoped_repo, sw_val.as_ref());
    Ok(json!({
        "aaf_accounts": aaf_accounts,
        "provider_quotas": provider_quotas,
        "repo_assignments": repo_assignments,
        "selection_mode": cfg.global.accounts.selection.as_str(),
        "cross_audit_executor": cfg.repo.cross_audit.executor,
        "synced_cooldowns": synced_cooldowns,
        "switcher_installed": sw_installed,
        "switcher": sw_val,
        "switcher_error": sw_err,
    }))
}

/// Dispatch repository account-assignment actions from `POST /api/action`.
pub(super) fn dispatch_cc_action(
    ctx: &Ctx,
    scoped_repo: Option<&Path>,
    action: &str,
    task_id: &str,
    note: &str,
) -> Result<Value, String> {
    switcher::dispatch_repo_action(ctx, scoped_repo, action, task_id, note)
}

#[cfg(test)]
mod tests;
