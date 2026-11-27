//! Live quota probes for non-Claude providers (`agy -p=/usage` for Google /
//! Antigravity and `grok usage --json` for Grok CLI) with an in-memory TTL
//! cache so regular Cockpit polling stays fast and read-only mode never spawns.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::ports::AccountStore;
use aaf_engine::db::Db;
use aaf_engine::executors::agy::AgyRunner;
use aaf_engine::executors::registry::{find_descriptor, resolve_binary_with_home};
use serde_json::{Value, json};

use crate::ctx::Ctx;

const FALLBACK_EXHAUSTED_COOLDOWN_SECS: i64 = 3600;

const LIVE_QUOTA_TTL: Duration = Duration::from_secs(180);
const AGY_USAGE_TIMEOUT: Duration = Duration::from_secs(15);
const GROK_USAGE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct QuotaCache {
    fetched_at: Option<Instant>,
    payload: Option<Value>,
}

static LIVE_QUOTA_CACHE: OnceLock<Mutex<QuotaCache>> = OnceLock::new();
static LIVE_QUOTA_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

fn cache() -> &'static Mutex<QuotaCache> {
    LIVE_QUOTA_CACHE.get_or_init(|| Mutex::new(QuotaCache::default()))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn run_cmd_bounded(bin: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", bin.display()))?;
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timed out after {}s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("wait failed: {e}")),
        }
    };
    let mut out = String::new();
    let mut err = String::new();
    if let Some(mut s) = stdout.take() {
        let _ = s.read_to_string(&mut out);
    }
    if let Some(mut s) = stderr.take() {
        let _ = s.read_to_string(&mut err);
    }
    if status.success() {
        Ok(out)
    } else {
        let detail = if !err.trim().is_empty() {
            err.trim()
        } else {
            out.trim()
        };
        Err(detail.chars().take(240).collect())
    }
}

/// Converts an RFC3339 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ` or with `.fracZ`)
/// to Unix epoch milliseconds. Returns `None` for invalid or sentinel (`<= 0`) timestamps.
pub(crate) fn parse_rfc3339_utc_ms(raw: &str) -> Option<i64> {
    let s = raw.trim().strip_suffix('Z')?;
    let (date, time_raw) = s.split_once('T')?;
    let time = time_raw.split('.').next()?;
    let mut d = date.split('-');
    let y: i64 = d.next()?.parse().ok()?;
    let m: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    let mut t = time.split(':');
    let hh: i64 = t.next()?.parse().ok()?;
    let mm: i64 = t.next()?.parse().ok()?;
    let ss: i64 = t.next()?.parse().ok()?;
    if !(1970..=2100).contains(&y)
        || !(1..=12).contains(&m)
        || !(1..=31).contains(&day)
        || hh > 23
        || mm > 59
        || ss > 59
    {
        return None;
    }
    // Howard Hinnant's civil-from-days inverse algorithm (UTC).
    let y_adj = if m <= 2 { y - 1 } else { y };
    let era = y_adj.div_euclid(400);
    let yoe = y_adj - era * 400;
    let m_adj = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * m_adj + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hh * 3600 + mm * 60 + ss;
    (secs > 0).then_some(secs * 1000)
}

fn format_remain_human(reset_at_ms: i64, now_epoch_ms: i64) -> String {
    let diff = (reset_at_ms - now_epoch_ms) / 1000;
    if diff <= 0 {
        return String::new();
    }
    let days = diff / 86_400;
    let hours = (diff % 86_400) / 3600;
    let mins = (diff % 3600) / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins:02}m")
    } else {
        format!("{mins}m")
    }
}

/// Parses tab-separated output from `agy -p=/usage --mode plan`.
pub(crate) fn parse_agy_usage_output(raw: &str, fetched_ms: i64) -> Option<Value> {
    let mut rem5: Option<i64> = None;
    let mut reset5_ms: Option<i64> = None;
    let mut rem7: Option<i64> = None;
    let mut reset7_ms: Option<i64> = None;
    for line in raw.lines() {
        let cols: Vec<&str> = line.split('\t').map(str::trim).collect();
        if cols.len() < 3 {
            continue;
        }
        let family = cols[0].to_ascii_lowercase();
        if !family.contains("gemini") {
            continue;
        }
        let label = cols[1].to_ascii_lowercase();
        let pct = cols[2].trim_end_matches('%').trim().parse::<i64>().ok();
        let ts_ms = cols.get(3).and_then(|ts| parse_rfc3339_utc_ms(ts));
        if label.contains("five hour") || label.contains("5h") {
            rem5 = pct.map(|v| v.clamp(0, 100));
            reset5_ms = ts_ms;
        } else if label.contains("weekly") || label.contains("7d") {
            rem7 = pct.map(|v| v.clamp(0, 100));
            reset7_ms = ts_ms;
        }
    }
    if rem5.is_none() && rem7.is_none() {
        return None;
    }
    let reset5_human = reset5_ms
        .map(|ms| format_remain_human(ms, fetched_ms))
        .unwrap_or_default();
    let reset7_human = reset7_ms
        .map(|ms| format_remain_human(ms, fetched_ms))
        .unwrap_or_default();
    Some(json!({
        "live": true,
        "source": "agy_usage",
        "remainingPct": rem5,
        "resetAtMs": reset5_ms,
        "resetRemainHuman": reset5_human,
        "weeklyRemainingPct": rem7,
        "weeklyResetAtMs": reset7_ms,
        "weeklyResetFormatted": reset7_human,
        "fetchedAtMs": fetched_ms,
    }))
}

