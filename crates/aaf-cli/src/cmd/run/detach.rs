//! Detached background runner (`aaf run --detach`) and `run.meta.json` helpers.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use aaf_domain::config::LandMode;
use aaf_domain::ports::Clock;
use aaf_engine::db::private;
use aaf_engine::factory::adapters::SystemClock;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::super::board::store::err;
use super::{ParallelArg, RunArgs};
use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput, CmdResult};

pub(super) const DETACHED_TASK_ENV: &str = "AAF_DETACHED_TASK_ID";
const READY_WAIT: Duration = Duration::from_secs(5);
const READY_POLL: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunMeta {
    pub pid: u32,
    pub repo_root: String,
    pub log_path: Option<String>,
    pub started_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
}

pub(crate) fn run_lock_path(data_dir: &Path, repo: &Path) -> PathBuf {
    data_dir
        .join("locks")
        .join(format!("{}.run.lock", aaf_fp::repo_id(repo)))
}

pub(crate) fn run_meta_path(data_dir: &Path, repo: &Path) -> PathBuf {
    data_dir
        .join("locks")
        .join(format!("{}.run.meta.json", aaf_fp::repo_id(repo)))
}

pub(crate) fn detached_log_path(data_dir: &Path, repo: &Path) -> PathBuf {
    data_dir
        .join("runs")
        .join(format!("{}.detached.log", aaf_fp::repo_id(repo)))
}

