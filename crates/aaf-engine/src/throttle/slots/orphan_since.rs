//! Persisted first-seen timestamps for orphaned heavy slots in `locks/`
//! (`0700`/`0600`), keyed by `(slot_index, pgid, lstart)`.
//!
//! Orphan age is always measured via `Clock` from the earliest observation
//! `min(in_memory_since, persisted_since)` — never from the lock file's `mtime`.
//! Sandboxed guests cannot read or write `locks/` and gracefully fall back to
//! in-memory observation only (and never reap owner lock files).

use std::collections::BTreeMap;
use std::fs::{self, File, TryLockError};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::super::orphans::{STARTED_KEY, orphan_reading};
use super::super::paths::{
    ensure_closed_dir, is_guest, legacy_slot_path, orphan_since_path, slot_path,
};
use super::super::probe::{GroupState, Unreadable};
use super::super::slot_file::{SlotFile, lock_text, unlock_all};
use crate::db::private;

const MAX_SIDECAR_BYTES: u64 = 4096;
const MAX_LSTART_LEN: usize = 40;

/// Identity of an orphaned command occupying a slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OrphanKey {
    pub(super) pgid: u32,
    pub(super) lstart: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OrphanSinceRecord {
    pgid: u32,
    lstart: Option<String>,
    since: i64,
}

impl OrphanSinceRecord {
    fn parse(text: &str) -> Option<Self> {
        let mut pgid: Option<u32> = None;
        let mut lstart: Option<String> = None;
        let mut since: Option<i64> = None;
        for line in text.lines() {
            if let Some(raw) = line.strip_prefix("pgid=") {
                pgid = raw.trim().parse::<u32>().ok().filter(|id| *id > 1);
            } else if let Some(raw) = line.strip_prefix(STARTED_KEY) {
                if !raw.is_empty() && raw.len() <= MAX_LSTART_LEN {
                    lstart = Some(raw.to_string());
                }
            } else if let Some(raw) = line.strip_prefix("since=") {
                since = raw.trim().parse::<i64>().ok();
            }
        }
        Some(Self {
            pgid: pgid?,
            lstart,
            since: since?,
        })
    }

    fn render(&self) -> String {
        let mut out = format!("pgid={}\n", self.pgid);
        if let Some(lstart) = &self.lstart {
            out.push_str(&format!("{STARTED_KEY}{lstart}\n"));
        }
        out.push_str(&format!("since={}\n", self.since));
        out
    }

    fn matches(&self, key: &OrphanKey) -> bool {
        self.pgid == key.pgid && self.lstart == key.lstart
    }
}

/// Sidecar paths attached to a held [`super::SlotGuard`], removed on release
/// while the slot lock is still held.
#[derive(Debug, Default)]
pub(super) struct SidecarSet {
    paths: Vec<PathBuf>,
}

impl SidecarSet {
    pub(super) fn empty() -> Self {
        Self { paths: Vec::new() }
    }

    pub(super) fn one(data_dir: &Path, index: usize) -> Self {
        let mut set = Self::empty();
        set.push(data_dir, index);
        set
    }

    pub(super) fn push(&mut self, data_dir: &Path, index: usize) {
        if !is_guest(data_dir) {
            self.paths.push(orphan_since_path(data_dir, index));
        }
    }

    pub(super) fn clear(&self) {
        for path in &self.paths {
            let _ = fs::remove_file(path);
        }
    }
}

/// In-memory first-seen timestamps keyed by slot index and orphan identity.
#[derive(Debug, Default)]
pub(crate) struct OrphanTracker {
    seen: BTreeMap<usize, (OrphanKey, i64)>,
}

impl OrphanTracker {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn observe(&mut self, slot: usize, key: &OrphanKey, now: i64) -> i64 {
        if let Some((existing_key, since)) = self.seen.get(&slot)
            && existing_key == key
        {
            return *since;
        }
        self.seen.insert(slot, (key.clone(), now));
        now
    }

    fn remove(&mut self, slot: usize) {
        self.seen.remove(&slot);
    }

    pub(crate) fn clear(&mut self) {
        self.seen.clear();
    }
}

fn recorded_lstart(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix(STARTED_KEY))
        .filter(|s| !s.is_empty() && s.len() <= MAX_LSTART_LEN)
        .map(str::to_string)
}

