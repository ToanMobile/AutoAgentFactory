//! Persistent per-scope state for `aaf notify`: cursor, pending delivery
//! progress, and single-instance advisory lock.

use std::collections::BTreeMap;
use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::super::board::store::err;
use crate::ctx::Ctx;
use crate::output::CmdError;

/// In-flight delivery state for a single alert event whose targets have not all
/// finished (`done` or `dropped`) yet.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct PendingState {
    pub event_id: i64,
    #[serde(default)]
    pub attempts: BTreeMap<String, u32>,
    #[serde(default)]
    pub done: Vec<String>,
    #[serde(default)]
    pub dropped: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub waiting_grace: bool,
}

impl PendingState {
    pub(super) fn new(event_id: i64) -> Self {
        Self {
            event_id,
            attempts: BTreeMap::new(),
            done: Vec::new(),
            dropped: Vec::new(),
            waiting_grace: false,
        }
    }

    pub(super) fn is_resolved(&self, target: &str) -> bool {
        self.done.iter().any(|t| t == target) || self.dropped.iter().any(|t| t == target)
    }

    pub(super) fn mark_done(&mut self, target: &str) {
        if !self.done.iter().any(|t| t == target) {
            self.done.push(target.to_string());
        }
    }

    pub(super) fn mark_dropped(&mut self, target: &str) {
        if !self.dropped.iter().any(|t| t == target) {
            self.dropped.push(target.to_string());
        }
    }

    pub(super) fn bump_attempt(&mut self, target: &str) -> u32 {
        let entry = self.attempts.entry(target.to_string()).or_insert(0);
        *entry = entry.saturating_add(1);
        *entry
    }
}

pub(super) fn cursor_path(ctx: &Ctx, scope_key: &str) -> PathBuf {
    ctx.data_dir
        .join("notify")
        .join(format!("{scope_key}.cursor"))
}

pub(super) fn pending_path(ctx: &Ctx, scope_key: &str) -> PathBuf {
    ctx.data_dir
        .join("notify")
        .join(format!("{scope_key}.pending"))
}

pub(super) fn lock_path(ctx: &Ctx, scope_key: &str) -> PathBuf {
    ctx.data_dir
        .join("notify")
        .join(format!("{scope_key}.lock"))
}

pub(super) fn read_cursor(path: &Path) -> Result<Option<i64>, CmdError> {
    match fs::read_to_string(path) {
        Ok(text) => text
            .trim()
            .parse()
            .map(Some)
            .map_err(|e| err(&format!("cursor {}", path.display()), e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(err(&format!("cursor {}", path.display()), e)),
    }
}

pub(super) fn write_cursor(path: &Path, id: i64) -> Result<(), CmdError> {
    write_atomic(path, "cursor.tmp", id.to_string().as_bytes(), "cursor")
}

/// Reads `<key>.pending`; corrupt JSON or read failures are discarded with a
/// warning on stderr so notify treats the event as fresh instead of wedging.
pub(super) fn read_pending(path: &Path) -> Option<PendingState> {
    let raw = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            eprintln!(
                "aaf notify: discarding unreadable pending state {}: {e}",
                path.display()
            );
            let _ = fs::remove_file(path);
            return None;
        }
    };
    match serde_json::from_str::<PendingState>(&raw) {
        Ok(state) => Some(state),
        Err(e) => {
            eprintln!(
                "aaf notify: discarding corrupt pending state {}: {e}",
                path.display()
            );
            let _ = fs::remove_file(path);
            None
        }
    }
}

pub(super) fn write_pending(path: &Path, state: &PendingState) -> Result<(), CmdError> {
    let bytes = serde_json::to_vec(state).map_err(|e| err("pending state", e))?;
    write_atomic(path, "pending.tmp", &bytes, "pending state")
}

pub(super) fn remove_pending(path: &Path) {
    if let Err(e) = fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!(
            "aaf notify: could not remove pending state {}: {e}",
            path.display()
        );
    }
}

/// Acquires an exclusive advisory lock for `scope_key` so two `aaf notify`
/// instances cannot race on the same cursor and duplicate alerts.
pub(super) fn acquire_scope_lock(ctx: &Ctx, scope_key: &str) -> Result<File, CmdError> {
    let path = lock_path(ctx, scope_key);
    if let Some(dir) = path.parent() {
        aaf_engine::db::private::create_dir(dir).map_err(|e| err("notify dir", e))?;
    }
    let file = aaf_engine::db::private::open_file(&path)
        .map_err(|e| err(&path.display().to_string(), e))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(CmdError::new(format!(
            "another aaf notify is already watching this scope ({scope_key})"
        ))),
        Err(TryLockError::Error(e)) => Err(err(&path.display().to_string(), e)),
    }
}

fn write_atomic(path: &Path, tmp_ext: &str, bytes: &[u8], label: &str) -> Result<(), CmdError> {
    if let Some(dir) = path.parent() {
        aaf_engine::db::private::create_dir(dir).map_err(|e| err("notify dir", e))?;
    }
    let tmp = path.with_extension(tmp_ext);
    fs::write(&tmp, bytes).map_err(|e| err(label, e))?;
    fs::rename(&tmp, path).map_err(|e| err(label, e))
}
