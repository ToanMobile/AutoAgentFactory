//! Machine-wide 24-hour token cap (auto-resuming pause) and 1-hour spike
//! detector (hard factory freeze requiring `aaf board unfreeze`).
//!
//! Invariant: pause state is tracked in `META_COST_PAUSED_UNTIL` (and derived
//! from `runs`), and never writes or clears `META_FACTORY_FROZEN`.

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{
    KvStore, META_COST_PAUSED_UNTIL, META_FACTORY_FROZEN, META_FACTORY_UNFROZEN_AT, PortResult,
};
use serde_json::json;

use super::{Scheduler, is_frozen};

const COST_CHECK_CACHE_SECS: i64 = 30;
const HOUR_SECS: i64 = 3600;
const DAY_SECS: i64 = 86_400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::factory) enum CostGate {
    Allow,
    Paused,
    Frozen,
}

fn is_cost_paused_meta(store: &dyn KvStore) -> PortResult<bool> {
    Ok(store.get_meta(META_COST_PAUSED_UNTIL)?.is_some_and(|raw| {
        let trimmed = raw.trim();
        !trimmed.is_empty() && trimmed != "0"
    }))
}

impl Scheduler<'_> {
    /// Evaluates the machine-wide 1h spike freeze and 24h token cap.
    ///
    /// Returns [`CostGate::Allow`] without querying the store when
    /// `daily_token_cap == 0`. Results are cached in `LoopState` for 30 s and
    /// invalidated whenever a worker finishes.
    pub(in crate::factory) fn cost_gate(&mut self) -> PortResult<CostGate> {
        let cfg = self.deps.settings.repo.unblocker.cost_guard;
        if cfg.daily_token_cap == 0 {
            return Ok(CostGate::Allow);
        }

        let now = self.deps.now();
        if let Some(checked_at) = self.state.cost_checked_at
            && now >= checked_at
            && now.saturating_sub(checked_at) < COST_CHECK_CACHE_SECS
        {
            return Ok(if self.state.cost_cached_paused {
                CostGate::Paused
            } else {
                CostGate::Allow
            });
        }
        self.state.cost_checked_at = Some(now);

        if cfg.spike_factor > 0 {
            let unfrozen_at = self
                .deps
                .store
                .get_meta(META_FACTORY_UNFROZEN_AT)?
                .and_then(|raw| raw.trim().parse::<i64>().ok());
            let spike_since = match unfrozen_at {
                Some(ts) => now.saturating_sub(HOUR_SECS).max(ts),
                None => now.saturating_sub(HOUR_SECS),
            };
            let (hour_tokens, _) = self.deps.store.cost_window_usage(spike_since)?;
            let raw_limit =
                (u128::from(cfg.spike_factor) * u128::from(cfg.daily_token_cap) / 24).max(1);
            let limit_1h = u64::try_from(raw_limit).unwrap_or(u64::MAX);
            if hour_tokens > limit_1h {
                if !is_frozen(&*self.deps.store)? {
                    self.deps.store.set_meta(META_FACTORY_FROZEN, "1")?;
                    let payload = json!({
                        "reason": "token_spike",
                        "tokens_last_hour": hour_tokens,
                        "limit_last_hour": limit_1h,
                        "consecutive_fails": 0,
                        "threshold": 0,
                    });
                    self.deps.store.emit(&FactoryEvent::new(
                        EventKind::FactoryFrozen,
                        payload.to_string(),
                        now,
                    ))?;
                }
                self.state.cost_cached_paused = false;
                return Ok(CostGate::Frozen);
            }
        }

        let (day_tokens, earliest) = self
            .deps
            .store
            .cost_window_usage(now.saturating_sub(DAY_SECS))?;
        let was_paused = is_cost_paused_meta(&*self.deps.store)?;
        if day_tokens >= cfg.daily_token_cap {
            let resume_at = earliest.map_or_else(
                || now.saturating_add(HOUR_SECS),
                |ts| ts.saturating_add(DAY_SECS),
            );
            self.deps
                .store
                .set_meta(META_COST_PAUSED_UNTIL, &resume_at.max(1).to_string())?;
            if !was_paused {
                let payload = json!({
                    "tokens_24h": day_tokens,
                    "cap": cfg.daily_token_cap,
                    "resume_at": resume_at,
                });
                self.deps.store.emit(&FactoryEvent::new(
                    EventKind::CostPaused,
                    payload.to_string(),
                    now,
                ))?;
            }
            self.state.cost_cached_paused = true;
            return Ok(CostGate::Paused);
        }

        if was_paused {
            self.deps.store.set_meta(META_COST_PAUSED_UNTIL, "0")?;
            let payload = json!({
                "tokens_24h": day_tokens,
                "cap": cfg.daily_token_cap,
            });
            self.deps.store.emit(&FactoryEvent::new(
                EventKind::CostResumed,
                payload.to_string(),
                now,
            ))?;
        }
        self.state.cost_cached_paused = false;
        Ok(CostGate::Allow)
    }
}
