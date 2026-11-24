//! Runs the repo's own lint, build and test commands.

use std::collections::BTreeSet;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aaf_domain::config::CommandsConfig;
use aaf_domain::gate::{ChangeKind, CommandResult, GateMode};
use aaf_domain::ports::Clock;

use super::GateError;
use super::changes::Change;
use super::proc::capture;
use crate::throttle::{HeavyJob, Throttle, heavy_tool};

static DIFF_ENV_SEQ: AtomicU64 = AtomicU64::new(0);
const DIFF_ENV_MAX_ATTEMPTS: usize = 16;

/// Temp files exposing the verified gate change set to `lint` / `test_scoped`
/// commands via `AAF_GATE_MODE`, `AAF_BASE_SHA`, `AAF_CHANGED_FILES_FILE`,
/// and `AAF_DELETED_FILES_FILE`. Removed automatically on drop.
pub struct DiffEnvFiles {
    mode: String,
    base_sha: String,
    changed_path: PathBuf,
    deleted_path: PathBuf,
}

impl DiffEnvFiles {
    pub fn write(
        mode: GateMode,
        base_sha: Option<&str>,
        changes: &[Change],
    ) -> Result<DiffEnvFiles, GateError> {
        let mut changed = BTreeSet::new();
        let mut deleted = BTreeSet::new();
        for ch in changes {
            match ch.kind {
                ChangeKind::Deleted => {
                    deleted.insert(ch.path.as_str());
                }
                ChangeKind::Renamed => {
                    changed.insert(ch.path.as_str());
                    if let Some(old) = &ch.old_path {
                        deleted.insert(old.as_str());
                    }
                }
                ChangeKind::Added | ChangeKind::Modified => {
                    changed.insert(ch.path.as_str());
                }
            }
        }
        let pid = std::process::id();
        let dir = std::env::temp_dir();
        let mut allocated = None;
        for _ in 0..DIFF_ENV_MAX_ATTEMPTS {
            let seq = DIFF_ENV_SEQ.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let candidate_changed = dir.join(format!("aaf-changed-{pid}-{nanos:x}-{seq}.txt"));
            let candidate_deleted = dir.join(format!("aaf-deleted-{pid}-{nanos:x}-{seq}.txt"));
            match write_lines_private(&candidate_changed, &changed) {
                Ok(()) => {}
                Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err.into()),
            }
            match write_lines_private(&candidate_deleted, &deleted) {
                Ok(()) => {
                    allocated = Some((candidate_changed, candidate_deleted));
                    break;
                }
                Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                    let _ = fs::remove_file(&candidate_changed);
                    continue;
                }
                Err(err) => {
                    let _ = fs::remove_file(&candidate_changed);
                    return Err(err.into());
                }
            }
        }
        let Some((changed_path, deleted_path)) = allocated else {
            return Err(std::io::Error::new(
                ErrorKind::AlreadyExists,
                "failed to allocate unique diff env files after retries",
            )
            .into());
        };
        let mode_str = match mode {
            GateMode::Scoped => "scoped",
            GateMode::Full => "full",
            GateMode::Release => "release",
        };
        Ok(DiffEnvFiles {
            mode: mode_str.to_string(),
            base_sha: base_sha.unwrap_or_default().to_string(),
            changed_path,
            deleted_path,
        })
    }

    pub fn env_pairs(&self) -> Vec<(&'static str, String)> {
        vec![
            ("AAF_GATE_MODE", self.mode.clone()),
            ("AAF_BASE_SHA", self.base_sha.clone()),
            (
                "AAF_CHANGED_FILES_FILE",
                self.changed_path.to_string_lossy().into_owned(),
            ),
            (
                "AAF_DELETED_FILES_FILE",
                self.deleted_path.to_string_lossy().into_owned(),
            ),
        ]
    }
}

impl Drop for DiffEnvFiles {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.changed_path);
        let _ = fs::remove_file(&self.deleted_path);
    }
}

fn write_lines_private(path: &Path, lines: &BTreeSet<&str>) -> std::io::Result<()> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    for line in lines {
        writeln!(file, "{line}")?;
    }
    Ok(())
}

/// Bytes of combined output kept per command.
pub const LOG_TAIL_BYTES: usize = 4096;

