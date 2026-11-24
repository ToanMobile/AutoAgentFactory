//! Tree fingerprint, data-directory layout and HMAC-signed receipts and
//! approvals. Synchronous on purpose: both the gate and the hook use it.

mod hidden;
pub mod output;
mod proc;
mod signed;
mod stale;
#[cfg(test)]
mod tests;
mod tools;

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};
use std::{env, fs, io};

use sha2::{Digest, Sha256};

pub use hidden::{HiddenFiles, IgnoredFile, hidden_files};
use proc::Failure;
pub use signed::{
    APPROVAL_DOMAIN, AUDIT_DOMAIN, COMMON_RECEIPT_DOMAIN, PROOF_DOMAIN, RECEIPT_DOMAIN, Signed,
    approval_path, audit_verdict_path, check_store_receipt, check_store_receipt_in,
    common_receipt_path, load_key, load_or_create_key, proof_path, read_approval,
    read_audit_verdict, read_common_receipt, read_proof, read_receipt, receipt_path,
    remove_common_receipt, sign, sign_tagged, verify, verify_tagged, write_approval,
    write_audit_verdict, write_common_receipt, write_proof, write_receipt,
};
pub use tools::{find_program, program, repository_dirs, resolve_programs};

/// Default budget for one git call.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum FpError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("git {0}")]
    Git(String),
    #[error("json: {0}")]
    Json(String),
    #[error("signature does not match")]
    BadSignature,
    #[error("cannot locate the data directory: set AAF_DATA_DIR or HOME")]
    NoDataDir,
    #[error("signing key is missing or too short")]
    BadKey,
}

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

/// Streams `reader` in 8 KiB chunks and returns its lowercase hex SHA-256.
pub fn sha256_reader<R: Read>(reader: &mut R) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(hex(&hasher.finalize()))
}

/// `AAF_DATA_DIR`, or `~/.local/share/autofactory`.
pub fn data_dir() -> Result<PathBuf, FpError> {
    if let Some(dir) = env::var_os("AAF_DATA_DIR").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let home = env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .ok_or(FpError::NoDataDir)?;
    Ok(PathBuf::from(home).join(".local/share/autofactory"))
}

/// 16 hex chars identifying a repo root (or worktree root) on this machine.
pub fn repo_id(repo_root: &Path) -> String {
    let canonical = fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let mut id = sha256_hex(canonical.as_os_str().as_encoded_bytes());
    id.truncate(16);
    id
}

/// Runs git in `repo` with a deadline and returns trimmed stdout.
/// Inherited `GIT_*` location variables are dropped so a caller running
/// inside a git hook still addresses `repo`.
pub fn git(
    repo: &Path,
    args: &[&str],
    envs: &[(&str, &OsStr)],
    timeout: Duration,
) -> Result<String, FpError> {
    let label = args.first().copied().unwrap_or("");
    git_raw(&GitCall::plain(repo, timeout), args, envs, None)
        .map(|stdout| stdout.trim().to_string())
        .map_err(|failure| failure.into_error(label))
}

/// Where and how the git commands of one operation run.
pub(crate) struct GitCall<'a> {
    /// What a test puts in place of git; `None` is the git [`program`]
    /// looked up once for this process.
    pub program: Option<&'a OsStr>,
    pub repo: &'a Path,
    pub timeout: Duration,
}

