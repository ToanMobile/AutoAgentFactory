//! Executors: implementations of [`AgentRunner`] that run a real agent CLI
//! under a pseudo-terminal, optionally inside a Seatbelt sandbox, and clean up
//! every process the run leaves behind.

pub mod agy;
pub mod claude;
pub mod env;
pub mod generic;
pub mod linux_sandbox;
pub mod pty;
pub mod reaper;
pub mod registry;
pub mod sandbox;
pub mod sandbox_check;
pub mod secret_mask;
pub mod stream_log;
#[cfg(test)]
mod tests;

#[path = "confined_paths.rs"]
mod confined_paths;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use aaf_domain::config::RunConfig;
use aaf_domain::keychain::KeychainPolicy;
use aaf_domain::ports::{AgentRequest, AgentRunner, PortError, PortResult};

use self::agy::AgyRunner;
use self::claude::ClaudeCodeRunner;
use self::env::inject_non_interactive_env;
pub use self::env::{agent_env, env_pass_secrets, validate_writable_caches};
use self::generic::GenericRunner;
use self::pty::{CommandSpec, DEFAULT_TAIL_BYTES, LineCallback, PtyOutput};
#[cfg(test)]
pub(crate) use self::sandbox::secondary_volume_temp_dir_in;

/// Tool caches under the home directory that builds write to (never blanket
/// `~/.cache` or `~/Library/Caches`; `.cache/sccache` is intentionally omitted).
pub const BUILD_CACHE_DIRS: [&str; 43] = [
    ".cargo",
    ".rustup",
    ".gradle",
    ".m2",
    ".android",
    ".npm",
    ".yarn",
    ".pub-cache",
    ".swiftpm",
    ".cocoapods",
    "go/pkg/mod",
    ".konan",
    ".cache/pip",
    ".cache/uv",
    ".cache/go-build",
    ".cache/pnpm",
    ".local/share/pnpm",
    ".bun/install/cache",
    ".cache/deno",
    "Library/Caches/deno",
    ".cache/ms-playwright",
    "Library/Caches/ms-playwright",
    ".cache/ruff",
    "Library/Caches/ruff",
    ".cache/mypy",
    ".cache/pytest",
    ".cache/flutter",
    "Library/Caches/pip",
    "Library/Caches/Yarn",
    "Library/Caches/pnpm",
    "Library/Caches/go-build",
    "Library/Caches/CocoaPods",
    "Library/Caches/org.swift.swiftpm",
    "Library/Developer/Xcode/DerivedData",
    "Library/Application Support/kotlin",
    "Library/Unity",
    "Library/Caches/com.unity3d.UnityEditor",
    "Library/Caches/Unity",
    "Library/Logs/Unity",
    "Library/Application Support/Unity",
    "Library/Developer/CoreSimulator",
    "Library/Caches/com.apple.dt.Xcode",
    "Library/org.swift.swiftpm",
];

/// Process environment inputs for executors (injected so tests stay isolated).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExecEnv {
    pub home: Option<String>,
    /// Temporary directories an agent may write to.
    pub temp_dirs: Vec<String>,
    /// Absolute path of the `claude` binary, when one was found.
    pub claude_bin: Option<String>,
    /// Absolute path of the `agy` (or `antigravity`) binary, when found.
    pub agy_bin: Option<String>,
    /// The aaf data directory (signing key, receipts, approvals); always denied.
    pub data_dir: Option<String>,
    /// Directory holding task worktrees (`None` = `$HOME/{DEFAULT_WORKTREES_REL}`).
    pub worktrees_root: Option<String>,
    /// External writable SDK roots (`ANDROID_HOME` / `ANDROID_SDK_ROOT`).
    pub sdk_dirs: Vec<String>,
    /// Custom toolchain home overrides (`CARGO_HOME`, `RUSTUP_HOME`, `GRADLE_USER_HOME`).
    pub cargo_home: Option<String>,
    pub rustup_home: Option<String>,
    pub gradle_user_home: Option<String>,
    /// Parent process environment; filtered through [`agent_env`].
    pub parent_env: Vec<(String, String)>,
    /// `run.env_pass`: further names to hand on.
    pub env_pass: Vec<String>,
    /// Tier-1 machine `run.env_pass` ([`env::validate_machine_env_pass`]).
    pub machine_env_pass: Vec<String>,
    /// Tier-1 `machine.writable_caches` ([`env::validate_writable_caches`]).
    pub machine_writable_caches: Vec<String>,
    /// Tier-1 `safety.deny_macos_keychain` policy for macOS Seatbelt runs.
    pub keychain_policy: KeychainPolicy,
}

