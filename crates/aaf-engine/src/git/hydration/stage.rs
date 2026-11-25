//! Host-only secret staging (`GateSecretStage`) and external clone/setup
//! execution helpers for [`super`].

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use aaf_domain::worktree_config::WorktreeConfig;

pub use super::cleanup::remove_partial_dst;
pub(super) use super::cleanup::{cleanup_or_git_err, cleanup_or_skip, truncate_stderr_first_line};
#[cfg(target_os = "macos")]
use super::device::same_cow_device;
use super::{load_base_worktree_config, resolve_entry};
use crate::git::cmd;
use crate::git::{GitError, path_str};

const CLONE_TIMEOUT: Duration = Duration::from_secs(300);
const SETUP_CMD_TIMEOUT: Duration = Duration::from_secs(300);
#[cfg(not(target_os = "macos"))]
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BATCH_ARGS: usize = 256;
const MAX_BATCH_BYTES: usize = 64 * 1024;
const SCRUBBED_GIT_ENV: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_PREFIX",
];

/// Host-only RAII guard for `[worktree].gate_copy_files`.
#[derive(Debug)]
pub struct GateSecretStage {
    staged: Vec<PathBuf>,
}

impl GateSecretStage {
    pub fn staged_paths(&self) -> &[PathBuf] {
        &self.staged
    }
}

impl Drop for GateSecretStage {
    fn drop(&mut self) {
        for path in self.staged.iter().rev() {
            let _ = fs::remove_file(path);
        }
    }
}

/// Reason a strict copy-on-write clone was skipped or aborted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneSkip {
    pub reason: String,
    /// True once clone creation has touched `dst`; callers must NEVER delete `dst` when false.
    pub created: bool,
    /// True when `dst` was created and subsequent cleanup failed.
    pub cleanup_failed: bool,
}

impl CloneSkip {
    /// Pre-creation skip (`dst` was never created or modified by this call).
    pub fn pre(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            created: false,
            cleanup_failed: false,
        }
    }

    /// Post-creation skip (`dst` was created by this call and already cleaned, or needs cleanup).
    pub fn after_create(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            created: true,
            cleanup_failed: false,
        }
    }

    /// Post-creation skip where cleaning `dst` itself failed.
    pub fn cleanup_failed(cleanup_err: impl Into<String>, orig_reason: impl Into<String>) -> Self {
        Self {
            reason: format!(
                "cleanup_failed: {} (after {})",
                cleanup_err.into(),
                orig_reason.into()
            ),
            created: true,
            cleanup_failed: true,
        }
    }
}

impl std::fmt::Display for CloneSkip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for CloneSkip {}

/// Stages `[worktree].gate_copy_files` (secrets allowed) strictly on the host
/// for gate execution and removes all staged files on [`Drop`].
pub fn stage_for_gate(
    repo_root: &Path,
    wt_root: &Path,
    cfg: &WorktreeConfig,
) -> Result<GateSecretStage, GitError> {
    let mut stage = GateSecretStage { staged: Vec::new() };
    for entry in &cfg.gate_copy_files {
        let (src, dst) = resolve_entry(repo_root, wt_root, entry, false, "gate_copy_files")?;
        if fs::symlink_metadata(&dst).is_ok() {
            return Err(GitError::AlreadyExists(dst.display().to_string()));
        }
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&src, &dst)?;
        stage.staged.push(dst);
    }
    Ok(stage)
}

/// Loads `[worktree]` from immutable `base_sha` and stages `gate_copy_files`
/// from the common main checkout into `wt_root` for the duration of host gate
/// command execution (cleaned up automatically on `Drop`).
pub(crate) fn stage_for_gate_from_base(
    wt_root: &Path,
    base_sha: Option<&str>,
) -> Result<Option<GateSecretStage>, GitError> {
    let Some(sha) = base_sha else {
        return Ok(None);
    };
    let main_root = crate::confine_core::git_refs::common_repo_root(wt_root)
        .unwrap_or_else(|| wt_root.to_path_buf());
    let cfg = load_base_worktree_config(&main_root, sha)?;
    if cfg.gate_copy_files.is_empty() {
        return Ok(None);
    }
    stage_for_gate(&main_root, wt_root, &cfg).map(Some)
}

pub(super) fn cp_program() -> &'static str {
    if Path::new("/bin/cp").is_file() {
        "/bin/cp"
    } else if Path::new("/usr/bin/cp").is_file() {
        "/usr/bin/cp"
    } else {
        "cp"
    }
}

fn check_same_device_strict(same_device: Result<bool, String>) -> Result<(), CloneSkip> {
    match same_device {
        Ok(true) => Ok(()),
        Ok(false) => Err(CloneSkip::pre(if cfg!(target_os = "macos") {
            "cross_device_or_non_apfs"
        } else {
            "cross_device"
        })),
        Err(err) => Err(CloneSkip::pre(format!("device_check_failed: {err}"))),
    }
}

