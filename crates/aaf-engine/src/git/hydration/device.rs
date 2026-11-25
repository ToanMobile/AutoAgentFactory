//! Copy-on-write volume / device detection without `unsafe`.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use super::cleanup::MAX_WALK_DEPTH;
use crate::git::{GitError, cmd};

const MOUNT_TIMEOUT: Duration = Duration::from_secs(5);

/// A parsed line from macOS `/sbin/mount`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    pub mount_point: PathBuf,
    pub fs_type: String,
    pub read_only: bool,
}

/// Parses `/sbin/mount` output into structured entries.
///
/// Format: `<device> on <mount_point> (<fs_type>[, <opt>...])`.
pub fn parse_macos_mount_output(output: &str) -> Vec<MountEntry> {
    let mut entries = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        let Some((_, rest)) = trimmed.split_once(" on ") else {
            continue;
        };
        let Some((mount_str, opts_tail)) = rest.rsplit_once(" (") else {
            continue;
        };
        let Some(opts_inner) = opts_tail.strip_suffix(')') else {
            continue;
        };
        if mount_str.is_empty() {
            continue;
        }
        let mut opts = opts_inner.split(',').map(str::trim);
        let Some(fs_type) = opts.next().filter(|s| !s.is_empty()) else {
            continue;
        };
        let read_only = opts.any(|opt| opt == "read-only");
        entries.push(MountEntry {
            mount_point: PathBuf::from(mount_str),
            fs_type: fs_type.to_string(),
            read_only,
        });
    }
    entries
}

/// Finds the longest mount point (matched by path component via `Path::starts_with`)
/// containing `path`.
pub fn find_mount_for_path<'a>(path: &Path, mounts: &'a [MountEntry]) -> Option<&'a MountEntry> {
    mounts
        .iter()
        .filter(|entry| path.starts_with(&entry.mount_point))
        .max_by_key(|entry| {
            (
                entry.mount_point.components().count(),
                entry.mount_point.as_os_str().len(),
            )
        })
}

/// Testable helper: returns true only when `path` matches an `apfs` mount in `mounts`
/// AND `mount_dev(mount_point) == Some(path_dev)` (fail-closed against `/` fallback).
#[doc(hidden)]
pub fn is_apfs_mount_with_dev(
    path: &Path,
    path_dev: u64,
    mounts: &[MountEntry],
    mount_dev: &dyn Fn(&Path) -> Option<u64>,
) -> bool {
    let Some(entry) = find_mount_for_path(path, mounts) else {
        return false;
    };
    entry.fs_type == "apfs" && mount_dev(&entry.mount_point) == Some(path_dev)
}

/// Walks up to the nearest existing ancestor of `path` and canonicalizes it.
///
/// Only climbs on `ErrorKind::NotFound`; any other error (e.g. `EACCES`, `ELOOP`,
/// `ENOTDIR`) is returned immediately (fail-closed).
pub fn nearest_existing_canonical(path: &Path) -> io::Result<PathBuf> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    for anc in abs.ancestors() {
        let candidate = if anc.as_os_str().is_empty() {
            Path::new(".")
        } else {
            anc
        };
        match fs::metadata(candidate) {
            Ok(_) => return fs::canonicalize(candidate),
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("no existing ancestor for {}", path.display()),
    ))
}

/// Returns the Unix `st_dev` device id for the nearest existing ancestor of `path`.
#[cfg(unix)]
pub fn device_id(path: &Path) -> io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    let canon = nearest_existing_canonical(path)?;
    Ok(fs::metadata(canon)?.dev())
}

#[cfg(not(unix))]
pub fn device_id(_path: &Path) -> io::Result<u64> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "device_id requires Unix",
    ))
}

/// Reads and parses macOS `/sbin/mount` output with a bounded timeout.
pub fn read_macos_mounts() -> Result<Vec<MountEntry>, GitError> {
    let mut c = Command::new("/sbin/mount");
    let out = cmd::run(&mut c, MOUNT_TIMEOUT).map_err(|err| {
        if err.kind() == io::ErrorKind::TimedOut {
            GitError::Timeout {
                command: "/sbin/mount".to_string(),
            }
        } else {
            GitError::Io(err)
        }
    })?;
    if !out.success() {
        return Err(GitError::Command {
            command: "/sbin/mount".to_string(),
            code: out.code,
            stderr: out.stderr.trim().to_string(),
        });
    }
    Ok(parse_macos_mount_output(&out.stdout))
}

/// Pure check combining `dev()` equality and optional macOS APFS status.
pub fn same_cow_device_from_meta(src_dev: u64, dst_dev: u64, macos_apfs: Option<bool>) -> bool {
    src_dev == dst_dev && macos_apfs.unwrap_or(true)
}