impl<'a> GitCall<'a> {
    pub(crate) fn plain(repo: &'a Path, timeout: Duration) -> GitCall<'a> {
        GitCall {
            program: None,
            repo,
            timeout,
        }
    }
}

/// [`git`] with stdout as git wrote it (NUL-separated listings must not be
/// trimmed), an optional stdin, and the exit code and stderr of a failure.
pub(crate) fn git_raw(
    call: &GitCall<'_>,
    args: &[&str],
    envs: &[(&str, &OsStr)],
    input: Option<Vec<u8>>,
) -> Result<String, Failure> {
    let bin = match call.program {
        Some(stand_in) => stand_in,
        None => program("git")?.as_os_str(),
    };
    let mut cmd = Command::new(bin);
    // Every git command of aaf runs without auto gc (docs/PLAN.md C.8): a
    // repack must not start inside a call that has a deadline. Nor with an
    // fsmonitor: one named by a repository an agent created inside the work
    // tree would run in the `git status` git starts there (`-c` reaches it).
    cmd.arg("-C")
        .arg(call.repo)
        .args(["-c", "gc.auto=0", "-c", "core.fsmonitor=false"])
        .args(args);
    for var in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
        cmd.env_remove(var);
    }
    for (key, value) in envs {
        cmd.env(key, value);
    }
    let label = args.first().copied().unwrap_or("");
    proc::run(cmd, label, input, call.timeout)
}

/// Top-level directory of the work tree containing `cwd`.
pub fn repo_root(cwd: &Path) -> Result<PathBuf, FpError> {
    git(cwd, &["rev-parse", "--show-toplevel"], &[], GIT_TIMEOUT).map(PathBuf::from)
}

/// `HEAD` commit, or `None` in a repo with no commit yet.
pub fn head_sha(repo_root: &Path) -> Option<String> {
    git(
        repo_root,
        &["rev-parse", "--verify", "HEAD"],
        &[],
        GIT_TIMEOUT,
    )
    .ok()
}

/// [`git`] for a question whose "none" git gives as exit 1 without a word
/// (`rev-parse --verify --quiet`, `merge-base` of unrelated histories):
/// `Ok(None)` then, and for an empty answer. A git that could not answer
/// (no repository, a timeout, a crash) is an error: taken for "none" it
/// would move what the gate compares against.
pub fn git_or_none(
    repo_root: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<Option<String>, FpError> {
    match git_raw(&GitCall::plain(repo_root, timeout), args, &[], None) {
        Ok(stdout) => Ok(Some(stdout.trim().to_string()).filter(|out| !out.is_empty())),
        Err(Failure::Exit {
            code: Some(1),
            stderr,
        }) if stderr.is_empty() => Ok(None),
        Err(failure) => Err(failure.into_error(args.first().copied().unwrap_or(""))),
    }
}

static TEMP_INDEX_SEQ: AtomicU64 = AtomicU64::new(0);

/// Resolved git repository paths needed by fingerprinting and worktree lookups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitLayout {
    pub git_dir: PathBuf,
    pub common_dir: PathBuf,
    pub index: PathBuf,
    pub objects: PathBuf,
}

impl GitLayout {
    /// Main repository checkout root derived from `common_dir`.
    pub fn common_repo_root(&self) -> &Path {
        match self.common_dir.parent() {
            Some(parent)
                if self
                    .common_dir
                    .file_name()
                    .is_some_and(|name| name == ".git") =>
            {
                parent
            }
            _ => &self.common_dir,
        }
    }
}

/// Probes `git_dir`, `common_dir`, `index`, and `objects` in a single
/// `git rev-parse` invocation, falling back to individual calls if the
/// installed git does not return 4 lines for `--path-format=absolute`.
pub fn probe_git_layout(repo_root: &Path, timeout: Duration) -> Result<GitLayout, FpError> {
    probe_git_layout_using(&GitCall::plain(repo_root, timeout))
}

fn resolve_git_path(repo_root: &Path, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo_root.join(path)
    }
}