fn read_sidecar(data_dir: &Path, slot: usize) -> Option<OrphanSinceRecord> {
    if is_guest(data_dir) {
        return None;
    }
    let path = orphan_since_path(data_dir, slot);
    let plain = fs::symlink_metadata(&path)
        .is_ok_and(|meta| meta.is_file() && meta.len() <= MAX_SIDECAR_BYTES);
    if !plain {
        return None;
    }
    let text = fs::read_to_string(&path).ok()?;
    OrphanSinceRecord::parse(&text)
}

/// Reads the persisted `orphaned_since` for `(slot, pgid, lstart)` or records
/// `now` in `locks/heavy.<slot>.orphan_since` (`0600`) when absent or changed.
fn observe_sidecar(data_dir: &Path, slot: usize, key: &OrphanKey, now: i64) -> Option<i64> {
    if is_guest(data_dir) {
        return None;
    }
    if let Some(existing) = read_sidecar(data_dir, slot)
        && existing.matches(key)
    {
        return Some(existing.since);
    }
    ensure_closed_dir(data_dir).ok()?;
    let path = orphan_since_path(data_dir, slot);
    let mut file = private::open_file(&path).ok()?;
    if file.lock().is_err() {
        return Some(now);
    }
    let result = if let Some(existing) = read_sidecar(data_dir, slot)
        && existing.matches(key)
    {
        existing.since
    } else {
        let record = OrphanSinceRecord {
            pgid: key.pgid,
            lstart: key.lstart.clone(),
            since: now,
        };
        let text = record.render();
        let _ = file
            .seek(SeekFrom::Start(0))
            .and_then(|_| file.write_all(text.as_bytes()))
            .and_then(|_| file.set_len(text.len() as u64));
        now
    };
    unlock_all(std::slice::from_ref(&file));
    Some(result)
}

/// Removes the persisted `orphan_since` sidecar for `slot` when outside a guest sandbox.
pub(super) fn clear(data_dir: &Path, slot: usize) {
    if !is_guest(data_dir) {
        let _ = fs::remove_file(orphan_since_path(data_dir, slot));
    }
}

fn waited_from(since: i64, now: i64) -> u64 {
    u64::try_from(now - since).unwrap_or(0)
}

enum SlotVisit {
    /// Slot file is absent or empty; caller may inspect legacy lock file.
    Empty,
    /// Slot is actively held by a live process or holds an observed non-reaped orphan.
    Occupied,
    /// Timed-out orphan was truncated (`set_len(0)`, never unlinked) while locked.
    Reaped,
}

fn evaluate_orphan_text(
    data_dir: &Path,
    index: usize,
    text: &str,
    now: i64,
    heavy_wait_secs: u64,
    tracker: &mut OrphanTracker,
    read_group: &dyn Fn(u32) -> Result<Option<GroupState>, Unreadable>,
) -> Option<OrphanKey> {
    match orphan_reading(text, read_group) {
        // Process table unreadable: leave lock file and sidecar completely untouched.
        Err(Unreadable) => None,
        Ok(None) => {
            tracker.remove(index);
            None
        }
        Ok(Some(pgid)) => {
            let key = OrphanKey {
                pgid,
                lstart: recorded_lstart(text),
            };
            let persisted = observe_sidecar(data_dir, index, &key, now);
            let mem_since = tracker.observe(index, &key, now);
            let effective_since = persisted.map_or(mem_since, |p| mem_since.min(p));
            let timed_out =
                heavy_wait_secs > 0 && waited_from(effective_since, now) >= heavy_wait_secs;
            timed_out.then_some(key)
        }
    }
}

