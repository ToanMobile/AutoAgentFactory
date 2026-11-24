//! Filesystem, CoW copy, symlink validation, read-only permission recovery,
//! and GC sweep helpers for [`super::warm_slot`].

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::process::Command;
use std::time::{Duration, SystemTime};

use super::GitError;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use super::cmd::run;
use super::cmd::{git, git_raw};
use super::warm_slot::{SlotKind, warm_slots_root};
use crate::db::private;

pub(super) const MAX_WALK_DEPTH: usize = 64;
#[cfg(any(target_os = "macos", target_os = "linux"))]
const COPY_TIMEOUT: Duration = Duration::from_secs(60);

pub(super) fn unlock_and_remove(file: File, lock_path: &Path) {
    let _ = file.unlock();
    drop(file);
    let _ = fs::remove_file(lock_path);
}

pub(super) fn cow_copy(src: &Path, dst: &Path) -> Result<(), GitError> {
    if path_contains_symlink(src, 0) {
        return Err(GitError::Invalid(format!(
            "symlink in snapshot {}",
            src.display()
        )));
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if let Some(cp) = ["/bin/cp", "/usr/bin/cp"]
        .into_iter()
        .map(Path::new)
        .find(|p| p.is_file())
    {
        #[cfg(target_os = "macos")]
        let flags: &[&str] = &["-cRP", "--"];
        #[cfg(target_os = "linux")]
        let flags: &[&str] = &["-aP", "--reflink=auto", "--"];
        let mut cmd = Command::new(cp);
        cmd.args(flags).arg(src).arg(dst);
        if run(&mut cmd, COPY_TIMEOUT).is_ok_and(|o| o.success()) {
            return Ok(());
        }
        remove_path_any(dst);
    }
    copy_tree_no_symlinks(src, dst, 0).map_err(GitError::Io)
}

fn copy_tree_no_symlinks(src: &Path, dst: &Path, depth: usize) -> io::Result<()> {
    if depth > MAX_WALK_DEPTH {
        return Err(io::Error::other("snapshot nesting limit exceeded"));
    }
    let meta = fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refused symlink",
        ));
    }
    if meta.is_dir() {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_tree_no_symlinks(&entry.path(), &dst.join(entry.file_name()), depth + 1)?;
        }
    } else if meta.is_file() {
        fs::copy(src, dst)?;
    }
    Ok(())
}

/// Returns `true` if any component prefix of `rel` under `base` is missing,
/// unreadable, or a symlink.
pub(super) fn has_symlink_prefix(base: &Path, rel: &Path) -> bool {
    let mut cur = base.to_path_buf();
    for comp in rel.components() {
        cur.push(comp);
        let Ok(m) = fs::symlink_metadata(&cur) else {
            return true;
        };
        if m.file_type().is_symlink() {
            return true;
        }
    }
    false
}

pub(super) fn path_contains_symlink(path: &Path, depth: usize) -> bool {
    let Ok(m) = fs::symlink_metadata(path) else {
        return true;
    };
    depth > MAX_WALK_DEPTH
        || m.file_type().is_symlink()
        || (m.is_dir()
            && fs::read_dir(path).map_or(true, |it| {
                it.into_iter()
                    .any(|e| e.map_or(true, |ent| path_contains_symlink(&ent.path(), depth + 1)))
            }))
}

/// Restores owner write/search permissions recursively without following symlinks,
/// so read-only build artifacts inside a warm slot can still be removed.
pub(super) fn make_writable_no_symlinks(path: &Path, depth: usize) {
    if depth > MAX_WALK_DEPTH {
        return;
    }
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        if meta.is_dir() {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o700));
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    make_writable_no_symlinks(&entry.path(), depth + 1);
                }
            }
        } else if meta.is_file() {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o600));
        }
    }
    #[cfg(not(unix))]
    {
        let mut perms = meta.permissions();
        if perms.readonly() {
            perms.set_readonly(false);
            let _ = fs::set_permissions(path, perms);
        }
        if meta.is_dir()
            && let Ok(entries) = fs::read_dir(path)
        {
            for entry in entries.flatten() {
                make_writable_no_symlinks(&entry.path(), depth + 1);
            }
        }
    }
}

pub(super) fn remove_dir_all_recover(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(_) => {
            make_writable_no_symlinks(path, 0);
            fs::remove_dir_all(path)
        }
    }
}

pub(super) fn remove_path_any(path: &Path) {
    if let Ok(m) = fs::symlink_metadata(path) {
        let _ = if m.is_dir() && !m.file_type().is_symlink() {
            remove_dir_all_recover(path)
        } else {
            fs::remove_file(path)
        };
    }
}

pub(super) fn read_dir_paths(dir: &Path) -> Vec<PathBuf> {
    if !is_real_dir(dir) {
        return Vec::new();
    }
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| Some(e.ok()?.path()))
        .collect();
    paths.sort();
    paths
}