const DATA_DIR_ENV: &str = "AAF_DATA_DIR";
const SDK_DIR_ENV: [&str; 2] = ["ANDROID_HOME", "ANDROID_SDK_ROOT"];

/// Default task-worktree directory, relative to the home directory
/// (docs/PLAN.md C.1).
pub const DEFAULT_WORKTREES_REL: &str = ".cache/autofactory/worktrees";
/// Host-owned warm-slot cache directory; always read- and write-denied inside sandboxes.
pub const DEFAULT_WARM_SLOTS_REL: &str = ".cache/autofactory/warm-slots";

impl ExecEnv {
    /// Reads the real environment: `HOME`, `TMPDIR` and `PATH`.
    pub fn from_process() -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let mut temp_dirs = vec!["/tmp".to_string(), "/var/tmp".to_string()];
        if let Some(tmp) = var("TMPDIR") {
            // macOS hands out /var/folders/<x>/<y>/T; compilers keep their
            // per-user caches in the sibling C directory.
            let path = Path::new(&tmp);
            if path.file_name() == Some(OsStr::new("T"))
                && let Some(sibling) = path.parent().map(|p| p.join("C"))
            {
                temp_dirs.push(sibling.to_string_lossy().into_owned());
            }
            temp_dirs.push(tmp);
        }
        let claude_bin = std::env::var_os("PATH")
            .and_then(|path| find_on_path(&path, "claude"))
            .map(|p| p.to_string_lossy().into_owned());
        let agy_bin = std::env::var_os("PATH")
            .and_then(|path| AgyRunner::resolve_binary(&path))
            .map(|p| p.to_string_lossy().into_owned());
        ExecEnv {
            home: var("HOME"),
            temp_dirs,
            claude_bin,
            agy_bin,
            // Fails only when neither AAF_DATA_DIR nor HOME is set, and then
            // no data directory can exist for this user either.
            data_dir: aaf_fp::data_dir()
                .ok()
                .map(|dir| dir.to_string_lossy().into_owned()),
            worktrees_root: var("AAF_WORKTREE_DIR"),
            sdk_dirs: SDK_DIR_ENV.iter().filter_map(|name| var(name)).collect(),
            cargo_home: var("CARGO_HOME"),
            rustup_home: var("RUSTUP_HOME"),
            gradle_user_home: var("GRADLE_USER_HOME"),
            // A name or value that is not UTF-8 cannot be on the allow-list.
            parent_env: std::env::vars_os()
                .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
                .collect(),
            env_pass: Vec::new(),
            machine_env_pass: Vec::new(),
            machine_writable_caches: Vec::new(),
            keychain_policy: KeychainPolicy::Auto,
        }
    }
}

/// Returns whether macOS Seatbelt should deny Keychain read/IPC access for this run.
pub fn should_deny_macos_keychain(
    policy: KeychainPolicy,
    executor: &str,
    env_vars: &[(String, String)],
) -> bool {
    match policy {
        KeychainPolicy::Always => true,
        KeychainPolicy::Never => false,
        KeychainPolicy::Auto => {
            if !executor.trim().eq_ignore_ascii_case("claude") {
                return true;
            }
            env_vars
                .iter()
                .any(|(k, v)| k == "ANTHROPIC_API_KEY" && !v.trim().is_empty())
        }
    }
}