fn resolve_agy_bin(ctx: &Ctx) -> Option<PathBuf> {
    AgyRunner::resolve_binary(&ctx.path_var).or_else(|| {
        let fallback = ctx.home.join(".local/bin/agy");
        fallback.is_file().then_some(fallback)
    })
}

fn probe_antigravity_quota(ctx: &Ctx) -> Value {
    let fetched_ms = now_ms();
    let Some(bin) = resolve_agy_bin(ctx) else {
        return json!({ "live": false, "reason": "not_installed" });
    };
    match run_cmd_bounded(&bin, &["-p=/usage", "--mode", "plan"], AGY_USAGE_TIMEOUT) {
        Ok(out) => parse_agy_usage_output(&out, fetched_ms).unwrap_or_else(|| {
            json!({ "live": false, "reason": "parse_failed", "fetchedAtMs": fetched_ms })
        }),
        Err(err) => json!({ "live": false, "reason": "error", "error": err, "fetchedAtMs": fetched_ms }),
    }
}

fn parse_grok_usage_json(raw: &str, fetched_ms: i64) -> Option<Value> {
    let val: Value = serde_json::from_str(raw.trim()).ok()?;
    let rem5 = val
        .get("remainingPct")
        .or_else(|| val.get("fiveHourRemainingPct"))
        .or_else(|| val.get("five_hour_remaining_pct"))
        .and_then(Value::as_i64)
        .map(|v| v.clamp(0, 100));
    let rem7 = val
        .get("weeklyRemainingPct")
        .or_else(|| val.get("weekly_remaining_pct"))
        .and_then(Value::as_i64)
        .map(|v| v.clamp(0, 100));
    if rem5.is_none() && rem7.is_none() {
        return None;
    }
    Some(json!({
        "live": true,
        "source": "grok_usage",
        "remainingPct": rem5,
        "weeklyRemainingPct": rem7,
        "fetchedAtMs": fetched_ms,
    }))
}

fn probe_grok_quota(ctx: &Ctx) -> Value {
    let fetched_ms = now_ms();
    let bin = find_descriptor("grok")
        .and_then(|d| resolve_binary_with_home(d, &ctx.path_var, Some(&ctx.home)));
    let Some(bin) = bin else {
        return json!({ "live": false, "reason": "not_installed" });
    };
    match run_cmd_bounded(&bin, &["usage", "--json"], GROK_USAGE_TIMEOUT) {
        Ok(out) => parse_grok_usage_json(&out, fetched_ms).unwrap_or_else(|| {
            json!({ "live": false, "reason": "tui_only", "fetchedAtMs": fetched_ms })
        }),
        Err(err) => {
            let reason = if err.to_ascii_lowercase().contains("/usage") {
                "tui_only"
            } else {
                "unavailable"
            };
            json!({ "live": false, "reason": reason, "fetchedAtMs": fetched_ms })
        }
    }
}

fn probe_and_store(ctx: &Ctx) -> Value {
    let ctx_grok = ctx.clone();
    let grok_handle = std::thread::spawn(move || probe_grok_quota(&ctx_grok));
    let agy = probe_antigravity_quota(ctx);
    let grok = grok_handle
        .join()
        .unwrap_or_else(|_| json!({ "live": false, "reason": "unavailable" }));
    let payload = json!({
        "antigravity": agy,
        "grok": grok,
    });
    if let Ok(mut guard) = cache().lock() {
        guard.fetched_at = Some(Instant::now());
        guard.payload = Some(payload.clone());
    }
    payload
}

