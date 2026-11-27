//! Atomic 0600 sidecar persistence for Tier-3 AI decision advice under
//! `<data_dir>/decision_advice/<sha256(task_id)>.json`.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

pub const ADVICE_SCHEMA_VERSION: u32 = 1;
pub const ADVICE_TTL_SECS: i64 = 24 * 3600;
pub const PENDING_STALE_SECS: i64 = 105; // 90s worker timeout + 15s grace
const MAX_SIDECAR_BYTES: u64 = 128 * 1024;

static TMP_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdviceOption {
    pub num: String,
    pub title: String,
    #[serde(default)]
    pub desc: String,
    #[serde(default)]
    pub recommended: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdviceItem {
    pub question: String,
    pub kind: String,
    pub requires_human_input: bool,
    pub hint: String,
    #[serde(default)]
    pub options: Vec<AdviceOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredAdvice {
    pub schema_version: u32,
    pub task_id: String,
    pub input_hash: String,
    /// One of `"pending"`, `"ready"`, `"error"`.
    pub status: String,
    #[serde(default)]
    pub model: String,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub items: Vec<AdviceItem>,
}

pub fn advice_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("decision_advice")
}

/// Keyed by SHA-256 of `task_id` so path traversal is impossible by construction.
pub fn advice_path(data_dir: &Path, task_id: &str) -> PathBuf {
    let hex = aaf_fp::sha256_hex(task_id.as_bytes());
    advice_dir(data_dir).join(format!("{hex}.json"))
}

fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    if let Ok(meta) = fs::symlink_metadata(dir) {
        if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "decision_advice directory is a symlink or non-directory",
            ));
        }
    } else {
        fs::create_dir_all(dir)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

pub fn write_advice_atomic(data_dir: &Path, record: &StoredAdvice) -> io::Result<()> {
    let dir = advice_dir(data_dir);
    ensure_private_dir(&dir)?;
    let target = advice_path(data_dir, &record.task_id);
    if let Ok(meta) = fs::symlink_metadata(&target)
        && meta.file_type().is_symlink()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to overwrite symlink advice file",
        ));
    }
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if bytes.len() as u64 > MAX_SIDECAR_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "decision advice payload exceeds size cap",
        ));
    }
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".tmp-{}-{seq}.json", std::process::id()));
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    let res = f.write_all(&bytes).and_then(|()| f.sync_all());
    drop(f);
    if let Err(e) = res {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp, &target) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Reads and validates a cached sidecar. Returns `None` on missing file,
/// schema mismatch, inner `task_id` mismatch, `input_hash` mismatch, or TTL expiry.
/// Converts stale `pending` entries older than [`PENDING_STALE_SECS`] into an
/// `error` status cleanly.
pub fn read_valid_advice(
    data_dir: &Path,
    expected_task_id: &str,
    expected_input_hash: &str,
    now: i64,
) -> Option<StoredAdvice> {
    let path = advice_path(data_dir, expected_task_id);
    let meta = fs::symlink_metadata(&path).ok()?;
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return None;
    }
    if meta.len() > MAX_SIDECAR_BYTES {
        return None;
    }
    let mut raw = String::new();
    let f = fs::File::open(&path).ok()?;
    f.take(MAX_SIDECAR_BYTES + 1)
        .read_to_string(&mut raw)
        .ok()?;
    if raw.len() as u64 > MAX_SIDECAR_BYTES {
        return None;
    }
    let mut parsed: StoredAdvice = serde_json::from_str(&raw).ok()?;
    if parsed.schema_version != ADVICE_SCHEMA_VERSION
        || parsed.task_id != expected_task_id
        || parsed.input_hash != expected_input_hash
    {
        return None;
    }
    let age = now.saturating_sub(parsed.created_at);
    if parsed.status == "pending" {
        if age > PENDING_STALE_SECS {
            parsed.status = "error".into();
            parsed.error = Some("analysis timed out; click retry to run again".into());
        }
        return Some(parsed);
    }
    if age > ADVICE_TTL_SECS {
        return None;
    }
    Some(parsed)
}
