//! The absolute path of each external program aaf runs by name (`git`,
//! `gh`), looked up once per process.
//!
//! A program named without a path is otherwise looked up again at every
//! spawn, in whatever the `PATH` directories hold at that moment: a `git`
//! the agent writes into a directory early on `PATH` would then run with the
//! operator's rights. So the lookup happens once, against the `PATH` this
//! process was started with (no code of aaf changes its own environment),
//! skips relative and empty entries (they mean the current directory), and
//! never picks a file inside the repository the process was started in.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The programs aaf runs by name.
const PROGRAMS: [&str; 2] = ["git", "gh"];

/// Longest `.git` file read: it holds one `gitdir:` line.
const GIT_FILE_MAX: u64 = 4096;

static RESOLVED: OnceLock<[Option<PathBuf>; 2]> = OnceLock::new();

fn resolved() -> &'static [Option<PathBuf>; 2] {
    RESOLVED.get_or_init(|| {
        let path_var = std::env::var_os("PATH").unwrap_or_default();
        let excluded = std::env::current_dir()
            .map(|cwd| repository_dirs(&cwd))
            // No current directory: there is no repository to leave out.
            .unwrap_or_default();
        PROGRAMS.map(|name| find_program(&path_var, name, &excluded))
    })
}

/// Looks every program up now. The `aaf` binary calls it first thing, so the
/// paths are those of its start, before any command of it has run.
pub fn resolve_programs() {
    resolved();
}

/// Absolute path of `name` (`git` or `gh`) as looked up once for this
/// process. `NotFound` when no usable one was on `PATH`.
pub fn program(name: &str) -> io::Result<&'static Path> {
    let index = PROGRAMS
        .iter()
        .position(|known| *known == name)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name} is not a program aaf looks up"),
            )
        })?;
    resolved()[index].as_deref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{name} not found on PATH (relative entries and directories inside this \
                 repository are not searched)"
            ),
        )
    })
}

/// First executable file `name` in the absolute directories of `path_var`
/// that lies outside every directory of `excluded` (canonical paths), as
/// written and wherever its links lead.
pub fn find_program(path_var: &OsStr, name: &str, excluded: &[PathBuf]) -> Option<PathBuf> {
    std::env::split_paths(path_var)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate) && !inside(candidate, excluded))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn inside(candidate: &Path, excluded: &[PathBuf]) -> bool {
    // A file that cannot be resolved is not trusted either.
    let Ok(real) = fs::canonicalize(candidate) else {
        return true;
    };
    let real_dir = candidate
        .parent()
        .and_then(|dir| fs::canonicalize(dir).ok());
    excluded.iter().any(|root| {
        candidate.starts_with(root)
            || real.starts_with(root)
            || real_dir.as_ref().is_some_and(|dir| dir.starts_with(root))
    })
}

/// The work tree enclosing `cwd` and, when that is a linked worktree, the
/// main work tree of its repository: canonical paths, found without running
/// git (git is what is being looked up). Empty outside a repository.
pub fn repository_dirs(cwd: &Path) -> Vec<PathBuf> {
    let Ok(cwd) = fs::canonicalize(cwd) else {
        return Vec::new();
    };
    let Some(top) = cwd
        .ancestors()
        .find(|dir| fs::symlink_metadata(dir.join(".git")).is_ok())
    else {
        return Vec::new();
    };
    let mut dirs = vec![top.to_path_buf()];
    if let Some(main) = main_work_tree(top)
        && !dirs.contains(&main)
    {
        dirs.push(main);
    }
    dirs
}

/// For a linked worktree (`.git` is a file naming its git dir), the work
/// tree that holds the shared `.git` directory.
fn main_work_tree(top: &Path) -> Option<PathBuf> {
    let mut text = String::new();
    fs::File::open(top.join(".git"))
        .ok()?
        .take(GIT_FILE_MAX)
        .read_to_string(&mut text)
        .ok()?;
    let git_dir = top.join(text.trim().strip_prefix("gitdir:")?.trim());
    // `commondir` names the shared git dir, relative to the worktree's own.
    let common = fs::read_to_string(git_dir.join("commondir")).ok()?;
    let common = fs::canonicalize(git_dir.join(common.trim())).ok()?;
    if common.file_name()? != ".git" {
        // A bare repository has no work tree of its own.
        return None;
    }
    common.parent().map(Path::to_path_buf)
}