pub(super) fn is_real_dir(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
}

pub(super) fn append_lock_ext(p: &Path) -> PathBuf {
    let mut os = p.as_os_str().to_os_string();
    os.push(".lock");
    PathBuf::from(os)
}

/// Sweeps expired unlocked ephemeral worktrees, base snapshots, and idle
/// `slot-0` dirs under `warm_slots_root(data_dir)`. NEVER unlinks `slot-0.lock`.
pub(crate) fn sweep_expired(
    data_dir: &Path,
    max_age: Duration,
    now: SystemTime,
    dry_run: bool,
    removed: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let root = warm_slots_root(data_dir);
    let older = |m: &fs::Metadata| {
        m.modified()
            .ok()
            .is_some_and(|t| now.duration_since(t).unwrap_or_default() >= max_age)
    };
    let mut common_dirs: BTreeSet<PathBuf> = BTreeSet::new();
    let mut record_common_dir = |dir: &Path| {
        if !dry_run
            && fs::symlink_metadata(dir.join(".git")).is_ok_and(|m| !m.file_type().is_symlink())
            && let Ok(common) = git(
                dir,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )
            && !common.is_empty()
        {
            common_dirs.insert(PathBuf::from(common));
        }
    };
    let mut rm_dir = |p: &Path| {
        if !dry_run && let Err(e) = remove_dir_all_recover(p) {
            warnings.push(format!("{}: {e}", p.display()));
            return;
        }
        removed.push(p.display().to_string());
    };
    for kind in [SlotKind::Merge, SlotKind::Oracle] {
        for repo_dir in read_dir_paths(&root.join(kind.dir_name())) {
            if !is_real_dir(&repo_dir) {
                continue;
            }
            for entry in read_dir_paths(&repo_dir) {
                let Some(name) = entry.file_name().and_then(OsStr::to_str) else {
                    continue;
                };
                let Ok(m) = fs::symlink_metadata(&entry) else {
                    continue;
                };
                if !name.starts_with("ephemeral-") || m.file_type().is_symlink() || !older(&m) {
                    continue;
                }
                if m.is_dir() {
                    let lp = append_lock_ext(&entry);
                    if let Some(held) = try_lock_existing(&lp) {
                        record_common_dir(&entry);
                        rm_dir(&entry);
                        if let Some(f) = held {
                            if !dry_run && !entry.exists() {
                                unlock_and_remove(f, &lp);
                            } else {
                                let _ = f.unlock();
                            }
                        }
                    }
                } else if !dry_run
                    && m.is_file()
                    && name.ends_with(".lock")
                    && !entry.with_extension("").exists()
                    && let Some(Some(f)) = try_lock_existing(&entry)
                {
                    unlock_and_remove(f, &entry);
                }
            }
            let (slot0, snaps) = (repo_dir.join("slot-0"), repo_dir.join("base-snaps"));
            if !slot0.exists() && !snaps.exists() {
                continue;
            }
            let lock_path = repo_dir.join("slot-0.lock");
            let lock_existed = match fs::symlink_metadata(&lock_path) {
                Ok(_) => true,
                Err(e) if e.kind() == io::ErrorKind::NotFound => false,
                Err(_) => continue,
            };
            let Some(guard) = try_lock_slot0(&lock_path, dry_run) else {
                continue;
            };
            let lock_older =
                !lock_existed || fs::symlink_metadata(&lock_path).is_ok_and(|m| older(&m));
            for snap in read_dir_paths(&snaps) {
                if fs::symlink_metadata(&snap)
                    .is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink() && older(&m))
                {
                    rm_dir(&snap);
                }
            }
            if lock_older
                && fs::symlink_metadata(&slot0)
                    .is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink() && older(&m))
            {
                record_common_dir(&slot0);
                rm_dir(&slot0);
            }
            if let Some(f) = guard {
                let _ = f.unlock(); // NEVER unlink `slot-0.lock`.
            }
        }
    }
    if !dry_run {
        for common_dir in common_dirs {
            let _ = git_raw(&common_dir, &["worktree", "prune"]);
        }
    }
}

fn try_lock_slot0(lp: &Path, dry_run: bool) -> Option<Option<File>> {
    match fs::symlink_metadata(lp) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if dry_run {
                Some(None)
            } else {
                private::open_file(lp)
                    .ok()
                    .filter(|f| f.try_lock().is_ok())
                    .map(Some)
            }
        }
        Ok(_) => try_lock_existing(lp),
        Err(_) => None,
    }
}

fn try_lock_existing(lp: &Path) -> Option<Option<File>> {
    match fs::symlink_metadata(lp) {
        Ok(m) if m.is_file() && !m.file_type().is_symlink() => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Some(None),
        _ => return None,
    }
    let file = File::options().read(true).write(true).open(lp).ok()?;
    file.try_lock().ok().map(|()| Some(file))
}
