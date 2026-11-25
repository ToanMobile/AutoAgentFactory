//! Per-repository freeze state with cooldown and capped half-open probing.

use std::path::Path;

use aaf_domain::ports::{
    KvStore, META_CONSECUTIVE_ROOT_FAILS, META_FACTORY_FROZEN, PortError, PortResult,
};

/// Cooldown before a frozen repository allows one half-open probe task.
pub const REPO_FREEZE_COOLDOWN_SECS: i64 = 1800;
/// Maximum automatic half-open probe attempts before requiring human unfreeze.
pub const MAX_REPO_PROBE_COUNT: u32 = 3;
/// Default consecutive root failures before freezing a repository.
pub const DEFAULT_REPO_FREEZE_THRESHOLD: u32 = 3;

pub use aaf_fp::fs_private::repo_frozen_key;

pub fn repo_frozen_until_key(repo_root: &str) -> String {
    format!(
        "repo_frozen_until:{}",
        aaf_fp::repo_id(Path::new(repo_root))
    )
}

pub fn repo_consecutive_fails_key(repo_root: &str) -> String {
    format!(
        "repo_consecutive_fails:{}",
        aaf_fp::repo_id(Path::new(repo_root))
    )
}

pub fn repo_probe_count_key(repo_root: &str) -> String {
    format!("repo_probe_count:{}", aaf_fp::repo_id(Path::new(repo_root)))
}

pub(super) fn parse_u32_meta(store: &dyn KvStore, key: &str) -> PortResult<u32> {
    match store.get_meta(key)? {
        None => Ok(0),
        Some(raw) if raw.trim().is_empty() => Ok(0),
        Some(raw) => raw
            .trim()
            .parse()
            .map_err(|_| PortError::Invalid(format!("{key} is not a number: {raw}"))),
    }
}

fn parse_i64_meta(store: &dyn KvStore, key: &str) -> PortResult<Option<i64>> {
    match store.get_meta(key)? {
        None => Ok(None),
        Some(raw) if raw.trim().is_empty() => Ok(None),
        Some(raw) => Ok(raw.trim().parse().ok()),
    }
}

fn probe_ready(store: &dyn KvStore, repo_root: &str, now: i64) -> PortResult<bool> {
    let until = parse_i64_meta(store, &repo_frozen_until_key(repo_root))?;
    let probes = parse_u32_meta(store, &repo_probe_count_key(repo_root)).unwrap_or(0);
    Ok(until.is_some_and(|u| u <= now) && probes < MAX_REPO_PROBE_COUNT)
}

/// Read-only check for whether `repo_root` is frozen at `now` (safe on read-only DBs).
/// Allows a half-open probe once cooldown expires while `repo_probe_count < MAX_REPO_PROBE_COUNT`,
/// and falls back to legacy `META_FACTORY_FROZEN` when no repo-scoped state is active.
pub fn is_repo_frozen(store: &dyn KvStore, repo_root: &str, now: i64) -> PortResult<bool> {
    match store.get_meta(&repo_frozen_key(repo_root))?.as_deref() {
        Some("probe") => Ok(false),
        Some("1") => Ok(!probe_ready(store, repo_root, now)?),
        _ => Ok(store.get_meta(META_FACTORY_FROZEN)?.as_deref() == Some("1")),
    }
}

/// Transitions a cooled-down frozen repo into `"probe"` state (concurrency cap = 1).
pub fn try_begin_repo_probe(store: &dyn KvStore, repo_root: &str, now: i64) -> PortResult<bool> {
    let key = repo_frozen_key(repo_root);
    match store.get_meta(&key)?.as_deref() {
        Some("probe") => Ok(true),
        Some("1") if probe_ready(store, repo_root, now)? => {
            store.set_meta(&key, "probe")?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Records a root task outcome for `repo_root`, freezing only that repo after
/// consecutive failures and handling half-open probe success/failure with a probe cap.
pub fn record_repo_root_result(
    store: &dyn KvStore,
    repo_root: &str,
    root_id: &str,
    success: bool,
    now: i64,
) -> PortResult<bool> {
    record_repo_root_result_with_threshold(
        store,
        repo_root,
        root_id,
        success,
        DEFAULT_REPO_FREEZE_THRESHOLD,
        now,
    )
}

pub(super) fn record_repo_root_result_with_threshold(
    store: &dyn KvStore,
    repo_root: &str,
    _root_id: &str,
    success: bool,
    threshold: u32,
    now: i64,
) -> PortResult<bool> {
    let frozen_key = repo_frozen_key(repo_root);
    let until_key = repo_frozen_until_key(repo_root);
    let fails_key = repo_consecutive_fails_key(repo_root);
    let probe_key = repo_probe_count_key(repo_root);
    let state = store.get_meta(&frozen_key)?;
    let in_probe = state.as_deref() == Some("probe")
        || (state.as_deref() == Some("1") && probe_ready(store, repo_root, now)?);

    if success {
        store.set_meta(&fails_key, "0")?;
        if in_probe {
            store.set_meta(&frozen_key, "0")?;
            store.set_meta(&until_key, "")?;
            store.set_meta(&probe_key, "0")?;
        }
        return is_repo_frozen(store, repo_root, now);
    }

    if in_probe {
        let probes = parse_u32_meta(store, &probe_key)?.saturating_add(1);
        store.set_meta(&probe_key, &probes.to_string())?;
        store.set_meta(&frozen_key, "1")?;
        if probes < MAX_REPO_PROBE_COUNT {
            store.set_meta(
                &until_key,
                &(now.saturating_add(REPO_FREEZE_COOLDOWN_SECS)).to_string(),
            )?;
        } else {
            store.set_meta(&until_key, "")?;
        }
        return Ok(true);
    }

    let fails = parse_u32_meta(store, &fails_key)?.saturating_add(1);
    store.set_meta(&fails_key, &fails.to_string())?;
    if fails >= threshold.max(1) && state.as_deref() != Some("1") {
        store.set_meta(&frozen_key, "1")?;
        store.set_meta(
            &until_key,
            &(now.saturating_add(REPO_FREEZE_COOLDOWN_SECS)).to_string(),
        )?;
        store.set_meta(&probe_key, "0")?;
    }
    is_repo_frozen(store, repo_root, now)
}

/// Clears both repo-scoped freeze keys for `repo_root` and legacy global freeze keys.
pub fn unfreeze_repo(store: &dyn KvStore, repo_root: &str) -> PortResult<()> {
    store.set_meta(&repo_frozen_key(repo_root), "0")?;
    store.set_meta(&repo_frozen_until_key(repo_root), "")?;
    store.set_meta(&repo_consecutive_fails_key(repo_root), "0")?;
    store.set_meta(&repo_probe_count_key(repo_root), "0")?;
    store.set_meta(META_FACTORY_FROZEN, "0")?;
    store.set_meta(META_CONSECUTIVE_ROOT_FAILS, "0")?;
    Ok(())
}