/// First regular file called `name` in the directories of `path_var`. Looking
/// the binary up ourselves skips any shell function or alias of that name.
pub fn find_on_path(path_var: &OsStr, name: &str) -> Option<PathBuf> {
    std::env::split_paths(path_var)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Builds the runner `cfg.executor` names, using the real environment.
pub fn build_runner(cfg: &RunConfig) -> PortResult<Box<dyn AgentRunner>> {
    build_runner_with(cfg, ExecEnv::from_process())
}

/// [`build_runner`] with an explicit environment.
pub fn build_runner_with(cfg: &RunConfig, mut env: ExecEnv) -> PortResult<Box<dyn AgentRunner>> {
    env.env_pass.extend(cfg.env_pass.iter().cloned());
    env::validate_env_pass(&env.env_pass)?;
    env::validate_machine_env_pass(&env.machine_env_pass)?;
    env::validate_writable_caches(&env.machine_writable_caches, &env)?;
    let path_var = env
        .parent_env
        .iter()
        .rev()
        .find(|(k, v)| k == "PATH" && !v.is_empty())
        .map(|(_, v)| v.clone());
    match cfg.executor.as_str() {
        "claude" => {
            let bin = env.claude_bin.clone().ok_or_else(|| {
                PortError::NotFound("the `claude` binary is not on PATH".to_string())
            })?;
            Ok(Box::new(
                ClaudeCodeRunner::new(bin, env).with_model_and_effort(&cfg.model, &cfg.effort),
            ))
        }
        "antigravity" if cfg.executor_cmd.is_empty() => {
            let bin = env
                .agy_bin
                .clone()
                .or_else(|| {
                    path_var
                        .as_deref()
                        .and_then(|p| AgyRunner::resolve_binary(OsStr::new(p)))
                        .map(|p| p.to_string_lossy().into_owned())
                })
                .ok_or_else(|| {
                    PortError::NotFound("neither `agy` nor `antigravity` binary is on PATH".into())
                })?;
            Ok(Box::new(
                AgyRunner::new(bin, env).with_model_and_effort(&cfg.model, &cfg.effort),
            ))
        }
        "custom" => Ok(Box::new(GenericRunner::new(cfg.executor_cmd.clone(), env)?)),
        exec if !cfg.executor_cmd.is_empty() => {
            if !registry::builtin_executors().iter().any(|d| d.id == exec) {
                return Err(PortError::Invalid(format!("unknown executor {exec:?}")));
            }
            Ok(Box::new(GenericRunner::new(cfg.executor_cmd.clone(), env)?))
        }
        exec => {
            let tpl = registry::resolve_generic_template(
                exec,
                &cfg.model,
                &cfg.effort,
                path_var.as_deref().map(OsStr::new),
            )?;
            Ok(Box::new(GenericRunner::new(tpl, env)?))
        }
    }
}

/// What of the agent's own account directory a confined run may write.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AccountPaths {
    pub writable: Vec<String>,
    /// Entries inside `writable` that stay closed.
    pub write_denied: Vec<String>,
    /// Narrow SBPL regexes allowed for writing (emitted before denies on macOS).
    pub writable_regex: Vec<String>,
}

/// Best-effort opens the run's sanitized stream log when `env.data_dir` is set
/// and the request passes basic pre-spawn checks.
pub(crate) fn open_stream_log(env: &ExecEnv, request: &AgentRequest) -> stream_log::StreamLog {
    if request.timeout_secs == 0 || !Path::new(&request.cwd).is_dir() {
        return stream_log::StreamLog::noop();
    }
    let secrets = env_pass_secrets(env);
    stream_log::StreamLog::open(
        env.data_dir.as_deref().map(Path::new),
        &request.run_id,
        secrets,
    )
    .unwrap_or_else(|_| stream_log::StreamLog::noop())
}

/// Shared run path of every executor: tags the process with the run id,
/// applies the sandbox, runs under the pty with the request's timeout and
/// reaps what is left. `account` says which config paths the agent's own
/// account needs to write.
pub(crate) fn run_confined(
    spec: CommandSpec,
    request: &AgentRequest,
    env: &ExecEnv,
    account: AccountPaths,
    executor: &str,
    on_line: Option<LineCallback>,
) -> PortResult<PtyOutput> {
    run_confined_with_tail(
        spec,
        request,
        env,
        account,
        executor,
        on_line,
        DEFAULT_TAIL_BYTES,
    )
}