/// Testable helper: checks whether `src` and `dst_parent` live on the same CoW-capable
/// device using caller-supplied macOS mount entries.
#[doc(hidden)]
pub fn same_cow_device_with_mounts(
    src: &Path,
    dst_parent: &Path,
    mounts: Option<&[MountEntry]>,
) -> Result<bool, GitError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let src_canon = nearest_existing_canonical(src)?;
        let dst_canon = nearest_existing_canonical(dst_parent)?;
        let src_dev = fs::metadata(&src_canon)?.dev();
        let dst_dev = fs::metadata(&dst_canon)?.dev();
        let macos_apfs = mounts.map(|ms| {
            let mp_dev = |mp: &Path| fs::metadata(mp).ok().map(|m| m.dev());
            is_apfs_mount_with_dev(&src_canon, src_dev, ms, &mp_dev)
                && is_apfs_mount_with_dev(&dst_canon, dst_dev, ms, &mp_dev)
        });
        Ok(same_cow_device_from_meta(src_dev, dst_dev, macos_apfs))
    }
    #[cfg(not(unix))]
    {
        let _ = (src, dst_parent, mounts);
        Ok(false)
    }
}

/// Checks whether `src` and `dst_parent` live on the same CoW-capable device.
///
/// On macOS this requires identical `MetadataExt::dev()` AND an `apfs` mount
/// whose `st_dev` matches the path. On Linux this requires identical `dev()`.
pub fn same_cow_device(src: &Path, dst_parent: &Path) -> Result<bool, GitError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let src_canon = nearest_existing_canonical(src)?;
        let dst_canon = nearest_existing_canonical(dst_parent)?;
        if fs::metadata(&src_canon)?.dev() != fs::metadata(&dst_canon)?.dev() {
            return Ok(false);
        }
        #[cfg(target_os = "macos")]
        {
            let mounts = read_macos_mounts()?;
            same_cow_device_with_mounts(&src_canon, &dst_canon, Some(&mounts))
        }
        #[cfg(not(target_os = "macos"))]
        {
            same_cow_device_with_mounts(&src_canon, &dst_canon, None)
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (src, dst_parent);
        Ok(false)
    }
}

/// Scans only subdirectories of `root_dir` (using `DirEntry::file_type`, never
/// stating regular files or following symlinks) and returns `Err("nested_mount: ...")`
/// if any directory has a `dev()` different from `root_dir`.
pub fn check_no_nested_mount(
    root_dir: &Path,
    sub_entries: Option<&[OsString]>,
    deadline: Instant,
) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        check_no_nested_mount_with(root_dir, sub_entries, deadline, &|p| {
            Ok(fs::symlink_metadata(p)?.dev())
        })
    }
    #[cfg(not(unix))]
    {
        let _ = (root_dir, sub_entries, deadline);
        Ok(())
    }
}

/// Testable helper for [`check_no_nested_mount`] with injectable `dev_of` lookup.
#[doc(hidden)]
pub fn check_no_nested_mount_with(
    root_dir: &Path,
    sub_entries: Option<&[OsString]>,
    deadline: Instant,
    dev_of: &dyn Fn(&Path) -> io::Result<u64>,
) -> Result<(), String> {
    let root_dev = dev_of(root_dir).map_err(|e| format!("nested_mount_stat: {e}"))?;
    if let Some(entries) = sub_entries {
        for name in entries {
            let p = root_dir.join(name);
            let meta = fs::symlink_metadata(&p).map_err(|e| format!("nested_mount_stat: {e}"))?;
            if meta.is_dir() && !meta.file_type().is_symlink() {
                walk_dirs_same_dev(&p, root_dev, deadline, 1, dev_of)?;
            }
        }
        Ok(())
    } else {
        walk_dirs_same_dev(root_dir, root_dev, deadline, 0, dev_of)
    }
}

fn walk_dirs_same_dev(
    dir: &Path,
    root_dev: u64,
    deadline: Instant,
    depth: usize,
    dev_of: &dyn Fn(&Path) -> io::Result<u64>,
) -> Result<(), String> {
    if depth > MAX_WALK_DEPTH {
        return Err("nested_mount_max_depth".to_string());
    }
    if Instant::now() >= deadline {
        return Err("timeout".to_string());
    }
    let dev = dev_of(dir).map_err(|e| format!("nested_mount_stat: {e}"))?;
    if dev != root_dev {
        return Err(format!("nested_mount: {}", dir.display()));
    }
    let rd = fs::read_dir(dir).map_err(|e| format!("nested_mount_read_dir: {e}"))?;
    for entry in rd {
        let entry = entry.map_err(|e| format!("nested_mount_entry: {e}"))?;
        // `DirEntry::file_type()` uses `d_type` without stating regular files and never follows symlinks.
        let ft = entry
            .file_type()
            .map_err(|e| format!("nested_mount_filetype: {e}"))?;
        if ft.is_dir() {
            walk_dirs_same_dev(&entry.path(), root_dev, deadline, depth + 1, dev_of)?;
        }
    }
    Ok(())
}
