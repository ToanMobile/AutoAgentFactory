//! Crash-safe JSONL archival for old `events` rows of terminal tasks (`done` / `cancelled`).
//!
//! Only runs during explicit `aaf gc` (never inside `lightweight_sweep`).
//! Ordering guarantee:
//! 1. Write JSONL archive to `<data_dir>/archives/events-<epoch>.jsonl.tmp` (mode `0600`),
//!    call `sync_all()`, and atomically `fs::rename` to `events-<epoch>.jsonl` **FIRST**.
//! 2. Open `aaf.db` and execute `DELETE FROM events WHERE id IN (...)` in one
//!    `BEGIN IMMEDIATE` transaction, commit, and run `PRAGMA wal_checkpoint(PASSIVE)`.

use std::fs;
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};

use super::GcError;
use crate::db::{ArchivedEventRow, Db, private};

/// Result of [`archive_old_terminal_events`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcArchiveStats {
    /// Number of terminal-task events archived (or eligible in `dry_run`).
    pub events_archived: usize,
    /// Path to the written JSONL archive file (`None` when `dry_run` or 0 rows).
    pub archive_path: Option<PathBuf>,
}

fn choose_archive_paths(archives_dir: &Path, epoch: i64) -> (PathBuf, PathBuf) {
    let base_final = archives_dir.join(format!("events-{epoch}.jsonl"));
    if !base_final.exists() {
        let base_tmp = archives_dir.join(format!("events-{epoch}.jsonl.tmp"));
        return (base_tmp, base_final);
    }
    for seq in 1u64..10_000 {
        let final_path = archives_dir.join(format!("events-{epoch}-{seq}.jsonl"));
        if !final_path.exists() {
            let tmp_path = archives_dir.join(format!("events-{epoch}-{seq}.jsonl.tmp"));
            return (tmp_path, final_path);
        }
    }
    let pid = std::process::id();
    (
        archives_dir.join(format!("events-{epoch}-{pid}.jsonl.tmp")),
        archives_dir.join(format!("events-{epoch}-{pid}.jsonl")),
    )
}

fn write_archive_atomic(
    archives_dir: &Path,
    epoch: i64,
    rows: &[ArchivedEventRow],
) -> Result<PathBuf, GcError> {
    private::create_dir(archives_dir)?;
    let (tmp_path, final_path) = choose_archive_paths(archives_dir, epoch);

    let file = private::open_file(&tmp_path)?;
    file.set_len(0)?;
    let write_res = (|| -> std::io::Result<()> {
        let mut writer = BufWriter::new(&file);
        for row in rows {
            let line = serde_json::to_string(row)
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
            writer.write_all(line.as_bytes())?;
            writer.write_all(b"\n")?;
        }
        writer.flush()?;
        drop(writer);
        file.sync_all()
    })();
    drop(file);

    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(err.into());
    }
    if let Err(err) = fs::rename(&tmp_path, &final_path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err.into());
    }
    private::restrict_file_if_present(&final_path)?;
    // Persist the directory entry on disk before callers delete rows from SQLite.
    fs::File::open(archives_dir).and_then(|dir| dir.sync_all())?;
    Ok(final_path)
}

/// Archives `events` rows belonging to terminal tasks (`done` or `cancelled`)
/// with `created_at < now - older_than_days * 86400`.
///
/// When `dry_run` is `true`, only counts eligible rows with zero disk or DB writes.
pub fn archive_old_terminal_events(
    data_dir: &Path,
    now: i64,
    older_than_days: u64,
    dry_run: bool,
) -> Result<GcArchiveStats, GcError> {
    let db_path = data_dir.join("aaf.db");
    if !db_path.is_file() {
        return Ok(GcArchiveStats::default());
    }

    let max_age_secs = i64::try_from(older_than_days.saturating_mul(86_400)).unwrap_or(i64::MAX);
    let cutoff = now.saturating_sub(max_age_secs);

    let ro = Db::open_read_only(&db_path)?;
    let rows = ro.list_archivable_terminal_events(cutoff)?;
    drop(ro);

    if rows.is_empty() {
        return Ok(GcArchiveStats::default());
    }
    if dry_run {
        return Ok(GcArchiveStats {
            events_archived: rows.len(),
            archive_path: None,
        });
    }

    let archive_path = write_archive_atomic(&data_dir.join("archives"), now, &rows)?;
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    let rw = Db::open(&db_path)?;
    let deleted = rw.delete_archived_events(&ids)?;
    Ok(GcArchiveStats {
        events_archived: deleted,
        archive_path: Some(archive_path),
    })
}