/// Start of every receipt reason that records what the machine throttle did
/// (a wait, a pause, a degraded run, a leftover daemon). Such a note depends
/// on the load and the other gates of the moment, never on the failure, so
/// the unblocker's failure digest leaves these reasons out.
pub const THROTTLE_NOTE: &str = "throttle: ";

/// Compiler wrapper and shared compilation-cache environment variables stripped
/// from gate commands when `GateRunOptions::strip_compiler_wrappers` is set
/// (e.g. during trusted `aaf merge` verification) so poisoned or ambient caches
/// cannot influence the build/test verdict.
pub const COMPILER_WRAPPER_ENV_VARS: &[&str] = &[
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "CARGO_BUILD_RUSTC_WRAPPER",
    "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
    "CARGO_TARGET_DIR",
    "SCCACHE_DIR",
    "SCCACHE_CONF",
    "SCCACHE_SERVER_PORT",
    "SCCACHE_SERVER_UDS",
    "SCCACHE_ENDPOINT",
    "SCCACHE_BUCKET",
    "SCCACHE_REDIS",
    "SCCACHE_MEMCACHED",
    "SCCACHE_GCS_BUCKET",
    "CCACHE_DIR",
    "CCACHE_CONFIGPATH",
    "CCACHE_PREFIX",
    "CCACHE_REMOTE_STORAGE",
];

/// Explicit environment overrides applied alongside [`COMPILER_WRAPPER_ENV_VARS`]
/// removal so config-file defaults (e.g. `[build] rustc-wrapper` in
/// `~/.cargo/config.toml` or `ccache.conf`) are neutralized too.
pub const COMPILER_WRAPPER_DISABLE_ENV: &[(&str, &str)] = &[
    ("RUSTC_WRAPPER", ""),
    ("RUSTC_WORKSPACE_WRAPPER", ""),
    ("CCACHE_DISABLE", "1"),
];

/// The test command the mode requires; empty means the gate cannot verify.
pub fn test_command(commands: &CommandsConfig, mode: GateMode) -> (&'static str, &str) {
    match mode {
        GateMode::Scoped => ("test_scoped", commands.test_scoped.trim()),
        GateMode::Full | GateMode::Release => ("test_full", commands.test_full.trim()),
    }
}

/// Configured commands for `mode`, in run order. Blank commands are left out.
pub fn plan(commands: &CommandsConfig, mode: GateMode) -> Vec<(&'static str, String)> {
    let mut steps = vec![("lint", commands.lint.trim())];
    if mode != GateMode::Scoped {
        steps.push(("lint_full", commands.lint_full.trim()));
        steps.push(("build", commands.build.trim()));
    }
    steps.push(test_command(commands, mode));
    steps
        .into_iter()
        .filter(|(_, command)| !command.is_empty())
        .map(|(name, command)| (name, command.to_string()))
        .collect()
}

/// Runs one command through `/bin/sh -c` in the repo root, in its own process
/// group, outside the throttle. On timeout the whole group is killed.
pub fn run_command(
    repo_root: &Path,
    name: &str,
    command: &str,
    timeout: Duration,
) -> Result<CommandResult, GateError> {
    run_command_with_env(repo_root, name, command, timeout, &[])
}

/// [`run_command`] with verified extra environment variables.
pub fn run_command_with_env(
    repo_root: &Path,
    name: &str,
    command: &str,
    timeout: Duration,
    extra_env: &[(&str, &str)],
) -> Result<CommandResult, GateError> {
    run_command_with_env_and_removals(repo_root, name, command, timeout, extra_env, &[])
}

/// [`run_command_with_env`] that also strips every variable listed in
/// `env_remove` (e.g. compiler wrapper and shared cache overrides).
pub fn run_command_with_env_and_removals(
    repo_root: &Path,
    name: &str,
    command: &str,
    timeout: Duration,
    extra_env: &[(&str, &str)],
    env_remove: &[&str],
) -> Result<CommandResult, GateError> {
    // By path: a bare `sh` is looked up in `PATH` at every spawn, where the
    // task can have put one of its own.
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(command).current_dir(repo_root);
    // When the gate itself runs inside a git hook these would point the
    // command's own git calls at the wrong repository. Strip ambient diff vars too.
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "AAF_GATE_MODE",
        "AAF_BASE_SHA",
        "AAF_CHANGED_FILES_FILE",
        "AAF_DELETED_FILES_FILE",
        "AAF_TEST_FILES_FILE",
    ] {
        cmd.env_remove(var);
    }
    for var in env_remove {
        cmd.env_remove(var);
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let done = capture(cmd, timeout, Some(LOG_TAIL_BYTES), true)?;
    Ok(CommandResult {
        name: name.to_string(),
        command: command.to_string(),
        exit_code: if done.timed_out { None } else { done.exit_code },
        timed_out: done.timed_out,
        duration_ms: u64::try_from(done.duration.as_millis()).unwrap_or(u64::MAX),
        log_tail: String::from_utf8_lossy(&done.output).into_owned(),
    })
}

