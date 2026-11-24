//! Where the throttle keeps its files, and who may do what with them.
//!
//! An agent run is confined by a sandbox that closes the data dir, and the
//! one way it may start a heavy build is `aaf slot`, which has to queue with
//! everybody else. So the files a queue is made of stand in a directory of
//! their own, `<data_dir>/throttle`, which the sandbox opens:
//!
//! - `throttle/` itself for reading only: the lock file of every slot and
//!   the drain lock. A process may lock a file it can only read, so a
//!   confined `aaf slot` takes its turn on the very files the factory's gate
//!   uses, and can neither remove nor rewrite them. A holder outside the
//!   sandbox writes its reservation and process group there, and nothing
//!   that opens a door.
//! - `throttle/guest/` for writing: what a confined holder has to write.
//!
//! Everything else stays in the closed part of the data dir, in `locks/`
//! next to the factory's run, commit and merge locks: the slot token of a
//! holder outside the sandbox, its pause records, the lock of the
//! statistics.
//!
//! A process that cannot write `throttle/` is a *guest*. Nothing a guest
//! writes is trusted where a file it cannot write says otherwise, and no
//! file of `guest/` is opened through a link.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use crate::db::private;

const DIR: &str = "throttle";
const GUEST: &str = "guest";
const POOLS: &str = "pools";
/// The closed directory; also where the slot and drain locks themselves
/// lived before they got a directory of their own ([`super::legacy`]).
const CLOSED_DIR: &str = "locks";
const DRAIN: &str = "drain.lock";
const GUEST_FILE_MAX: u64 = 1 << 20;

pub fn throttle_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(DIR)
}

pub fn guest_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(DIR).join(GUEST)
}

/// Shared directory for Layer-1 correctness pool locks (`tool.unity.*.lock`,
/// `worktree.*.lock`), writable by both host and sandboxed guests so both
/// acquire advisory locks on the exact same inode.
pub fn pools_dir(data_dir: &Path) -> PathBuf {
    guest_dir(data_dir).join(POOLS)
}

pub(super) fn closed_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(CLOSED_DIR)
}

/// [`closed_dir`], created for the owner only.
pub(super) fn ensure_closed_dir(data_dir: &Path) -> io::Result<PathBuf> {
    let dir = closed_dir(data_dir);
    private::create_dir(data_dir)?;
    private::create_dir(&dir)?;
    Ok(dir)
}

pub fn slot_path(data_dir: &Path, index: usize) -> PathBuf {
    throttle_dir(data_dir).join(format!("heavy.{index}.lock"))
}

/// The token of a holder of slot `index` outside the sandbox: where no
/// agent reads it. Whoever reads it runs beside that holder without waiting.
pub fn token_path(data_dir: &Path, index: usize) -> PathBuf {
    closed_dir(data_dir).join(format!("heavy.{index}.token"))
}

/// The record of a guest that holds slot `index`.
pub fn guest_record_path(data_dir: &Path, index: usize) -> PathBuf {
    guest_dir(data_dir).join(format!("heavy.{index}.rec"))
}

pub(super) fn legacy_slot_path(data_dir: &Path, index: usize) -> PathBuf {
    closed_dir(data_dir).join(format!("heavy.{index}.lock"))
}

pub fn drain_path(data_dir: &Path) -> PathBuf {
    throttle_dir(data_dir).join(DRAIN)
}

pub(super) fn legacy_drain_path(data_dir: &Path) -> PathBuf {
    closed_dir(data_dir).join(DRAIN)
}

/// `<data_dir>/throttle` with its `guest` directory, both for the owner
/// only inside a data dir that is for the owner only (existing ones are
/// tightened): the data dir holds receipts and the signing key.
///
/// A guest can create neither; for it the directories have to be there
/// already ([`super::slots::prepare_for_guests`]), and are then left as
/// they are.
pub fn locks_dir(data_dir: &Path) -> io::Result<PathBuf> {
    let dir = throttle_dir(data_dir);
    if is_guest(data_dir) {
        return Ok(dir);
    }
    private::create_dir(data_dir)?;
    private::create_dir(&dir)?;
    private::create_dir(&dir.join(GUEST))?;
    Ok(dir)
}

/// True for a process that may read `throttle/` and not write it: an agent
/// inside its sandbox. Asked of the drain lock, which is created before any
/// agent starts ([`super::slots::prepare_for_guests`]); where it does not
/// exist nobody was confined here yet.
pub fn is_guest(data_dir: &Path) -> bool {
    let opened = File::options().write(true).open(drain_path(data_dir));
    matches!(opened, Err(err) if err.kind() == io::ErrorKind::PermissionDenied)
}