/// Moves a non-empty log of the previous detached run to `<log>.1` (one
/// generation), so restarting after a crash keeps the crash's evidence.
pub(crate) fn rotate_detached_log(log_path: &Path) -> std::io::Result<()> {
    match std::fs::metadata(log_path) {
        Ok(meta) if meta.len() > 0 => {
            let mut rotated = log_path.as_os_str().to_owned();
            rotated.push(".1");
            std::fs::rename(log_path, PathBuf::from(rotated))
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Writes `<data_dir>/locks/<repo_id>.run.meta.json` with `0600` permissions.
pub(crate) fn write_run_meta(
    data_dir: &Path,
    repo: &Path,
    log_path: Option<&Path>,
    task_id: Option<&str>,
) -> Result<PathBuf, CmdError> {
    let locks_dir = data_dir.join("locks");
    private::create_dir(&locks_dir).map_err(|e| err("locks dir", e))?;
    let path = run_meta_path(data_dir, repo);
    let meta = RunMeta {
        pid: std::process::id(),
        repo_root: repo.to_string_lossy().into_owned(),
        log_path: log_path.map(|p| p.to_string_lossy().into_owned()),
        started_at: SystemClock.now(),
        task_id: task_id.map(str::to_string),
    };
    let bytes = serde_json::to_vec_pretty(&meta).map_err(|e| err("run meta", e))?;
    let mut file = private::open_file(&path).map_err(|e| err(&path.display().to_string(), e))?;
    file.set_len(0)
        .map_err(|e| err(&path.display().to_string(), e))?;
    file.write_all(&bytes)
        .map_err(|e| err(&path.display().to_string(), e))?;
    file.flush()
        .map_err(|e| err(&path.display().to_string(), e))?;
    Ok(path)
}

fn read_meta(path: &Path) -> Option<RunMeta> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn read_log_tail(path: &Path) -> String {
    let Ok(text) = fs::read_to_string(path) else {
        return String::new();
    };
    let trimmed = text.trim();
    if trimmed.len() <= 800 {
        trimmed.to_string()
    } else {
        let mut start = trimmed.len() - 800;
        while !trimmed.is_char_boundary(start) {
            start += 1;
        }
        trimmed[start..].to_string()
    }
}

/// Spawns `current_exe() --json run --detached-child ...` in its own process
/// group (`process_group(0)` on Unix), writing logs to
/// `<data_dir>/runs/<repo_id>.detached.log` (`0600`), and waits briefly for
/// the child to acquire `run_lock` and write `run.meta.json`.
pub(super) fn spawn_detached(
    args: &RunArgs,
    ctx: &Ctx,
    repo: &Path,
    created_task_id: Option<&str>,
) -> CmdResult {
    let runs_dir = ctx.data_dir.join("runs");
    private::create_dir(&runs_dir).map_err(|e| err("runs dir", e))?;
    let log_path = detached_log_path(&ctx.data_dir, repo);
    rotate_detached_log(&log_path).map_err(|e| err(&log_path.display().to_string(), e))?;
    let log_file =
        private::open_file(&log_path).map_err(|e| err(&log_path.display().to_string(), e))?;
    log_file
        .set_len(0)
        .map_err(|e| err(&log_path.display().to_string(), e))?;
    let log_err = log_file
        .try_clone()
        .map_err(|e| err(&log_path.display().to_string(), e))?;

    let exe = std::env::current_exe().map_err(|e| err("current_exe", e))?;
    let mut cmd = Command::new(exe);
    cmd.current_dir(&ctx.cwd)
        .args(["--json", "run", "--detached-child"]);
    if args.auto {
        cmd.arg("--auto");
    }
    if args.once {
        cmd.arg("--once");
    }
    if args.auto_merge {
        cmd.arg("--auto-merge");
    }
    if let Some(land) = args.land {
        cmd.args(["--land", LandMode::from(land).as_str()]);
    }
    if let Some(parallel) = args.max_parallel {
        let value = match parallel {
            ParallelArg::Auto => "auto".to_string(),
            ParallelArg::Fixed(n) => n.to_string(),
        };
        cmd.args(["--max-parallel", &value]);
    }
    if args.auto_split {
        cmd.arg("--auto-split");
    }
    if args.unsandboxed {
        cmd.arg("--unsandboxed");
    }
    if let Some(task_id) = created_task_id {
        cmd.env(DETACHED_TASK_ENV, task_id);
    } else {
        cmd.env_remove(DETACHED_TASK_ENV);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_err));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = cmd.spawn().map_err(|e| err("spawn detached runner", e))?;
    let pid = child.id();
    let meta_path = run_meta_path(&ctx.data_dir, repo);
    let deadline = Instant::now() + READY_WAIT;
    while Instant::now() < deadline {
        if read_meta(&meta_path).is_some_and(|m| m.pid == pid) {
            break;
        }
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(status)) => {
                let tail = read_log_tail(&log_path);
                let msg = if tail.is_empty() {
                    format!(
                        "detached runner (pid {pid}) exited early with {status}; log: {}",
                        log_path.display()
                    )
                } else {
                    format!("detached runner (pid {pid}) exited early with {status}: {tail}")
                };
                return Err(CmdError::new(msg));
            }
            Ok(None) => std::thread::sleep(READY_POLL),
            Err(_) => break,
        }
    }

    let mut text = format!(
        "detached runner started (pid={pid}, log={})",
        log_path.display()
    );
    if let Some(task_id) = created_task_id {
        text.push_str(&format!(", task_id={task_id}"));
    }
    let mut out = CmdOutput::ok("detached", text)
        .field("detached", json!(true))
        .field("pid", json!(pid))
        .field("log_path", json!(log_path.to_string_lossy().into_owned()))
        .field("meta_path", json!(meta_path.to_string_lossy().into_owned()));
    if let Some(task_id) = created_task_id {
        out = out.field("task_id", json!(task_id));
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn restarting_keeps_the_previous_log_as_one_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("r.detached.log");
        let old = tmp.path().join("r.detached.log.1");
        rotate_detached_log(&log).unwrap();
        assert!(!old.exists(), "nothing to keep yet");

        std::fs::write(&log, "crash 1\n").unwrap();
        rotate_detached_log(&log).unwrap();
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "crash 1\n");
        assert!(!log.exists());

        std::fs::write(&log, "").unwrap();
        rotate_detached_log(&log).unwrap();
        assert_eq!(
            std::fs::read_to_string(&old).unwrap(),
            "crash 1\n",
            "empty log is not kept"
        );

        std::fs::write(&log, "crash 2\n").unwrap();
        rotate_detached_log(&log).unwrap();
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "crash 2\n");
    }
}