/// Fetches cached or freshly probed live `/usage` quotas for Google
/// (Antigravity / Gemini) and Grok CLI. Never spawns subprocesses in `read_only`
/// mode; on cold non-refresh reads, warms the cache asynchronously so HTTP
/// latency stays near-zero; on `force_refresh`, probes `agy` and `grok` in parallel.
pub(super) fn fetch_provider_quotas(ctx: &Ctx, read_only: bool, force_refresh: bool) -> Value {
    let (warm, stale_snapshot) = match cache().lock() {
        Ok(guard) => {
            let is_warm = guard
                .fetched_at
                .is_some_and(|at| at.elapsed() < LIVE_QUOTA_TTL);
            (is_warm, guard.payload.clone())
        }
        Err(_) => (false, None),
    };
    if !force_refresh && warm {
        return stale_snapshot.unwrap_or_else(|| json!({}));
    }
    if read_only {
        return stale_snapshot.unwrap_or_else(|| json!({}));
    }
    if !force_refresh {
        if LIVE_QUOTA_IN_FLIGHT
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let bg_ctx = ctx.clone();
            std::thread::spawn(move || {
                let _ = probe_and_store(&bg_ctx);
                LIVE_QUOTA_IN_FLIGHT.store(false, Ordering::Release);
            });
        }
        return stale_snapshot.unwrap_or_else(|| json!({}));
    }
    probe_and_store(ctx)
}

/// Syncs a live-exhausted non-Claude provider quota (`remainingPct <= 0` or
/// `weeklyRemainingPct <= 0`) into `aaf.db` cooldowns without shortening longer ones.
pub(super) fn sync_live_provider_cooldown(
    db: &Db,
    discovered: &[AccountProfile],
    provider: ProviderKind,
    quota: Option<&Value>,
    now_secs: i64,
) -> usize {
    let Some(q) = quota.filter(|v| v["live"].as_bool() == Some(true)) else {
        return 0;
    };
    let rem5_zero = q["remainingPct"].as_i64().is_some_and(|v| v <= 0);
    let rem7_zero = q["weeklyRemainingPct"].as_i64().is_some_and(|v| v <= 0);
    if !rem5_zero && !rem7_zero {
        return 0;
    }
    let r5 = if rem5_zero {
        q["resetAtMs"].as_i64().unwrap_or(0)
    } else {
        0
    };
    let r7 = if rem7_zero {
        q["weeklyResetAtMs"].as_i64().unwrap_or(0)
    } else {
        0
    };
    let now_ms = now_secs.saturating_mul(1000);
    let until_ms = r5.max(r7);
    let missing_reset = (rem5_zero && r5 <= 0) || (rem7_zero && r7 <= 0);
    let until_secs = if until_ms > now_ms {
        until_ms.saturating_add(999) / 1000
    } else if missing_reset {
        now_secs.saturating_add(FALLBACK_EXHAUSTED_COOLDOWN_SECS)
    } else {
        return 0;
    };
    let Ok(accounts) = db.list_accounts() else {
        return 0;
    };
    let existing: HashMap<String, Option<i64>> = accounts
        .into_iter()
        .map(|s| (s.profile.id, s.cooldown_until))
        .collect();
    let mut synced = 0usize;
    for acct in discovered.iter().filter(|a| a.provider == provider) {
        let cur_until = existing.get(&acct.id).copied().flatten().unwrap_or(0);
        if until_secs > cur_until && db.set_cooldown(&acct.id, Some(until_secs)).is_ok() {
            synced += 1;
        }
    }
    synced
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_live_agy_usage_tsv_and_utc_timestamps() {
        let sample = "\
Gemini Models\tWeekly Limit Remaining\t84%\t2026-12-03T07:07:12Z\n\
Gemini Models\tFive Hour Limit Remaining\t85%\t2026-11-27T03:07:12Z\n\
Claude and GPT models\tWeekly Limit Remaining\t100%\t1970-01-01T00:00:00Z\n";
        let now = parse_rfc3339_utc_ms("2026-11-27T01:07:12Z").unwrap();
        let parsed = parse_agy_usage_output(sample, now).unwrap();
        assert_eq!(parsed["live"].as_bool(), Some(true));
        assert_eq!(parsed["remainingPct"].as_i64(), Some(85));
        assert_eq!(parsed["weeklyRemainingPct"].as_i64(), Some(84));
        assert_eq!(parsed["resetRemainHuman"].as_str(), Some("2h 00m"));
        assert_eq!(parsed["weeklyResetFormatted"].as_str(), Some("6d 6h"));
    }

    #[test]
    fn rejects_epoch_zero_sentinel_timestamp() {
        assert_eq!(parse_rfc3339_utc_ms("1970-01-01T00:00:00Z"), None);
        assert!(parse_rfc3339_utc_ms("2026-11-27T03:07:12Z").unwrap() > 0);
    }
}
