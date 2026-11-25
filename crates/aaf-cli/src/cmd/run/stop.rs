//! Cooperative graceful stop (`aaf run --stop`) via `<locks_dir>/<repo_id>.stop`.

use std::fs::{self, File, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use aaf_engine::db::private;
use aaf_engine::factory::RunSummary;
use serde_json::json;

use super::super::board::store::{err, repo_root};
use super::detach;
use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput, CmdResult};

const STOP_POLL_SLICE: Duration = Duration::from_millis(100);

/// Path of the cooperative stop sentinel for `repo` (same `repo_id` as `.run.lock`).
pub(crate) fn stop_file_path(data_dir: &Path, repo: &Path) -> PathBuf {
    data_dir
        .join("locks")
        .join(format!("{}.stop", aaf_fp::repo_id(repo)))
}

fn remove_if_present(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Removes any stale `.stop` file when `aaf run` starts under `.run.lock`,
/// checks it cooperatively during the run, and deletes it on clean exit.
pub(crate) struct StopFileGuard {
    path: PathBuf,
    stopped: AtomicBool,
}

impl StopFileGuard {
    pub(crate) fn arm(data_dir: &Path, repo: &Path) -> Result<Self, CmdError> {
        let path = stop_file_path(data_dir, repo);
        remove_if_present(&path).map_err(|e| err(&path.display().to_string(), e))?;
        Ok(Self {
            path,
            stopped: AtomicBool::new(false),
        })
    }

    pub(crate) fn should_stop(&self) -> bool {
        if self.stopped.load(Ordering::Acquire) {
            return true;
        }
        if self.path.exists() {
            self.stopped.store(true, Ordering::Release);
            return true;
        }
        false
    }

    pub(crate) fn was_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
}

impl Drop for StopFileGuard {
    fn drop(&mut self) {
        let _ = remove_if_present(&self.path);
    }
}

/// Sleeps up to `total` in 100ms slices, returning `true` immediately if
/// `should_stop()` becomes true.
pub(crate) fn sleep_or_stop(total: Duration, should_stop: &dyn Fn() -> bool) -> bool {
    let deadline = Instant::now() + total;
    loop {
        if should_stop() {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep((deadline - now).min(STOP_POLL_SLICE));
    }
}

/// Annotates `aaf run` output when it drained and exited cleanly due to `.stop`.
pub(crate) fn apply_stopped_report(
    mut out: CmdOutput,
    was_stopped: bool,
    summary: &RunSummary,
) -> CmdOutput {
    if was_stopped {
        out = out.field("stopped", json!(true));
        if !summary.frozen && summary.halted.is_none() && summary.blocked.is_empty() {
            out = out.field("drain_stop_reason", json!("stopped"));
        }
        out.text
            .push_str("\nstopped: graceful stop requested (aaf run --stop)");
    }
    out
}

/// `aaf run --stop`: checks whether `.run.lock` is held for the current
/// repository and, only when an `aaf run` process is live, creates
/// `<locks_dir>/<repo_id>.stop`.
pub(crate) fn request_stop(ctx: &Ctx) -> CmdResult {
    let repo = repo_root(ctx)?;
    let lock_path = detach::run_lock_path(&ctx.data_dir, &repo);
    let is_running = if !lock_path.exists() {
        false
    } else {
        let file = File::options()
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|e| err(&lock_path.display().to_string(), e))?;
        match file.try_lock() {
            Ok(()) => false,
            Err(TryLockError::WouldBlock) => true,
            Err(TryLockError::Error(e)) => {
                return Err(err(&lock_path.display().to_string(), e));
            }
        }
    };

    if !is_running {
        return Ok(CmdOutput::ok(
            "ok",
            format!("no running aaf run found for {}", repo.display()),
        )
        .field("running", json!(false))
        .field("stop_requested", json!(false)));
    }

    let locks_dir = ctx.data_dir.join("locks");
    private::create_dir(&locks_dir).map_err(|e| err("locks dir", e))?;
    let stop_path = stop_file_path(&ctx.data_dir, &repo);
    let mut file =
        private::open_file(&stop_path).map_err(|e| err(&stop_path.display().to_string(), e))?;
    file.set_len(0)
        .map_err(|e| err(&stop_path.display().to_string(), e))?;
    writeln!(file, "{}", std::process::id())
        .map_err(|e| err(&stop_path.display().to_string(), e))?;
    file.flush()
        .map_err(|e| err(&stop_path.display().to_string(), e))?;

    Ok(CmdOutput::ok(
        "ok",
        format!("requested graceful stop for {}", repo.display()),
    )
    .field("running", json!(true))
    .field("stop_requested", json!(true))
    .field("stop_file", json!(stop_path.to_string_lossy().into_owned())))
}
