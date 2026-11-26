//! Helper functions & budget/duration calculations for `studio_tokens`.
//! Kept in a submodule so `studio_tokens.rs` stays comfortably under 600 lines.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use aaf_domain::budget::RootBudget;
use aaf_domain::ports::{KvStore, META_COST_PAUSED_UNTIL, META_FACTORY_FROZEN, TaskStore};
use aaf_domain::run::{RunRecord, RunStatus};
use aaf_domain::task::Task;
use aaf_engine::db::Db;

use super::format::BurnSnapshot;
use super::{DiagnosticsCtx, pct};

const FIVE_MIN_SECS: i64 = 300;
const DAY_SECS: i64 = 86_400;

/// Safe run duration: only falls back to `now` while `Running`, and caps stale
/// running durations at `stale_running_secs` so zombie rows cannot skew totals.
pub(crate) fn safe_run_duration(r: &RunRecord, now: i64, stale_running_secs: u64) -> (u64, bool) {
    let raw = match (r.status, r.ended_at) {
        (_, Some(end)) => u64::try_from(end.saturating_sub(r.started_at)).unwrap_or(0),
        (RunStatus::Running, None) => u64::try_from(now.saturating_sub(r.started_at)).unwrap_or(0),
        (_, None) => 0,
    };
    let is_stale = r.status == RunStatus::Running && raw > stale_running_secs;
    let capped = if is_stale {
        raw.min(stale_running_secs)
    } else {
        raw
    };
    (capped, is_stale)
}

/// Evaluates all three root budget dimensions (`tokens`, `wall_secs`, `cycles`),
/// treating any `0` limit as unlimited (never dividing by zero).
/// Returns `(max_pct, limiting_dimension)`.
pub(super) fn budget_pressure(b: &RootBudget) -> (f64, &'static str) {
    let tok_pct = if b.token_limit > 0 {
        pct(b.tokens_used, b.token_limit)
    } else {
        0.0
    };
    let wall_pct = if b.wall_secs_limit > 0 {
        pct(b.wall_secs_used, b.wall_secs_limit)
    } else {
        0.0
    };
    let cyc_pct = if b.cycles_limit > 0 {
        pct(u64::from(b.cycles_used), u64::from(b.cycles_limit))
    } else {
        0.0
    };
    let mut best = (tok_pct, "tokens");
    if wall_pct > best.0 {
        best = (wall_pct, "wall");
    }
    if cyc_pct > best.0 {
        best = (cyc_pct, "cycles");
    }
    best
}

pub(super) fn top_keys_by_score(map: &HashMap<String, u64>, limit: usize) -> Vec<String> {
    let mut pairs: Vec<(&String, u64)> = map.iter().map(|(k, &v)| (k, v)).collect();
    pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    pairs
        .into_iter()
        .take(limit)
        .map(|(k, _)| k.clone())
        .collect()
}

pub(super) fn bump_max(slot: &mut Option<i64>, ts: i64) {
    *slot = Some(slot.map_or(ts, |cur| cur.max(ts)));
}

pub(super) fn repo_basename(repo_root: &str) -> String {
    Path::new(repo_root)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string()
}

pub(super) fn is_non_coding_aux(executor: &str) -> bool {
    let lower = executor.to_ascii_lowercase();
    super::is_audit_executor(&lower)
        || lower == "plan"
        || lower.starts_with("plan-")
        || lower.starts_with("plan:")
        || lower == "split"
        || lower.starts_with("split-")
        || lower == "sop"
        || lower.starts_with("sop:")
        || lower.starts_with("sop-")
        || lower == "unblock"
        || lower.starts_with("unblock-")
        || lower.contains("proposer")
}

pub(super) fn build_burn_snapshot(
    db: &Db,
    now: i64,
    spike_since: i64,
    dctx: DiagnosticsCtx<'_>,
) -> BurnSnapshot {
    let (tokens_last_5m, _) = db
        .cost_window_usage(now.saturating_sub(FIVE_MIN_SECS))
        .unwrap_or((0, None));
    let (tokens_last_1h, _) = db.cost_window_usage(spike_since).unwrap_or((0, None));
    let (tokens_last_24h, _) = db
        .cost_window_usage(now.saturating_sub(DAY_SECS))
        .unwrap_or((0, None));
    let daily_cap = dctx.cost_guard.daily_token_cap;
    let raw_paused = db
        .get_meta(META_COST_PAUSED_UNTIL)
        .ok()
        .flatten()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|&v| v > 0);
    let cost_paused_until = raw_paused.filter(|&v| v > now && daily_cap > 0);
    let global_frozen = db.get_meta(META_FACTORY_FROZEN).ok().flatten().as_deref() == Some("1");
    let repo_frozen = dctx.repo_root.is_some_and(|root| {
        let key = aaf_fp::fs_private::repo_frozen_key(root);
        matches!(
            db.get_meta(&key).ok().flatten().as_deref(),
            Some("1" | "probe")
        )
    });
    BurnSnapshot {
        tokens_last_5m,
        tokens_last_1h,
        tokens_last_24h,
        daily_cap,
        spike_limit_1h: if daily_cap > 0 && dctx.cost_guard.spike_factor > 0 {
            let raw =
                (u128::from(dctx.cost_guard.spike_factor) * u128::from(daily_cap) / 24).max(1);
            u64::try_from(raw).unwrap_or(u64::MAX)
        } else {
            0
        },
        cost_paused_until,
        cost_paused_stale: raw_paused.is_some() && cost_paused_until.is_none(),
        factory_frozen: global_frozen || repo_frozen,
    }
}

pub(super) fn collect_budget_rows<'a>(
    db: &Db,
    budgets: &'a HashMap<String, RootBudget>,
    task_map: &HashMap<&str, &Task>,
    open_roots: &HashSet<String>,
) -> Vec<(&'a RootBudget, String, f64, &'static str, bool)> {
    let mut rows: Vec<_> = budgets
        .values()
        .filter_map(|b| {
            let (max_p, kind) = budget_pressure(b);
            if !(b.frozen || max_p >= 50.0) {
                return None;
            }
            let title = task_map
                .get(b.root_id.as_str())
                .map(|t| t.title.clone())
                .or_else(|| db.get_task(&b.root_id).ok().map(|t| t.title))
                .unwrap_or_else(|| b.root_id.clone());
            Some((b, title, max_p, kind, open_roots.contains(&b.root_id)))
        })
        .collect();
    rows.sort_by(|a, b| {
        b.4.cmp(&a.4)
            .then_with(|| b.0.frozen.cmp(&a.0.frozen))
            .then_with(|| b.2.total_cmp(&a.2))
            .then_with(|| b.0.tokens_used.cmp(&a.0.tokens_used))
            .then_with(|| a.0.root_id.cmp(&b.0.root_id))
    });
    rows
}