/// Runs the plan in order and stops at the first command that does not pass:
/// later steps would only add time to a result that is already `Blocked`.
pub fn run_all(
    repo_root: &Path,
    commands: &CommandsConfig,
    mode: GateMode,
    timeout: Duration,
) -> Result<Vec<CommandResult>, GateError> {
    let mut results = Vec::new();
    for (name, command) in plan(commands, mode) {
        let result = run_command(repo_root, name, &command, timeout)?;
        let passed = result.passed();
        results.push(result);
        if !passed {
            break;
        }
    }
    Ok(results)
}

/// Commands that go through the machine throttle (docs/PLAN.md C.11).
///
/// Build and test commands always do: whatever they start, ten of them at
/// once are what freezes the machine. Any other command (lint) stays light,
/// so cheap failures show before any queueing, unless its text starts a
/// build tool: `./gradlew lint` is a full Gradle run with a daemon, and the
/// name of the config key does not make it cheaper. The decision comes from
/// the one classifier the throttle, `aaf slot` and the doctor share.
pub fn is_heavy(name: &str, command: &str) -> bool {
    matches!(name, "lint_full" | "build" | "test_scoped" | "test_full")
        || heavy_tool(command).is_some()
}

/// What the throttled commands need besides the plan itself.
pub struct HeavyContext<'a> {
    pub data_dir: &'a Path,
    /// Key of this repository in the learned memory statistics.
    pub stats_repo_id: &'a str,
    pub clock: &'a dyn Clock,
    pub throttle: &'a Throttle<'a>,
}

pub struct ThrottledRun {
    pub results: Vec<CommandResult>,
    /// What the throttle did (waits, degraded runs, pauses), for the
    /// receipt's `reasons`. Never changes the status.
    pub notes: Vec<String>,
}

#[derive(Clone, Copy)]
struct CommandEnv<'a> {
    extra: &'a [(&'a str, &'a str)],
    remove: &'a [&'a str],
}

fn run_heavy(
    repo_root: &Path,
    name: &str,
    command: &str,
    timeout: Duration,
    heavy: &HeavyContext<'_>,
    env: CommandEnv<'_>,
    notes: &mut Vec<String>,
) -> Result<CommandResult, GateError> {
    let job = HeavyJob {
        repo_id: heavy.stats_repo_id,
        label: name,
        command,
        cwd: repo_root,
        timeout,
        log_tail_bytes: LOG_TAIL_BYTES,
    };
    let filtered_remove: Vec<&str> = env
        .remove
        .iter()
        .copied()
        .filter(|var| !env.extra.iter().any(|(k, _)| k == var))
        .collect();
    let run = heavy.throttle.run_heavy_with_env_and_removals(
        heavy.data_dir,
        heavy.clock,
        &job,
        env.extra,
        &filtered_remove,
    )?;
    if run.waited_secs > 0 {
        notes.push(format!(
            "{THROTTLE_NOTE}{name} waited {}s for a heavy slot or a calmer machine",
            run.waited_secs
        ));
    }
    if run.degraded {
        let machine = &heavy.throttle.machine;
        // With the real-CPU check on, load average alone does not hold a
        // command (`throttle::admit`); an unreadable CPU probe falls back to it.
        let cpu = if machine.max_cpu_percent > 0 {
            format!(
                " and CPU busy ≥ {}% (when measurable)",
                machine.max_cpu_percent
            )
        } else {
            String::new()
        };
        notes.push(format!(
            "{THROTTLE_NOTE}{name} ran degraded: CPU load stayed above {}%{cpu} for {}s",
            machine.max_load_percent, machine.heavy_wait_secs
        ));
    }
    if !run.paused.is_zero() {
        notes.push(format!(
            "{THROTTLE_NOTE}{name} was paused {}s under critical memory pressure",
            run.paused.as_secs()
        ));
    }
    for daemon in &run.leftover_daemons {
        notes.push(format!(
            "{THROTTLE_NOTE}{} daemon still running pid={}; builds are not using --no-daemon",
            daemon.kind, daemon.pid
        ));
    }
    Ok(CommandResult {
        name: name.to_string(),
        command: command.to_string(),
        exit_code: if run.timed_out { None } else { run.exit_code },
        timed_out: run.timed_out,
        duration_ms: u64::try_from(run.duration.as_millis()).unwrap_or(u64::MAX),
        log_tail: String::from_utf8_lossy(&run.output).into_owned(),
    })
}