/// [`run_confined`] with a caller-chosen bounded PTY output buffer cap.
pub(crate) fn run_confined_with_tail(
    mut spec: CommandSpec,
    request: &AgentRequest,
    env: &ExecEnv,
    account: AccountPaths,
    executor: &str,
    on_line: Option<LineCallback>,
    tail_bytes: usize,
) -> PortResult<PtyOutput> {
    reaper::validate_run_id(&request.run_id)?;
    // Where the environment is built, so a runner made without
    // `build_runner_with` is held to the same names.
    env::validate_env_pass(&env.env_pass)?;
    env::validate_machine_env_pass(&env.machine_env_pass)?;
    let extra_caches = env::validate_writable_caches(&env.machine_writable_caches, env)?;
    if request.timeout_secs == 0 {
        return Err(PortError::Invalid("timeout_secs must be > 0".to_string()));
    }
    if !Path::new(&request.cwd).is_dir() {
        return Err(PortError::NotFound(format!(
            "working directory {}",
            request.cwd
        )));
    }
    spec.cwd = Some(request.cwd.clone());
    spec.env
        .push((reaper::RUN_ID_ENV.to_string(), request.run_id.clone()));
    if let Some(data_dir) = &env.data_dir {
        // Not the caller's `AAF_DATA_DIR` handed on, but the directory this
        // process really uses: `aaf-hook` guards it and `aaf slot` queues in
        // it, and both run inside the agent.
        spec.env.push((DATA_DIR_ENV.to_string(), data_dir.clone()));
    }
    inject_non_interactive_env(env, &mut spec.env);
    spec.env = agent_env(env, &spec.env);
    spec.env_clear = true;

    let mut marks = None;
    if request.sandbox.enabled {
        // The files the reaper asks the kernel about, in aaf's data
        // directory (closed to the agent). Without one, the reaper still
        // finds what carries the run id.
        if let Some(dir) = &env.data_dir {
            let run_marks =
                reaper::RunMarks::at(&std::path::Path::new(dir).join("marks"), &request.run_id)?;
            run_marks.create()?;
            marks = Some(run_marks);
        }
        let extra = confined_paths::build_sandbox_paths(
            request,
            env,
            account,
            executor,
            &spec.env,
            &extra_caches,
            marks.as_ref().map(|marks| marks.mark.clone()),
        )?;
        if cfg!(target_os = "linux") {
            match linux_sandbox::detect_host_linux_isolation() {
                linux_sandbox::LinuxIsolation::Bubblewrap { bwrap_bin } => {
                    spec = linux_sandbox::wrap(&bwrap_bin, &extra, &spec)?;
                }
                linux_sandbox::LinuxIsolation::HardenedContainer { .. } => {}
                linux_sandbox::LinuxIsolation::None => {
                    return Err(PortError::Unsupported(
                        "no Linux sandbox found (install bubblewrap `bwrap`, run inside a hardened container, or pass --unsandboxed)".to_string(),
                    ));
                }
            }
        } else {
            let profile = sandbox::seatbelt_profile(&request.sandbox, &extra)?;
            spec = sandbox::wrap(&spec, &profile)?;
        }
    }

    let mut output = pty::run_pty(
        &spec,
        Duration::from_secs(request.timeout_secs),
        tail_bytes,
        on_line,
    )?;
    // The run itself is over and its result stands; a failed scan is recorded
    // in the log instead of turning a finished run into an error.
    if let Err(err) = reaper::reap_in(&request.run_id, marks.as_ref()) {
        output.tail.push_str(&format!("\n[aaf] reaper: {err}\n"));
    }
    if let Some(marks) = &marks
        && let Err(err) = marks.remove()
    {
        output.tail.push_str(&format!("\n[aaf] reaper: {err}\n"));
    }
    for name in env::withheld_proxies(env) {
        output.tail.push_str(&format!(
            "\n[aaf] {name} was not handed to the agent: its URL carries a login. \
             Name it in run.env_pass to hand it on.\n"
        ));
    }
    Ok(output)
}

/// Runs a short helper (`ps`, `kill`) without a shell and returns `(exit_code, stdout)`.
pub(crate) fn capture(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> PortResult<(Option<i32>, String)> {
    let mut command = Command::new(program);
    command.args(args);
    let finished = crate::process::run_with_timeout(command, timeout, true)
        .map_err(|err| PortError::Io(format!("{program}: {err}")))?;
    match finished.status {
        Some(status) => Ok((
            status.code(),
            String::from_utf8_lossy(&finished.stdout).into_owned(),
        )),
        // Killed at the deadline, or its output did not end with it.
        None => Err(PortError::Io(format!("{program}: timed out"))),
    }
}