fn is_single_normal_component(name: &OsStr) -> bool {
    let mut comps = Path::new(name).components();
    matches!(
        (comps.next(), comps.next()),
        (Some(Component::Normal(_)), None)
    )
}

/// Builds the OS-specific strict CoW `cp` command (`-cRP` on macOS,
/// `-a --reflink=always --no-dereference` on Linux).
fn strict_cp_cmd(cp_bin: &str) -> (Command, &'static str) {
    let mut cmd = Command::new(cp_bin);
    let label = if cfg!(target_os = "macos") {
        cmd.args(["-cRP", "--"]);
        "cp -cRP"
    } else {
        cmd.args(["-a", "--reflink=always", "--no-dereference", "--"]);
        "cp -a --reflink=always"
    };
    (cmd, label)
}

/// Runs a strict CoW `cp` command and cleans `dst` on any error or timeout.
fn run_strict_cp(
    cmd: &mut Command,
    label: &str,
    dst: &Path,
    timeout: Duration,
) -> Result<(), CloneSkip> {
    match run_external(cmd, label, timeout) {
        Ok(out) if out.success() => Ok(()),
        Ok(out) => Err(cleanup_or_skip(
            dst,
            format!(
                "cp_failed (exit {:?}): {}",
                out.code,
                truncate_stderr_first_line(&out.stderr)
            ),
        )),
        Err(GitError::Timeout { .. }) => Err(cleanup_or_skip(dst, "timeout".to_string())),
        Err(err) => Err(cleanup_or_skip(dst, format!("cp_error: {err}"))),
    }
}

/// Probes Linux reflink support INSIDE `gitignored_dir` (never in `dst_parent`,
/// so an abrupt kill can never leave untracked files for `git add -A`).
#[cfg(not(target_os = "macos"))]
fn probe_linux_reflink_inside(
    gitignored_dir: &Path,
    cp_bin: &str,
    timeout: Duration,
) -> Result<bool, String> {
    let pid = std::process::id();
    let probe_src = gitignored_dir.join(format!(".aaf-reflink-probe-{pid}-src"));
    let probe_dst = gitignored_dir.join(format!(".aaf-reflink-probe-{pid}-dst"));
    let _ = remove_partial_dst(&probe_src);
    let _ = remove_partial_dst(&probe_dst);
    fs::write(&probe_src, b"p").map_err(|e| format!("probe_write: {e}"))?;

    let mut cmd = Command::new(cp_bin);
    cmd.args(["--reflink=always", "--"])
        .arg(&probe_src)
        .arg(&probe_dst);
    let res = run_external(
        &mut cmd,
        "cp --reflink=always (probe)",
        timeout.min(PROBE_TIMEOUT),
    );

    let _ = remove_partial_dst(&probe_src);
    let _ = remove_partial_dst(&probe_dst);

    match res {
        Ok(out) => Ok(out.success()),
        Err(GitError::Timeout { .. }) => Err("timeout".to_string()),
        Err(err) => Err(err.to_string()),
    }
}

/// Evaluates Linux reflink support inside an already-created gitignored `probe_dir`.
/// When `reflink_ok` is `Some(flag)`, skips probing (used by fake-`cp` tests on Linux).
fn probe_or_skip(
    probe_dir: &Path,
    cp_bin: &str,
    timeout: Duration,
    reflink_ok: Option<bool>,
) -> Result<(), CloneSkip> {
    match reflink_ok {
        Some(true) => Ok(()),
        Some(false) => Err(cleanup_or_skip(
            probe_dir,
            "reflink_unsupported".to_string(),
        )),
        None => {
            #[cfg(target_os = "macos")]
            {
                let _ = (probe_dir, cp_bin, timeout);
                Ok(())
            }
            #[cfg(not(target_os = "macos"))]
            {
                match probe_linux_reflink_inside(probe_dir, cp_bin, timeout) {
                    Ok(true) => Ok(()),
                    Ok(false) => Err(cleanup_or_skip(
                        probe_dir,
                        "reflink_unsupported".to_string(),
                    )),
                    Err(err) => Err(cleanup_or_skip(
                        probe_dir,
                        format!("reflink_probe_failed: {err}"),
                    )),
                }
            }
        }
    }
}