/// Creates what a guest needs and cannot create, apart from the lock files
/// of the slots ([`super::slots::prepare_for_guests`]): the directories and
/// the drain lock.
pub(super) fn prepare(data_dir: &Path) -> io::Result<()> {
    locks_dir(data_dir)?;
    ensure_pools_dir(data_dir)?;
    private::open_file(&drain_path(data_dir)).map(|_| ())
}

/// Ensures [`pools_dir`] exists as a real (non-symlink) directory.
///
/// Outside the sandbox this also creates `<data_dir>/throttle/guest` (`0700`);
/// inside the sandbox it leaves the read-only parent alone and refuses any
/// symlink planted at `pools/`.
pub(super) fn ensure_pools_dir(data_dir: &Path) -> io::Result<PathBuf> {
    if !is_guest(data_dir) {
        locks_dir(data_dir)?;
    }
    let dir = pools_dir(data_dir);
    match fs::symlink_metadata(&dir) {
        Ok(meta) => {
            if !meta.is_dir() {
                return Err(not_plain(&dir));
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&dir) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    if !fs::symlink_metadata(&dir)?.is_dir() {
                        return Err(not_plain(&dir));
                    }
                }
                Err(err) => return Err(err),
            }
        }
        Err(err) => return Err(err),
    }
    Ok(dir)
}

#[cfg(unix)]
fn same_real_dir(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    before.is_dir()
        && !before.file_type().is_symlink()
        && after.is_dir()
        && !after.file_type().is_symlink()
        && before.dev() == after.dev()
        && before.ino() == after.ino()
}

#[cfg(not(unix))]
fn same_real_dir(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    before.is_dir()
        && !before.file_type().is_symlink()
        && after.is_dir()
        && !after.file_type().is_symlink()
}

/// Opens a Layer-1 pool lock file under [`pools_dir`], verifying via
/// `symlink_metadata` both before and after the open that `pools/` itself
/// remained the exact same real (non-symlink) directory.
pub(crate) fn open_pool_file(data_dir: &Path, filename: &str) -> io::Result<File> {
    let dir = ensure_pools_dir(data_dir)?;
    let before = fs::symlink_metadata(&dir)?;
    if !before.is_dir() || before.file_type().is_symlink() {
        return Err(not_plain(&dir));
    }
    let file = open_guest_file(&dir.join(filename))?;
    let after = fs::symlink_metadata(&dir)?;
    if !same_real_dir(&before, &after) {
        return Err(not_plain(&dir));
    }
    Ok(file)
}

#[cfg(unix)]
fn same_plain_file(at_path: &fs::Metadata, opened: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    at_path.is_file()
        && at_path.dev() == opened.dev()
        && at_path.ino() == opened.ino()
        && opened.nlink() == 1
}

#[cfg(not(unix))]
fn same_plain_file(at_path: &fs::Metadata, _opened: &fs::Metadata) -> bool {
    at_path.is_file()
}

fn not_plain(path: &Path) -> io::Error {
    io::Error::other(format!(
        "{} is a link or not a file of its own",
        path.display()
    ))
}

/// Opens a file of `guest/` for reading and writing, creating it for the
/// owner only. A guest can put any directory entry there, so the file is
/// used only when the entry is a regular file with a single name: never
/// through a symbolic link, and never a second name of some other file,
/// which a write here would then damage.
pub(super) fn open_guest_file(path: &Path) -> io::Result<File> {
    if fs::symlink_metadata(path).is_ok_and(|meta| !meta.is_file()) {
        return Err(not_plain(path));
    }
    let mut options = File::options();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    // Compared after the open: whatever was swapped in between, the handle
    // is used only if it is the plain file the name stands for now.
    if !same_plain_file(&fs::symlink_metadata(path)?, &file.metadata()?) {
        return Err(not_plain(path));
    }
    Ok(file)
}

/// The text of a file of `guest/`; empty when it is missing, unreadable,
/// too large or not a plain file (see [`open_guest_file`]).
pub(super) fn read_guest_file(path: &Path) -> String {
    // A record or the statistics of a few commands; anything larger was not
    // written by this tool and is not loaded into memory.
    let plain =
        fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file() && meta.len() <= GUEST_FILE_MAX);
    if !plain {
        return String::new();
    }
    fs::read_to_string(path).unwrap_or_default()
}
