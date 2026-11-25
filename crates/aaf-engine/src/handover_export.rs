//! Atomic, symlink-safe automatic export of DevKit HTML + Markdown handover
//! reports under `<data_dir>/reports/handover-<task_id>.{html,md}`.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use aaf_domain::config::RepoConfig;
use serde::{Deserialize, Serialize};

use crate::db::{Db, private};
use crate::git::worktree::validate_task_id;
use crate::handover::{load_handover, render_html, render_markdown};

static EXPORT_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Paths and handover verdict produced by [`write_auto_handover_reports`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoHandoverPaths {
    pub html: PathBuf,
    pub markdown: PathBuf,
    pub status: String,
}

/// Errors returned while validating, loading, or atomically writing handover reports.
#[derive(Debug, thiserror::Error)]
pub enum HandoverError {
    #[error("invalid task id {0:?}")]
    InvalidTaskId(String),
    #[error("refusing to write through symlink or non-regular path: {}", .0.display())]
    UnsafePath(PathBuf),
    #[error("load handover: {0}")]
    Load(String),
    #[error("io error at {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn ensure_safe_reports_dir(data_dir: &Path) -> Result<PathBuf, HandoverError> {
    let base_dir = match fs::canonicalize(data_dir) {
        Ok(canon) if canon.is_dir() => canon,
        Ok(_) => return Err(HandoverError::UnsafePath(data_dir.to_path_buf())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => data_dir.to_path_buf(),
        Err(source) => {
            return Err(HandoverError::Io {
                path: data_dir.to_path_buf(),
                source,
            });
        }
    };
    let reports_dir = base_dir.join("reports");
    match fs::symlink_metadata(&reports_dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(HandoverError::UnsafePath(reports_dir));
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(HandoverError::Io {
                path: reports_dir,
                source,
            });
        }
    }
    private::create_dir(&reports_dir).map_err(|source| HandoverError::Io {
        path: reports_dir.clone(),
        source,
    })?;
    let meta = fs::symlink_metadata(&reports_dir).map_err(|source| HandoverError::Io {
        path: reports_dir.clone(),
        source,
    })?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(HandoverError::UnsafePath(reports_dir));
    }
    fs::canonicalize(&reports_dir).map_err(|source| HandoverError::Io {
        path: reports_dir,
        source,
    })
}

fn check_target_not_symlink(target: &Path) -> Result<(), HandoverError> {
    match fs::symlink_metadata(target) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            Err(HandoverError::UnsafePath(target.to_path_buf()))
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(HandoverError::Io {
            path: target.to_path_buf(),
            source,
        }),
    }
}

fn write_atomic_0600(dir: &Path, target: &Path, bytes: &[u8]) -> Result<(), HandoverError> {
    check_target_not_symlink(target)?;
    let seq = EXPORT_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let temp = dir.join(format!(
        ".aaf-auto-handover-{}-{seq}.tmp",
        std::process::id()
    ));
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&temp).map_err(|source| HandoverError::Io {
        path: temp.clone(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(source) = file.set_permissions(fs::Permissions::from_mode(0o600)) {
            drop(file);
            let _ = fs::remove_file(&temp);
            return Err(HandoverError::Io { path: temp, source });
        }
    }
    if let Err(source) = file.write_all(bytes) {
        drop(file);
        let _ = fs::remove_file(&temp);
        return Err(HandoverError::Io { path: temp, source });
    }
    drop(file);
    if let Err(err) = check_target_not_symlink(target) {
        let _ = fs::remove_file(&temp);
        return Err(err);
    }
    if let Err(source) = fs::rename(&temp, target) {
        let _ = fs::remove_file(&temp);
        return Err(HandoverError::Io {
            path: target.to_path_buf(),
            source,
        });
    }
    Ok(())
}

/// Loads handover data read-only (never creates `receipt.key`) and writes both
/// `<data_dir>/reports/handover-<task_id>.html` and `.md` atomically (mode 0600).
pub fn write_auto_handover_reports(
    data_dir: &Path,
    db: &Db,
    task_id: &str,
    cfg: &RepoConfig,
) -> Result<AutoHandoverPaths, HandoverError> {
    validate_task_id(task_id).map_err(|_| HandoverError::InvalidTaskId(task_id.to_string()))?;
    let reports_dir = ensure_safe_reports_dir(data_dir)?;
    let data = load_handover(db, data_dir, task_id, cfg).map_err(HandoverError::Load)?;
    let html_bytes = render_html(&data, data_dir);
    let md_bytes = render_markdown(&data);

    let html_path = reports_dir.join(format!("handover-{task_id}.html"));
    let md_path = reports_dir.join(format!("handover-{task_id}.md"));
    write_atomic_0600(&reports_dir, &html_path, html_bytes.as_bytes())?;
    write_atomic_0600(&reports_dir, &md_path, md_bytes.as_bytes())?;

    Ok(AutoHandoverPaths {
        html: html_path,
        markdown: md_path,
        status: data.status,
    })
}
