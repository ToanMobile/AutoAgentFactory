//! Learned memory use of heavy commands: `<data_dir>/heavy_stats.json`.
//! The file only tunes admission, so every failure to read it degrades to
//! "never measured" (the command runs alone) instead of an error.
//!
//! A guest ([`super::paths`]) can neither read nor write that file. What it
//! measures goes to `guest/heavy_stats.json`, which only guests read: a
//! figure an agent writes there changes how the builds of agents are
//! admitted, and nothing for a command outside the sandbox.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::Sleeper;
use super::paths::{ensure_closed_dir, guest_dir, is_guest, open_guest_file, read_guest_file};
use crate::db::private;

/// Runs remembered per command; the admission figure is their maximum.
pub const KEEP_RUNS: usize = 5;

const FILE: &str = "heavy_stats.json";
const LOCK: &str = "heavy_stats.lock";
const LOCK_ATTEMPTS: u32 = 40;
const LOCK_RETRY: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeavySample {
    pub peak_rss_bytes: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StatsFile {
    /// `<repo_id>/<label>` to its most recent runs, oldest first.
    #[serde(default)]
    entries: BTreeMap<String, Vec<HeavySample>>,
}

pub fn stats_path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE)
}

fn key(repo_id: &str, label: &str) -> String {
    format!("{repo_id}/{label}")
}

fn load(data_dir: &Path, guest: bool) -> StatsFile {
    let bytes = if guest {
        Some(read_guest_file(&guest_dir(data_dir).join(FILE)).into_bytes())
    } else {
        fs::read(stats_path(data_dir)).ok()
    };
    bytes
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Remembered runs of one command, oldest first; empty when never measured
/// or when the file is missing or corrupt.
pub fn samples(data_dir: &Path, repo_id: &str, label: &str) -> Vec<HeavySample> {
    load(data_dir, is_guest(data_dir))
        .entries
        .remove(&key(repo_id, label))
        .unwrap_or_default()
}

/// Largest peak among the remembered runs; `None` means "never measured".
pub fn recorded_peak(data_dir: &Path, repo_id: &str, label: &str) -> Option<u64> {
    samples(data_dir, repo_id, label)
        .iter()
        .map(|sample| sample.peak_rss_bytes)
        .max()
        .filter(|peak| *peak > 0)
}

fn lock_file(data_dir: &Path, guest: bool, sleeper: &dyn Sleeper) -> io::Result<Option<File>> {
    let file = if guest {
        open_guest_file(&guest_dir(data_dir).join(LOCK))?
    } else {
        private::open_file(&ensure_closed_dir(data_dir)?.join(LOCK))?
    };
    for _ in 0..LOCK_ATTEMPTS {
        if file.try_lock().is_ok() {
            return Ok(Some(file));
        }
        sleeper.sleep(LOCK_RETRY);
    }
    Ok(None)
}

/// Appends one run and keeps the last [`KEEP_RUNS`]. Returns `false` when the
/// stats lock stayed taken: the run is then simply not remembered.
pub fn record(
    data_dir: &Path,
    repo_id: &str,
    label: &str,
    sample: HeavySample,
    sleeper: &dyn Sleeper,
) -> io::Result<bool> {
    let guest = is_guest(data_dir);
    // The lock serialises read-modify-write between processes; readers need
    // none because the rename below replaces the file atomically.
    let Some(lock) = lock_file(data_dir, guest, sleeper)? else {
        return Ok(false);
    };
    let written = append(data_dir, guest, &key(repo_id, label), sample);
    // Unlocked explicitly: a child process being spawned right now holds a
    // copy of the descriptor, so the close alone could release it late. If
    // the unlock fails the close still does it.
    let _ = lock.unlock();
    written.map(|()| true)
}

fn append(data_dir: &Path, guest: bool, key: &str, sample: HeavySample) -> io::Result<()> {
    let mut stats = load(data_dir, guest);
    let runs = stats.entries.entry(key.to_string()).or_default();
    runs.push(sample);
    if runs.len() > KEEP_RUNS {
        let extra = runs.len() - KEEP_RUNS;
        runs.drain(..extra);
    }
    let body = serde_json::to_vec_pretty(&stats).map_err(io::Error::other)?;
    let path = if guest {
        guest_dir(data_dir).join(FILE)
    } else {
        stats_path(data_dir)
    };
    let temp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    // Owner-only before the first byte: the rename then replaces a file an
    // earlier version left world-readable.
    let mut file = if guest {
        open_guest_file(&temp)?
    } else {
        private::open_file(&temp)?
    };
    file.set_len(0)?;
    file.write_all(&body)?;
    drop(file);
    fs::rename(&temp, &path)
}
