//! Host-only secret staging (`GateSecretStage`) and external clone/setup
//! execution helpers for [`super`].

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use aaf_domain::worktree_config::WorktreeConfig;

use super::{load_base_worktree_config, resolve_entry};
use crate::git::cmd;
use crate::git::{GitError, path_str};

const CLONE_TIMEOUT: Duration = Duration::from_secs(300);
const SETUP_CMD_TIMEOUT: Duration = Duration::from_secs(300);
const SCRUBBED_GIT_ENV: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_PREFIX",
];

/// Host-only RAII guard for `[worktree].gate_copy_files`.
///
/// Staged files (which may include credentials such as `.env` or
/// `google-services.json`) are removed automatically on [`Drop`] so they are
/// never left behind in the worktree for subsequent agent turns.
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

fn cp_program() -> &'static str {
    if Path::new("/bin/cp").is_file() {
        "/bin/cp"
    } else if Path::new("/usr/bin/cp").is_file() {
        "/usr/bin/cp"
    } else {
        "cp"
    }
}

pub(super) fn cow_clone_dir(src: &Path, dst: &Path) -> Result<(), GitError> {
    let cp_bin = cp_program();
    let (src_s, dst_s) = (path_str(src)?, path_str(dst)?);
    if cfg!(target_os = "macos") {
        let mut cmd = Command::new(cp_bin);
        cmd.args(["-cRP", "--", src_s, dst_s]);
        if run_external(&mut cmd, "cp -cRP", CLONE_TIMEOUT)?.success() {
            return Ok(());
        }
        // Cross-volume or non-APFS destinations return EXDEV for clonefile(2);
        // clean any partial target and fall back to deep copy (never hardlinks).
        remove_partial_dst(dst);
        let mut fallback = Command::new(cp_bin);
        fallback.args(["-RP", "--", src_s, dst_s]);
        let out = run_external(&mut fallback, "cp -RP", CLONE_TIMEOUT)?;
        if out.success() {
            return Ok(());
        }
        Err(GitError::Command {
            command: format!("cp -RP -- {src_s} {dst_s}"),
            code: out.code,
            stderr: out.stderr.trim().to_string(),
        })
    } else {
        let mut cmd = Command::new(cp_bin);
        cmd.args([
            "-a",
            "--reflink=auto",
            "--no-dereference",
            "--",
            src_s,
            dst_s,
        ]);
        let out = run_external(&mut cmd, "cp -a --reflink=auto", CLONE_TIMEOUT)?;
        if out.success() {
            return Ok(());
        }
        remove_partial_dst(dst);
        Err(GitError::Command {
            command: format!("cp -a --reflink=auto --no-dereference -- {src_s} {dst_s}"),
            code: out.code,
            stderr: out.stderr.trim().to_string(),
        })
    }
}

fn remove_partial_dst(dst: &Path) {
    if let Ok(meta) = fs::symlink_metadata(dst) {
        if meta.is_dir() {
            let _ = fs::remove_dir_all(dst);
        } else {
            let _ = fs::remove_file(dst);
        }
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

fn run_external(
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