/// Strict copy-on-write directory clone with injectable device check, `cp` binary,
/// and optional Linux reflink probe override (`None` = probe inside `dst`).
#[doc(hidden)]
pub fn cow_clone_dir_strict_with(
    src: &Path,
    dst: &Path,
    timeout: Duration,
    same_device: Result<bool, String>,
    cp_bin: &str,
    reflink_ok: Option<bool>,
) -> Result<(), CloneSkip> {
    check_same_device_strict(same_device)?;
    match fs::symlink_metadata(dst) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Ok(_) => return Err(CloneSkip::pre("dst_already_exists")),
        Err(err) => return Err(CloneSkip::pre(format!("dst_stat_error: {err}"))),
    }
    let deadline = Instant::now() + timeout;
    // On Linux (when probing), create `dst` temporarily to probe inside the gitignored
    // destination, then remove `dst` before `cp -a --reflink=always -- src dst`.
    let need_probe = match reflink_ok {
        None => cfg!(not(target_os = "macos")),
        Some(ok) => !ok,
    };
    if need_probe {
        match fs::create_dir(dst) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                return Err(CloneSkip::pre("dst_already_exists"));
            }
            Err(err) => return Err(CloneSkip::pre(format!("create_dst_failed: {err}"))),
        }
        probe_or_skip(dst, cp_bin, timeout, reflink_ok)?;
        if let Err(err) = remove_partial_dst(dst) {
            return Err(CloneSkip::cleanup_failed(err.to_string(), "probe_cleanup"));
        }
    }
    let rem = deadline
        .checked_duration_since(Instant::now())
        .unwrap_or(Duration::ZERO);
    if rem.is_zero() {
        return Err(CloneSkip::pre("timeout"));
    }
    let src_s = path_str(src).map_err(|e| CloneSkip::pre(e.to_string()))?;
    let dst_s = path_str(dst).map_err(|e| CloneSkip::pre(e.to_string()))?;
    let (mut cmd, label) = strict_cp_cmd(cp_bin);
    cmd.args([src_s, dst_s]);
    run_strict_cp(&mut cmd, label, dst, rem)
}

/// Strict copy-on-write clone of selected top-level entries from `src_dir` into
/// a newly created `dst_dir`, batched under `ARG_MAX` (256 args / 64 KiB).
#[doc(hidden)]
pub fn cow_clone_entries_strict_with(
    src_dir: &Path,
    entries: &[OsString],
    dst_dir: &Path,
    timeout: Duration,
    same_device: Result<bool, String>,
    cp_bin: &str,
    reflink_ok: Option<bool>,
) -> Result<(), CloneSkip> {
    if entries.is_empty() {
        return Err(CloneSkip::pre("empty_entries"));
    }
    for entry in entries {
        if !is_single_normal_component(entry) {
            return Err(CloneSkip::pre(format!("invalid_entry_name: {entry:?}")));
        }
    }
    check_same_device_strict(same_device)?;

    // Create `dst_dir` FIRST so pre-existing dst returns `dst_already_exists` without
    // deletion and any Linux reflink probe executes safely inside gitignored `dst_dir`.
    match fs::create_dir(dst_dir) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            return Err(CloneSkip::pre("dst_already_exists"));
        }
        Err(err) => return Err(CloneSkip::pre(format!("create_dst_failed: {err}"))),
    }

    let deadline = Instant::now() + timeout;
    probe_or_skip(dst_dir, cp_bin, timeout, reflink_ok)?;

    let dst_s = path_str(dst_dir)
        .map(|s| format!("{s}/"))
        .map_err(|e| cleanup_or_skip(dst_dir, e.to_string()))?;

    let mut idx = 0;
    while idx < entries.len() {
        let rem = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO);
        if rem.is_zero() {
            return Err(cleanup_or_skip(dst_dir, "timeout".to_string()));
        }
        let mut batch: Vec<PathBuf> = Vec::new();
        let mut batch_bytes = dst_s.len();
        while idx < entries.len() && batch.len() < MAX_BATCH_ARGS {
            let p = src_dir.join(&entries[idx]);
            let p_len = p.as_os_str().len() + 1;
            if !batch.is_empty() && batch_bytes + p_len > MAX_BATCH_BYTES {
                break;
            }
            batch_bytes += p_len;
            batch.push(p);
            idx += 1;
        }
        let (mut cmd, label) = strict_cp_cmd(cp_bin);
        for p in &batch {
            cmd.arg(p);
        }
        cmd.arg(&dst_s);
        run_strict_cp(&mut cmd, label, dst_dir, rem)?;
    }
    Ok(())
}

/// Backwards-compatible clone for explicit `[worktree].clone_dirs` entries.
pub(super) fn cow_clone_dir(src: &Path, dst: &Path) -> Result<(), GitError> {
    #[cfg(target_os = "macos")]
    let same_dev = {
        let dst_parent = dst.parent().unwrap_or(dst);
        same_cow_device(src, dst_parent).map_err(|e| e.to_string())
    };
    #[cfg(not(target_os = "macos"))]
    let same_dev = Ok(true);
    cow_clone_dir_with(src, dst, CLONE_TIMEOUT, same_dev, cp_program(), &mut |w| {
        eprintln!("aaf: warning: {w}");
    })
}