/// [`run_all`] with the heavy commands sent through the machine throttle.
pub fn run_all_throttled(
    repo_root: &Path,
    commands: &CommandsConfig,
    mode: GateMode,
    timeout: Duration,
    heavy: &HeavyContext<'_>,
) -> Result<ThrottledRun, GateError> {
    run_all_throttled_with_env(repo_root, commands, mode, timeout, heavy, &[])
}

/// [`run_all_throttled`] with verified extra environment variables.
pub fn run_all_throttled_with_env(
    repo_root: &Path,
    commands: &CommandsConfig,
    mode: GateMode,
    timeout: Duration,
    heavy: &HeavyContext<'_>,
    extra_env: &[(&str, &str)],
) -> Result<ThrottledRun, GateError> {
    run_all_throttled_with_options(repo_root, commands, mode, timeout, heavy, extra_env, &[])
}

/// [`run_all_throttled_with_env`] that also strips `env_remove` from both
/// light and heavy commands.
pub fn run_all_throttled_with_options(
    repo_root: &Path,
    commands: &CommandsConfig,
    mode: GateMode,
    timeout: Duration,
    heavy: &HeavyContext<'_>,
    extra_env: &[(&str, &str)],
    env_remove: &[&str],
) -> Result<ThrottledRun, GateError> {
    let mut run = ThrottledRun {
        results: Vec::new(),
        notes: Vec::new(),
    };
    let env = CommandEnv {
        extra: extra_env,
        remove: env_remove,
    };
    for (name, command) in plan(commands, mode) {
        let result = if is_heavy(name, &command) {
            run_heavy(
                repo_root,
                name,
                &command,
                timeout,
                heavy,
                env,
                &mut run.notes,
            )?
        } else {
            run_command_with_env_and_removals(
                repo_root, name, &command, timeout, extra_env, env_remove,
            )?
        };
        let passed = result.passed();
        run.results.push(result);
        if !passed {
            break;
        }
    }
    Ok(run)
}

/// Runs one heavy command through the machine throttle with verified env vars.
pub fn run_one_heavy(
    repo_root: &Path,
    name: &str,
    command: &str,
    timeout: Duration,
    heavy: &HeavyContext<'_>,
    extra_env: &[(&str, &str)],
) -> Result<CommandResult, GateError> {
    let mut notes = Vec::new();
    let env = CommandEnv {
        extra: extra_env,
        remove: &[],
    };
    run_heavy(repo_root, name, command, timeout, heavy, env, &mut notes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn diff_env_files_unique_nonce_and_cleanup() {
        let changes = vec![
            Change {
                path: "src/a.rs".to_string(),
                kind: ChangeKind::Modified,
                old_path: None,
            },
            Change {
                path: "src/old.rs".to_string(),
                kind: ChangeKind::Deleted,
                old_path: None,
            },
        ];
        let first = DiffEnvFiles::write(GateMode::Scoped, Some("abc"), &changes).unwrap();
        let second = DiffEnvFiles::write(GateMode::Scoped, Some("abc"), &changes).unwrap();
        assert_ne!(first.changed_path, second.changed_path);
        assert_ne!(first.deleted_path, second.deleted_path);
        let p1 = first.changed_path.clone();
        let p2 = first.deleted_path.clone();
        assert!(p1.exists() && p2.exists());
        drop(first);
        assert!(!p1.exists() && !p2.exists());
    }
}