fn probe_git_layout_using(call: &GitCall<'_>) -> Result<GitLayout, FpError> {
    let single_args = [
        "rev-parse",
        "--path-format=absolute",
        "--git-dir",
        "--git-common-dir",
        "--git-path",
        "index",
        "--git-path",
        "objects",
    ];
    match git_raw(call, &single_args, &[], None) {
        Ok(stdout) => {
            let lines: Vec<&str> = stdout.lines().filter(|line| !line.is_empty()).collect();
            if lines.len() == 4 {
                return Ok(GitLayout {
                    git_dir: resolve_git_path(call.repo, lines[0]),
                    common_dir: resolve_git_path(call.repo, lines[1]),
                    index: resolve_git_path(call.repo, lines[2]),
                    objects: resolve_git_path(call.repo, lines[3]),
                });
            }
        }
        Err(Failure::Other(err)) => return Err(err),
        Err(Failure::Exit { .. }) => {}
    }
    let run = |args: &[&str]| -> Result<String, FpError> {
        let label = args.first().copied().unwrap_or("");
        git_raw(call, args, &[], None)
            .map(|stdout| stdout.trim().to_string())
            .map_err(|failure| failure.into_error(label))
    };
    let git_dir = PathBuf::from(run(&["rev-parse", "--absolute-git-dir"])?);
    let common_dir = resolve_git_path(call.repo, &run(&["rev-parse", "--git-common-dir"])?);
    let index = resolve_git_path(call.repo, &run(&["rev-parse", "--git-path", "index"])?);
    let objects = resolve_git_path(call.repo, &run(&["rev-parse", "--git-path", "objects"])?);
    Ok(GitLayout {
        git_dir,
        common_dir,
        index,
        objects,
    })
}

/// Content fingerprint of the work tree: the git tree hash of every tracked
/// and untracked, non-ignored file. Committing the same content does not
/// change it; adding, editing or deleting any such file does.
///
/// Nothing is left in the repository: the index is a temporary copy, and the
/// blobs and trees git has to write for files it has never stored go to a
/// temporary object directory that has the real store as its alternate. The
/// hash is the same either way; only where the objects land differs.
pub fn tree_fp(repo_root: &Path, timeout: Duration) -> Result<String, FpError> {
    tree_fp_using(&GitCall::plain(repo_root, timeout))
}

/// [`tree_fp`] using a previously probed [`GitLayout`], avoiding redundant
/// `git rev-parse` subprocesses.
pub fn tree_fp_with_layout(
    repo_root: &Path,
    layout: &GitLayout,
    timeout: Duration,
) -> Result<String, FpError> {
    tree_fp_with_layout_using(&GitCall::plain(repo_root, timeout), layout)
}

fn tree_fp_using(call: &GitCall<'_>) -> Result<String, FpError> {
    let layout = probe_git_layout_using(call)?;
    tree_fp_with_layout_using(call, &layout)
}

fn tree_fp_with_layout_using(call: &GitCall<'_>, layout: &GitLayout) -> Result<String, FpError> {
    let git = |args: &[&str], envs: &[(&str, &OsStr)]| -> Result<String, FpError> {
        let label = args.first().copied().unwrap_or("");
        git_raw(call, args, envs, None)
            .map(|stdout| stdout.trim().to_string())
            .map_err(|failure| failure.into_error(label))
    };
    // A run that was killed could not clean up after itself.
    stale::sweep(&layout.git_dir, std::process::id(), &stale::process_alive);
    let seq = TEMP_INDEX_SEQ.fetch_add(1, Ordering::Relaxed);
    let tag = format!("{}-{seq}", std::process::id());
    let temp = layout.git_dir.join(format!("{}{tag}", stale::INDEX_PREFIX));
    let objects = layout
        .git_dir
        .join(format!("{}{tag}", stale::OBJECTS_PREFIX));
    let tree = (|| {
        fs::create_dir_all(&objects)?;
        if layout.index.exists() {
            // Start from the real index so unchanged files are not re-hashed.
            fs::copy(&layout.index, &temp)?;
        }
        let env = [
            ("GIT_INDEX_FILE", temp.as_os_str()),
            ("GIT_OBJECT_DIRECTORY", objects.as_os_str()),
            (
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                layout.objects.as_os_str(),
            ),
        ];
        // `--sparse`: in a sparse checkout a plain `git add -A` exits 1 as
        // soon as a file on disk lies outside the sparse definition. Entries
        // the sparse checkout left out of the work tree stay in the index as
        // they are. Without a sparse checkout the flag changes nothing.
        let add = ["add", "-A", "--sparse"];
        git_raw(call, &add, &env, None).map_err(|failure| match failure {
            // 129 is git's "usage" exit: this `git add` has no `--sparse`.
            Failure::Exit {
                code: Some(129),
                stderr,
            } => {
                let installed = git(&["--version"], &[]).unwrap_or_default();
                FpError::Git(format!(
                    "add --sparse: exit 129: {stderr}; the fingerprint needs git 2.34 or \
                     newer for `git add --sparse`, and this is {installed}"
                ))
            }
            failure => failure.into_error("add"),
        })?;
        git(&["write-tree"], &env)
    })();
    // Cleanup only: the fingerprint is already decided.
    let _ = fs::remove_file(&temp);
    let _ = fs::remove_file(temp.with_extension("lock"));
    let _ = fs::remove_dir_all(&objects);
    Ok(format!("git-tree:{}", tree?))
}