/// Testable core of [`cow_clone_dir`] with injectable device check, `cp` binary, and warning sink.
#[doc(hidden)]
pub fn cow_clone_dir_with(
    src: &Path,
    dst: &Path,
    timeout: Duration,
    same_device: Result<bool, String>,
    cp_bin: &str,
    warn: &mut dyn FnMut(&str),
) -> Result<(), GitError> {
    let (src_s, dst_s) = (path_str(src)?, path_str(dst)?);
    #[cfg(target_os = "macos")]
    {
        match &same_device {
            Ok(true) => {}
            Ok(false) => warn(&format!(
                "clone_dirs {} -> {} is cross-device or non-APFS; cp -cRP will deep copy",
                src.display(),
                dst.display()
            )),
            Err(err) => warn(&format!(
                "clone_dirs device check failed ({err}); still attempting cp -cRP"
            )),
        }
    }
    let try_primary = {
        #[cfg(target_os = "macos")]
        {
            true
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = same_device;
            // Probe inside gitignored `dst` (never `dst_parent`).
            match fs::create_dir(dst) {
                Ok(()) => {
                    let probe = probe_linux_reflink_inside(dst, cp_bin, timeout);
                    remove_partial_dst(dst)?;
                    match probe {
                        Ok(true) => true,
                        Ok(false) => {
                            warn(&format!(
                                "clone_dirs {} -> {} does not support reflink; using --reflink=never",
                                src.display(),
                                dst.display()
                            ));
                            false
                        }
                        Err(err) => {
                            warn(&format!(
                                "clone_dirs reflink probe failed ({err}); using --reflink=never"
                            ));
                            false
                        }
                    }
                }
                Err(_) => true,
            }
        }
    };
    if try_primary {
        let (mut cmd, label) = strict_cp_cmd(cp_bin);
        cmd.args([src_s, dst_s]);
        match run_external(&mut cmd, label, timeout) {
            Ok(out) if out.success() => return Ok(()),
            Err(err @ GitError::Timeout { .. }) => return Err(cleanup_or_git_err(dst, err)),
            Ok(out) => {
                remove_partial_dst(dst)?;
                warn(&format!(
                    "clone_dirs {label} failed (exit {:?}: {}); falling back to deep copy",
                    out.code,
                    truncate_stderr_first_line(&out.stderr)
                ));
            }
            Err(err) => {
                remove_partial_dst(dst)?;
                warn(&format!(
                    "clone_dirs {label} error ({err}); falling back to deep copy"
                ));
            }
        }
    }
    let mut fallback = Command::new(cp_bin);
    let fb_label = if cfg!(target_os = "macos") {
        fallback.args(["-RP", "--", src_s, dst_s]);
        "cp -RP"
    } else {
        fallback.args([
            "-a",
            "--reflink=never",
            "--no-dereference",
            "--",
            src_s,
            dst_s,
        ]);
        "cp -a --reflink=never"
    };
    match run_external(&mut fallback, fb_label, timeout) {
        Ok(out) if out.success() => Ok(()),
        Ok(out) => Err(cleanup_or_git_err(
            dst,
            GitError::Command {
                command: format!("{fb_label} -- {src_s} {dst_s}"),
                code: out.code,
                stderr: truncate_stderr_first_line(&out.stderr),
            },
        )),
        Err(err) => Err(cleanup_or_git_err(dst, err)),
    }
}

pub(super) fn run_setup_cmd(
    wt_root: &Path,
    setup_cmd: &str,
    data_dir: Option<&Path>,
) -> Result<(), GitError> {
    let (argv, _mode) = crate::confine_core::shell_argv(setup_cmd, wt_root, data_dir)?;
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).current_dir(wt_root);
    for var in SCRUBBED_GIT_ENV {
        cmd.env_remove(var);
    }
    let out = run_external(&mut cmd, "sh -c <setup_cmd>", SETUP_CMD_TIMEOUT)?;
    if out.success() {
        return Ok(());
    }
    Err(GitError::Command {
        command: format!("sh -c {setup_cmd:?}"),
        code: out.code,
        stderr: out.stderr.trim().to_string(),
    })
}

pub(super) fn run_external(
    cmd: &mut Command,
    label: &str,
    timeout: Duration,
) -> Result<cmd::Output, GitError> {
    cmd::run(cmd, timeout).map_err(|err| {
        if err.kind() == io::ErrorKind::TimedOut {
            GitError::Timeout {
                command: label.to_string(),
            }
        } else {
            GitError::Io(err)
        }
    })
}
