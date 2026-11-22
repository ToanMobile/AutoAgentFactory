//! Tree fingerprint, data-directory layout and HMAC-signed receipts and
//! approvals. Synchronous on purpose: both the gate and the hook use it.

mod hidden;
pub mod output;
mod proc;
mod stale;
#[cfg(test)]
mod tests;
mod tools;

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::{env, fs, io};

use aaf_domain::gate::{Approval, GateMode, GateStatus, Receipt, StoreStatus};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use hidden::{HiddenFiles, IgnoredFile, hidden_files};
use proc::Failure;
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

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let nibble = |c: u8| {
        char::from(c)
            .to_digit(16)
            .and_then(|d| u8::try_from(d).ok())
    };
    text.as_bytes()
        .chunks(2)
        .map(|pair| Some(nibble(pair[0])? << 4 | nibble(pair[1])?))
        .collect()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
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
    // repack must not start inside a call that has a deadline.
    cmd.arg("-C")
        .arg(call.repo)
        .args(["-c", "gc.auto=0"])
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

fn tree_fp_using(call: &GitCall<'_>) -> Result<String, FpError> {
    let repo_root = call.repo;
    let git = |args: &[&str], envs: &[(&str, &OsStr)]| -> Result<String, FpError> {
        let label = args.first().copied().unwrap_or("");
        git_raw(call, args, envs, None)
            .map(|stdout| stdout.trim().to_string())
            .map_err(|failure| failure.into_error(label))
    };
    let git_dir = git(&["rev-parse", "--absolute-git-dir"], &[])?;
    // A run that was killed could not clean up after itself.
    stale::sweep(
        Path::new(&git_dir),
        std::process::id(),
        &stale::process_alive,
    );
    let git_path = |name: &str| -> Result<PathBuf, FpError> {
        let path = git(&["rev-parse", "--git-path", name], &[])?;
        Ok(if Path::new(&path).is_absolute() {
            PathBuf::from(path)
        } else {
            repo_root.join(path)
        })
    };
    let index = git_path("index")?;
    // Of the main repository when `repo_root` is a linked worktree.
    let store = git_path("objects")?;
    let seq = TEMP_INDEX_SEQ.fetch_add(1, Ordering::Relaxed);
    let tag = format!("{}-{seq}", std::process::id());
    let temp = Path::new(&git_dir).join(format!("{}{tag}", stale::INDEX_PREFIX));
    let objects = Path::new(&git_dir).join(format!("{}{tag}", stale::OBJECTS_PREFIX));
    let tree = (|| {
        fs::create_dir_all(&objects)?;
        if index.exists() {
            // Start from the real index so unchanged files are not re-hashed.
            fs::copy(&index, &temp)?;
        }
        let env = [
            ("GIT_INDEX_FILE", temp.as_os_str()),
            ("GIT_OBJECT_DIRECTORY", objects.as_os_str()),
            ("GIT_ALTERNATE_OBJECT_DIRECTORIES", store.as_os_str()),
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

const KEY_FILE: &str = "receipt.key";
const KEY_LEN: usize = 32;

/// Reads the signing key. The hook uses this: a missing key means no valid
/// receipt can exist.
pub fn load_key(data_dir: &Path) -> Result<Vec<u8>, FpError> {
    let key = fs::read(data_dir.join(KEY_FILE)).map_err(|_| FpError::BadKey)?;
    if key.len() < KEY_LEN {
        return Err(FpError::BadKey);
    }
    Ok(key)
}

/// Reads the signing key, creating it (mode 0600) on first use.
pub fn load_or_create_key(data_dir: &Path) -> Result<Vec<u8>, FpError> {
    if let Ok(key) = load_key(data_dir) {
        return Ok(key);
    }
    // The key signs what may merge: its directory is the owner's alone.
    private_dir(data_dir)?;
    let mut key = [0u8; KEY_LEN];
    fs::File::open("/dev/urandom")?.read_exact(&mut key)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(data_dir.join(KEY_FILE)) {
        Ok(mut file) => {
            file.write_all(&key)?;
            Ok(key.to_vec())
        }
        // Another process created it first: use theirs.
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => load_key(data_dir),
        Err(err) => Err(err.into()),
    }
}

/// A JSON payload and the HMAC-SHA256 of its exact bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signed {
    pub payload: String,
    pub hmac: String,
}

/// Domain tag of a gate receipt signature. The tag is fed to the HMAC before
/// the payload, so a signature made for one kind of document never verifies
/// as another. The trailing NUL cannot occur at the start of a JSON payload.
pub const RECEIPT_DOMAIN: &str = "receipt\0";
/// Domain tag of a head-lock approval signature.
pub const APPROVAL_DOMAIN: &str = "approval\0";

fn mac_for(key: &[u8], domain: &str, payload: &str) -> Result<Hmac<Sha256>, FpError> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|_| FpError::BadKey)?;
    mac.update(domain.as_bytes());
    mac.update(payload.as_bytes());
    Ok(mac)
}

/// Untagged signature. Receipts and approvals do not use it: see
/// [`sign_tagged`].
pub fn sign(key: &[u8], payload: String) -> Result<Signed, FpError> {
    sign_tagged(key, "", payload)
}

/// Constant-time check that `signed.hmac` matches `signed.payload`, untagged.
pub fn verify(key: &[u8], signed: &Signed) -> bool {
    verify_tagged(key, "", signed)
}

/// Signs `payload` for one kind of document ([`RECEIPT_DOMAIN`],
/// [`APPROVAL_DOMAIN`]).
pub fn sign_tagged(key: &[u8], domain: &str, payload: String) -> Result<Signed, FpError> {
    let hmac = hex(&mac_for(key, domain, &payload)?.finalize().into_bytes());
    Ok(Signed { payload, hmac })
}

/// Constant-time check that `signed` was made by [`sign_tagged`] with the
/// same key and `domain`.
pub fn verify_tagged(key: &[u8], domain: &str, signed: &Signed) -> bool {
    let Some(expected) = unhex(&signed.hmac) else {
        return false;
    };
    mac_for(key, domain, &signed.payload).is_ok_and(|mac| mac.verify_slice(&expected).is_ok())
}

pub fn receipt_path(data_dir: &Path, repo_root: &Path) -> PathBuf {
    data_dir
        .join("receipts")
        .join(repo_id(repo_root))
        .join("latest.json")
}

pub fn approval_path(data_dir: &Path, repo_root: &Path, lock_fp: &str) -> PathBuf {
    let safe: String = lock_fp
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();
    data_dir
        .join("approvals")
        .join(repo_id(repo_root))
        .join(format!("{safe}.json"))
}

/// Creates `dir` and its missing parents for the owner only, and closes an
/// existing `dir` that an earlier version (or the umask) left open.
fn private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        builder.mode(0o700);
        builder.create(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    builder.create(dir)
}

/// Signs `value` and stores it at `path`, somewhere below `data_dir`.
///
/// Receipts and approvals decide what may merge and are nobody else's to
/// read: the file is 0600 and every directory from the data dir down to it
/// is 0700, whatever the umask says and whatever was there before. (This
/// crate cannot use the helper of `aaf-engine`, which depends on it.)
fn write_signed<T: Serialize>(
    data_dir: &Path,
    path: &Path,
    key: &[u8],
    domain: &str,
    value: &T,
) -> Result<(), FpError> {
    let payload = serde_json::to_string(value).map_err(|e| FpError::Json(e.to_string()))?;
    let signed = sign_tagged(key, domain, payload)?;
    let body = serde_json::to_vec_pretty(&signed).map_err(|e| FpError::Json(e.to_string()))?;
    let mut dirs: Vec<&Path> = path
        .ancestors()
        .skip(1)
        .take_while(|dir| dir.starts_with(data_dir))
        .collect();
    // Outermost first, so a new directory is created inside a closed one.
    dirs.reverse();
    for dir in dirs {
        private_dir(dir)?;
    }
    // Write-then-rename so a reader never sees a half-written file; the
    // rename also replaces a file that was left readable by others.
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    #[cfg(unix)]
    {
        // The mode above only applies to a file this call created.
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(&body)?;
    drop(file);
    fs::rename(&temp, path)?;
    Ok(())
}

fn read_signed<T: for<'de> Deserialize<'de>>(
    path: &Path,
    key: &[u8],
    domain: &str,
) -> Result<T, FpError> {
    let body = fs::read(path)?;
    let signed: Signed = serde_json::from_slice(&body).map_err(|e| FpError::Json(e.to_string()))?;
    if !verify_tagged(key, domain, &signed) {
        return Err(FpError::BadSignature);
    }
    serde_json::from_str(&signed.payload).map_err(|e| FpError::Json(e.to_string()))
}

/// Signs and stores the receipt as the latest one for its repo root.
pub fn write_receipt(data_dir: &Path, key: &[u8], receipt: &Receipt) -> Result<PathBuf, FpError> {
    let path = receipt_path(data_dir, Path::new(&receipt.repo_root));
    write_signed(data_dir, &path, key, RECEIPT_DOMAIN, receipt)?;
    Ok(path)
}

/// Loads the latest receipt for `repo_root`; fails unless its signature holds.
pub fn read_receipt(data_dir: &Path, key: &[u8], repo_root: &Path) -> Result<Receipt, FpError> {
    read_signed(&receipt_path(data_dir, repo_root), key, RECEIPT_DOMAIN)
}

pub fn write_approval(
    data_dir: &Path,
    key: &[u8],
    repo_root: &Path,
    approval: &Approval,
) -> Result<PathBuf, FpError> {
    let path = approval_path(data_dir, repo_root, &approval.lock_fp);
    write_signed(data_dir, &path, key, APPROVAL_DOMAIN, approval)?;
    Ok(path)
}

/// `Ok(None)` when no approval file exists; an unsigned or tampered file is
/// an error, never an approval.
pub fn read_approval(
    data_dir: &Path,
    key: &[u8],
    repo_root: &Path,
    lock_fp: &str,
) -> Result<Option<Approval>, FpError> {
    let path = approval_path(data_dir, repo_root, lock_fp);
    if !path.exists() {
        return Ok(None);
    }
    read_signed(&path, key, APPROVAL_DOMAIN).map(Some)
}

/// The rule a store submission from `repo_root` must satisfy: the receipt was
/// issued for this very repository (canonical paths compared, so a receipt
/// copied from another repo's slot is refused) and passes
/// [`check_store_receipt`]. Returns the reason on refusal.
pub fn check_store_receipt_in(
    repo_root: &Path,
    receipt: &Receipt,
    current_tree_fp: &str,
) -> Result<(), String> {
    let here = fs::canonicalize(repo_root)
        .map_err(|e| format!("the repository root cannot be resolved ({e})"))?;
    // A path that no longer resolves cannot be this repository.
    if fs::canonicalize(&receipt.repo_root).ok().as_ref() != Some(&here) {
        return Err("the receipt was issued for another repository".to_string());
    }
    check_store_receipt(receipt, current_tree_fp)
}

/// The content rules of a store receipt, without the repository binding:
/// callers that know the repo root use [`check_store_receipt_in`].
pub fn check_store_receipt(receipt: &Receipt, current_tree_fp: &str) -> Result<(), String> {
    if receipt.tree_fp != current_tree_fp {
        return Err("the work tree changed since the last gate".to_string());
    }
    if receipt.mode != GateMode::Release {
        return Err("the last gate was not a release gate".to_string());
    }
    if receipt.status != GateStatus::Clear {
        return Err(format!("the last gate ended {:?}", receipt.status));
    }
    if receipt.store_compliance.status != StoreStatus::Clear {
        return Err(format!(
            "store compliance is {:?}",
            receipt.store_compliance.status
        ));
    }
    Ok(())
}