const LEGACY_FP_PREFIX: &str = "aaf-fp-";
const STALE_FP_AGE: Duration = Duration::from_secs(3600);

/// Removes stale temporary index/object entries left behind in `repo_root`'s
/// git directory (`aaf-index-*`, `aaf-objects-*`, and stale `aaf-fp-*`).
/// Entries owned by a live process are never removed.
pub fn sweep_stale(repo_root: &Path) -> Result<usize, FpError> {
    let git_dir = git(
        repo_root,
        &["rev-parse", "--absolute-git-dir"],
        &[],
        GIT_TIMEOUT,
    )?;
    sweep_stale_in(
        Path::new(&git_dir),
        std::process::id(),
        &stale::process_alive,
        SystemTime::now(),
    )
}

fn stale_pid(name: &str) -> Option<u32> {
    let rest = name
        .strip_prefix(stale::INDEX_PREFIX)
        .or_else(|| name.strip_prefix(stale::OBJECTS_PREFIX))?;
    rest.split('-').next()?.parse().ok()
}

fn should_remove_legacy_fp(
    name: &str,
    path: &Path,
    own_pid: u32,
    is_alive: &mut impl FnMut(u32) -> bool,
    now: SystemTime,
) -> bool {
    let Some(rest) = name.strip_prefix(LEGACY_FP_PREFIX) else {
        return false;
    };
    if let Some(pid) = rest.split('-').next().and_then(|p| p.parse::<u32>().ok()) {
        return pid != own_pid && !is_alive(pid);
    }
    fs::symlink_metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age >= STALE_FP_AGE)
}

fn sweep_stale_in(
    git_dir: &Path,
    own_pid: u32,
    alive: &dyn Fn(u32) -> bool,
    now: SystemTime,
) -> Result<usize, FpError> {
    let mut removed_count = 0usize;
    let mut alive_cache: HashMap<u32, bool> = HashMap::new();
    let mut is_alive = |pid: u32| *alive_cache.entry(pid).or_insert_with(|| alive(pid));
    for entry in fs::read_dir(git_dir)?.filter_map(Result::ok) {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let path = entry.path();
        let eligible = if let Some(pid) = stale_pid(name_str) {
            pid != own_pid && !is_alive(pid)
        } else {
            should_remove_legacy_fp(name_str, &path, own_pid, &mut is_alive, now)
        };
        if !eligible {
            continue;
        }
        // `symlink_metadata`: a symlink planted under a temp name is unlinked,
        // never followed into its target.
        let removed = match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => fs::remove_dir_all(&path),
            Ok(_) => fs::remove_file(&path),
            Err(err) => Err(err),
        };
        if removed.is_ok() {
            removed_count += 1;
        }
    }
    Ok(removed_count)
}
