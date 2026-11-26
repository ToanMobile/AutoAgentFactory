//! Bounded TTL caches for `/api/report` and `/api/doctor` so 15s Studio polls
//! do not hammer `SharedRoDb` or spike RSS under concurrent tabs.
//!
//! Never holds the cache mutex across expensive DB or subprocess work: callers
//! snapshot the current `generation`, compute outside the lock, and only store
//! the result if no mutating POST invalidated the cache in between.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use aaf_engine::throttle::{LoadProbe, Pressure, SystemProbe};
use serde_json::Value;

use super::studio_health::RuntimeHealthCache;

const STATUS_CACHE_TTL: Duration = Duration::from_secs(1);
const PRESSURE_CACHE_TTL: Duration = Duration::from_secs(5);

#[derive(Default)]
pub(super) struct StatusCache {
    pub(super) generation: u64,
    pub(super) entry: Option<(Instant, Value)>,
}

#[derive(Default)]
pub(super) struct UiCaches {
    pub(super) status: Mutex<StatusCache>,
    pub(super) status_refresh: Mutex<()>,
    pub(super) report_refresh: Mutex<()>,
    pub(super) pressure: Mutex<Option<(Instant, Option<Pressure>)>>,
    pub(super) report: ReportCache,
    pub(super) doctor: DoctorCache,
    pub(super) health: RuntimeHealthCache,
}

impl UiCaches {
    pub(super) fn invalidate_all(&self) {
        let mut guard = self.status.lock().unwrap_or_else(|e| e.into_inner());
        guard.generation = guard.generation.wrapping_add(1);
        guard.entry = None;
        drop(guard);
        self.report.invalidate();
    }

    pub(super) fn cached_pressure(&self) -> Option<Pressure> {
        {
            let guard = self.pressure.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((at, reading)) = guard.as_ref()
                && at.elapsed() < PRESSURE_CACHE_TTL
            {
                return *reading;
            }
        }
        let reading = SystemProbe::detect().pressure();
        *self.pressure.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), reading));
        reading
    }

    pub(super) fn cached_status_or_gen(&self) -> Result<Value, u64> {
        let guard = self.status.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .entry
            .as_ref()
            .filter(|(at, _)| at.elapsed() < STATUS_CACHE_TTL)
            .map(|(_, cached)| cached.clone())
            .ok_or(guard.generation)
    }

    pub(super) fn store_status_if_current(&self, start_gen: u64, out: &Value) {
        let mut guard = self.status.lock().unwrap_or_else(|e| e.into_inner());
        if guard.generation == start_gen {
            guard.entry = Some((Instant::now(), out.clone()));
        }
    }
}

const REPORT_TTL: Duration = Duration::from_secs(8);
const DOCTOR_TTL: Duration = Duration::from_secs(30);
const MAX_REPORT_BUCKETS: usize = 4;
const MAX_DOCTOR_BUCKETS: usize = 8;

type ReportKey = (Option<String>, Option<u64>);

#[derive(Default)]
struct CacheInner<K> {
    generation: u64,
    entries: HashMap<K, (Instant, Value)>,
}

pub(super) struct ReportCache {
    inner: Mutex<CacheInner<ReportKey>>,
}

impl Default for ReportCache {
    fn default() -> Self {
        Self {
            inner: Mutex::new(CacheInner::default()),
        }
    }
}

impl ReportCache {
    pub(super) fn get_or_generation(
        &self,
        repo: Option<&str>,
        since: Option<u64>,
    ) -> Result<Value, u64> {
        let key = (repo.map(str::to_string), since);
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .entries
            .get(&key)
            .filter(|(at, _)| at.elapsed() < REPORT_TTL)
            .map(|(_, v)| v.clone())
            .ok_or(guard.generation)
    }

    pub(super) fn store_if_current(
        &self,
        repo: Option<&str>,
        since: Option<u64>,
        start_gen: u64,
        val: &Value,
    ) {
        let key = (repo.map(str::to_string), since);
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard.generation != start_gen {
            return;
        }
        guard.entries.retain(|_, (at, _)| at.elapsed() < REPORT_TTL);
        if guard.entries.len() >= MAX_REPORT_BUCKETS
            && !guard.entries.contains_key(&key)
            && let Some(oldest) = guard
                .entries
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(k, _)| k.clone())
        {
            guard.entries.remove(&oldest);
        }
        guard.entries.insert(key, (Instant::now(), val.clone()));
    }

    pub(super) fn invalidate(&self) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.generation = guard.generation.wrapping_add(1);
        guard.entries.clear();
    }
}

pub(super) struct DoctorCache {
    inner: Mutex<CacheInner<Option<String>>>,
}

impl Default for DoctorCache {
    fn default() -> Self {
        Self {
            inner: Mutex::new(CacheInner::default()),
        }
    }
}

impl DoctorCache {
    pub(super) fn get_or_generation(&self, repo: Option<&str>) -> Result<Value, u64> {
        let key = repo.map(str::to_string);
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .entries
            .get(&key)
            .filter(|(at, _)| at.elapsed() < DOCTOR_TTL)
            .map(|(_, v)| v.clone())
            .ok_or(guard.generation)
    }

    pub(super) fn store_if_current(&self, repo: Option<&str>, start_gen: u64, val: &Value) {
        let key = repo.map(str::to_string);
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard.generation != start_gen {
            return;
        }
        guard.entries.retain(|_, (at, _)| at.elapsed() < DOCTOR_TTL);
        if guard.entries.len() >= MAX_DOCTOR_BUCKETS
            && !guard.entries.contains_key(&key)
            && let Some(oldest) = guard
                .entries
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(k, _)| k.clone())
        {
            guard.entries.remove(&oldest);
        }
        guard.entries.insert(key, (Instant::now(), val.clone()));
    }
}