fn observe_current_slot(
    data_dir: &Path,
    index: usize,
    now: i64,
    heavy_wait_secs: u64,
    tracker: &mut OrphanTracker,
    read_group: &dyn Fn(u32) -> Result<Option<GroupState>, Unreadable>,
) -> io::Result<SlotVisit> {
    let path = slot_path(data_dir, index);
    if !path.exists() {
        return Ok(SlotVisit::Empty);
    }
    let Some(file) = SlotFile::open(data_dir, index)? else {
        return Ok(SlotVisit::Empty);
    };
    match file.try_lock() {
        Ok(()) => {
            let outcome = if file.lock_has_content().unwrap_or(false) {
                let text = lock_text(data_dir, index);
                if text.is_empty() {
                    SlotVisit::Empty
                } else if evaluate_orphan_text(
                    data_dir,
                    index,
                    &text,
                    now,
                    heavy_wait_secs,
                    tracker,
                    read_group,
                )
                .is_some()
                    && !file.is_guest()
                    && file.clear_record().is_ok()
                {
                    clear(data_dir, index);
                    tracker.remove(index);
                    SlotVisit::Reaped
                } else {
                    SlotVisit::Occupied
                }
            } else {
                SlotVisit::Empty
            };
            file.unlock();
            Ok(outcome)
        }
        Err(TryLockError::WouldBlock) => {
            tracker.remove(index);
            Ok(SlotVisit::Occupied)
        }
        Err(TryLockError::Error(err)) => Err(err),
    }
}

fn observe_legacy_slot(
    data_dir: &Path,
    index: usize,
    now: i64,
    heavy_wait_secs: u64,
    tracker: &mut OrphanTracker,
    read_group: &dyn Fn(u32) -> Result<Option<GroupState>, Unreadable>,
) -> bool {
    let legacy = legacy_slot_path(data_dir, index);
    if !legacy.exists() {
        return false;
    }
    let Ok(file) = File::options().write(true).open(&legacy) else {
        return false;
    };
    match file.try_lock() {
        Ok(()) => {
            let text = fs::read_to_string(&legacy).unwrap_or_default();
            let reaped = if !text.is_empty()
                && evaluate_orphan_text(
                    data_dir,
                    index,
                    &text,
                    now,
                    heavy_wait_secs,
                    tracker,
                    read_group,
                )
                .is_some()
                && file.set_len(0).is_ok()
            {
                clear(data_dir, index);
                tracker.remove(index);
                true
            } else {
                false
            };
            unlock_all(std::slice::from_ref(&file));
            reaped
        }
        Err(TryLockError::WouldBlock) => {
            tracker.remove(index);
            false
        }
        Err(TryLockError::Error(_)) => false,
    }
}

/// Observes live orphaned slots in `0..ceiling`, persists/reads their
/// `locks/heavy.<n>.orphan_since` timestamps keyed by `(slot, pgid, lstart)`,
/// and safely truncates (`set_len(0)`, never unlinks) any orphan whose effective
/// age has reached `heavy_wait_secs` (when `heavy_wait_secs > 0`).
///
/// All observation and reaping runs while holding each slot's exclusive flock
/// so a concurrently acquired live slot can never have an orphan sidecar written.
pub(crate) fn observe_and_reap_orphans(
    data_dir: &Path,
    ceiling: usize,
    now: i64,
    heavy_wait_secs: u64,
    tracker: &mut OrphanTracker,
    read_group: &dyn Fn(u32) -> Result<Option<GroupState>, Unreadable>,
) -> io::Result<usize> {
    let ceiling = ceiling.max(1);
    let mut reaped = 0usize;

    for index in 0..ceiling {
        match observe_current_slot(data_dir, index, now, heavy_wait_secs, tracker, read_group)? {
            SlotVisit::Reaped => reaped += 1,
            SlotVisit::Occupied => {}
            SlotVisit::Empty => {
                if observe_legacy_slot(data_dir, index, now, heavy_wait_secs, tracker, read_group) {
                    reaped += 1;
                }
            }
        }
    }

    Ok(reaped)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn held_flock_slot_never_creates_orphan_since_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let lock_path = slot_path(data, 0);
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        let holder = private::open_file(&lock_path).unwrap();
        holder.try_lock().unwrap();
        fs::write(
            &lock_path,
            "reserved=unknown\npgid=42424\nlstart=Sun Nov 22 11:00:00 2026\n",
        )
        .unwrap();

        let mut tracker = OrphanTracker::new();
        let read_group = |pgid: u32| {
            Ok((pgid == 42424).then(|| GroupState {
                stopped: false,
                leader_started: Some("Sun Nov 22 11:00:00 2026".to_string()),
            }))
        };
        let reaped =
            observe_and_reap_orphans(data, 1, 10_000, 1, &mut tracker, &read_group).unwrap();
        assert_eq!(reaped, 0);
        assert!(
            !orphan_since_path(data, 0).exists(),
            "held flock slot must never create heavy.0.orphan_since"
        );
        drop(holder);
    }
}
