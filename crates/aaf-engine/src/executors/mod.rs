//! Executors: implementations of [`AgentRunner`] that run a real agent CLI
//! under a pseudo-terminal, optionally inside a Seatbelt sandbox, and clean up
//! every process the run leaves behind.

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

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use aaf_domain::config::RunConfig;
use aaf_domain::ports::{AgentRequest, AgentRunner, PortError, PortResult};

use self::claude::ClaudeCodeRunner;
pub use self::env::{agent_env, env_pass_secrets};
use self::env::{push_denied_entries, toolchain_home};
use self::generic::GenericRunner;
use self::pty::{CommandSpec, DEFAULT_TAIL_BYTES, LineCallback, PtyOutput};
use self::sandbox::SandboxPaths;

/// Tool caches under the home directory that builds write to. Each entry is
/// one tool's own directory: a blanket `~/.cache` or `~/Library/Caches` would
/// also open other tools' state, including `~/.cache/autofactory/worktrees`
/// and `warm-slots`. `.cache/sccache` is intentionally omitted: unsandboxed
/// `aaf gate`/`aaf merge` would otherwise inherit poisoned compiler artifacts.
pub const BUILD_CACHE_DIRS: [&str; 31] = [
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
    ".cache/pip",
    ".cache/uv",
    ".cache/go-build",
    "Library/Caches/pip",
    "Library/Caches/Yarn",
    "Library/Caches/pnpm",
    "Library/Caches/go-build",
    "Library/Caches/CocoaPods",
    "Library/Caches/org.swift.swiftpm",
    "Library/Developer/Xcode/DerivedData",
    ".konan",
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

/// Sensitive paths inside writable caches (scripts, PATH bins, configs, signing
/// keys, credentials); a trailing `*` closes every matching prefix. See SECURITY.md.
const CACHE_WRITE_DENIED: [&str; 13] = [
    ".gradle/init.d",
    ".gradle/gradle.properties",
    ".gradle/init.gradle*",
    ".cargo/bin",
    ".cargo/env",
    ".cargo/config*",
    ".cargo/credentials*",
    ".android/adbkey*",
    ".android/debug.keystore",
    ".m2/settings.xml",
    ".m2/settings-security.xml",
    ".pub-cache/bin",
    ".pub-cache/credentials.json",
];

/// The part of an Android SDK the owner's shell runs (`adb`, `fastboot`);
/// the rest of the SDK is where builds install platforms and build tools.
const SDK_WRITE_DENIED: [&str; 1] = ["platform-tools"];
const CARGO_WRITE_DENIED: [&str; 4] = ["bin", "env", "config*", "credentials*"];
const GRADLE_WRITE_DENIED: [&str; 3] = ["init.d", "gradle.properties", "init.gradle*"];

/// Process environment inputs for executors (injected so tests stay isolated).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExecEnv {
    pub home: Option<String>,
    /// Temporary directories an agent may write to.
    pub temp_dirs: Vec<String>,
    /// Absolute path of the `claude` binary, when one was found.
    pub claude_bin: Option<String>,
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
        ExecEnv {
            home: var("HOME"),
            temp_dirs,
            claude_bin,
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
    // Here as well as in `run_confined`: a bad name stops `aaf run` before
    // any task is taken, not at the first agent start.
    env::validate_env_pass(&env.env_pass)?;
    env::validate_machine_env_pass(&env.machine_env_pass)?;
    match cfg.executor.as_str() {
        "claude" => {
            let bin = env.claude_bin.clone().ok_or_else(|| {
                PortError::NotFound("the `claude` binary is not on PATH".to_string())
            })?;
            Ok(Box::new(
                ClaudeCodeRunner::new(bin, env).with_model_and_effort(&cfg.model, &cfg.effort),
            ))
        }
        "codex" | "gemini" | "antigravity" | "opencode" | "custom" => {
            Ok(Box::new(GenericRunner::new(cfg.executor_cmd.clone(), env)?))
        }
        other => Err(PortError::Invalid(format!("unknown executor {other:?}"))),
    }
}

/// What of the agent's own account directory a confined run may write.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct AccountPaths {
    pub writable: Vec<String>,
    /// Entries inside `writable` that stay closed.
    pub write_denied: Vec<String>,
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

/// The part of the data dir a confined agent queues its heavy builds in
/// (`aaf slot`): the throttle's directory to read and lock, its guest
/// directory to write. Both are made ready first, because the agent can
/// create nothing there.
fn open_queue(env: &ExecEnv) -> PortResult<(Vec<String>, Vec<String>)> {
    use crate::throttle::{GroupControl, SystemGroup, paths, slots};
    let Some(data_dir) = &env.data_dir else {
        return Ok((Vec::new(), Vec::new()));
    };
    let data_dir = Path::new(data_dir);
    let cpus = thread::available_parallelism().map_or(1, usize::from);
    slots::prepare_for_guests(data_dir, cpus, &|pgid| SystemGroup.group_state(pgid))
        .map_err(|err| PortError::Io(format!("throttle directory: {err}")))?;
    let text = |path: PathBuf| path.to_string_lossy().into_owned();
    Ok((
        vec![text(paths::throttle_dir(data_dir))],
        vec![text(paths::guest_dir(data_dir))],
    ))
}

/// Creates every missing writable root that encloses a closed path, so Seatbelt
/// can guard the root entry without preventing the first build from creating it.
fn create_missing_roots<'a>(
    roots: &[String],
    closed: impl Iterator<Item = &'a String> + Clone,
) -> PortResult<()> {
    for root in roots {
        let root = Path::new(root);
        let guards = closed
            .clone()
            .any(|path| Path::new(path) != root && Path::new(path).starts_with(root));
        if guards && !root.exists() {
            std::fs::create_dir_all(root)
                .map_err(|err| PortError::Io(format!("create {}: {err}", root.display())))?;
        }
    }
    Ok(())
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
    on_line: Option<LineCallback>,
) -> PortResult<PtyOutput> {
    run_confined_with_tail(spec, request, env, account, on_line, DEFAULT_TAIL_BYTES)
}

/// [`run_confined`] with a caller-chosen bounded PTY output buffer cap.
pub(crate) fn run_confined_with_tail(
    mut spec: CommandSpec,
    request: &AgentRequest,
    env: &ExecEnv,
    account: AccountPaths,
    on_line: Option<LineCallback>,
    tail_bytes: usize,
) -> PortResult<PtyOutput> {
    reaper::validate_run_id(&request.run_id)?;
    // Where the environment is built, so a runner made without
    // `build_runner_with` is held to the same names.
    env::validate_env_pass(&env.env_pass)?;
    env::validate_machine_env_pass(&env.machine_env_pass)?;
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
        let (queue, queue_guest) = open_queue(env)?;
        let read_only = request.sandbox.read_only;
        // A linked worktree keeps its index, refs and objects in the main
        // repository's git dir. Not being in a repository is fine: then there
        // is no such directory to open up. Read-only runs never write to git.
        let git_dirs = if read_only {
            Vec::new()
        } else {
            match aaf_fp::git(
                Path::new(&request.cwd),
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                &[],
                aaf_fp::GIT_TIMEOUT,
            ) {
                Ok(git_dir) if !git_dir.is_empty() => vec![git_dir],
                _ => Vec::new(),
            }
        };
        let cargo_home = toolchain_home(env.cargo_home.as_deref(), &env.parent_env, "CARGO_HOME");
        let rustup_home =
            toolchain_home(env.rustup_home.as_deref(), &env.parent_env, "RUSTUP_HOME");
        let gradle_user_home = toolchain_home(
            env.gradle_user_home.as_deref(),
            &env.parent_env,
            "GRADLE_USER_HOME",
        );
        let mut writable = env.temp_dirs.clone();
        if !read_only {
            if let Some(home) = &env.home {
                writable.extend(BUILD_CACHE_DIRS.iter().map(|dir| format!("{home}/{dir}")));
            }
            writable.extend(env.sdk_dirs.iter().cloned());
            for custom in [&cargo_home, &rustup_home, &gradle_user_home]
                .into_iter()
                .flatten()
            {
                push_unique(&mut writable, custom.clone());
            }
        }
        writable.extend(account.writable);
        let mut write_denied = account.write_denied;
        let mut write_denied_prefix = Vec::new();
        for (base, entries) in [
            (env.home.as_deref(), CACHE_WRITE_DENIED.as_slice()),
            (cargo_home.as_deref(), CARGO_WRITE_DENIED.as_slice()),
            (gradle_user_home.as_deref(), GRADLE_WRITE_DENIED.as_slice()),
        ] {
            if let Some(base) = base {
                push_denied_entries(base, entries, &mut write_denied, &mut write_denied_prefix);
            }
        }
        for sdk in &env.sdk_dirs {
            write_denied.extend(SDK_WRITE_DENIED.iter().map(|dir| format!("{sdk}/{dir}")));
        }
        // PATH dirs inside writable roots are closed by the sandbox profile.
        let path_dirs: Vec<String> = env
            .parent_env
            .iter()
            .filter(|(name, _)| name == "PATH")
            .flat_map(|(_, value)| std::env::split_paths(value).collect::<Vec<_>>())
            .map(|dir| dir.to_string_lossy().into_owned())
            .collect();
        if !read_only
            && cfg!(target_os = "macos")
            && let Some(vol_tmp) =
                secondary_volume_temp_dir(Path::new(&request.cwd), env.home.as_deref())
        {
            writable.push(vol_tmp.to_string_lossy().into_owned());
        }
        let worktrees_root = env.worktrees_root.clone().or_else(|| {
            env.home
                .as_ref()
                .map(|home| format!("{home}/{DEFAULT_WORKTREES_REL}"))
        });
        if !read_only {
            for path in &request.sandbox.writable_paths {
                push_unique(&mut writable, path.clone());
            }
        }
        let worktree = (!read_only).then(|| request.cwd.clone());
        let mut read_denied: Vec<String> = env.data_dir.iter().cloned().collect();
        for (warm, explicit) in [
            (
                env.home
                    .as_ref()
                    .map(|h| format!("{h}/{DEFAULT_WARM_SLOTS_REL}")),
                false,
            ),
            (
                toolchain_home(None, &env.parent_env, "AAF_WARM_SLOTS_DIR"),
                true,
            ),
        ]
        .into_iter()
        .filter_map(|(opt, explicit)| opt.map(|p| (p, explicit)))
        {
            let warm_path = Path::new(&warm);
            let warm_canon = sandbox::canonical(&warm).ok();
            let under_writable = writable.iter().chain(&worktree).any(|root| {
                warm_path.starts_with(Path::new(root))
                    || warm_canon
                        .as_ref()
                        .zip(sandbox::canonical(root).ok())
                        .is_some_and(|(w, r)| w.starts_with(r))
            });
            if under_writable || (cfg!(target_os = "linux") && explicit) {
                let _ = crate::db::private::create_dir(warm_path);
            }
            push_unique(&mut write_denied, warm.clone());
            push_unique(&mut read_denied, warm);
        }
        if cfg!(target_os = "macos") {
            create_missing_roots(
                &writable,
                write_denied
                    .iter()
                    .chain(&write_denied_prefix)
                    .chain(&path_dirs),
            )?;
        }
        for path in &request.sandbox.read_denied_paths {
            push_unique(&mut read_denied, path.clone());
        }
        let extra = SandboxPaths {
            writable,
            write_denied,
            write_denied_prefix,
            // Data dir + warm-slots stay closed (`~/.claude` stays open for account symlinks).
            read_denied,
            read_reopened: queue,
            write_reopened: queue_guest,
            worktree,
            git_dirs,
            worktrees_root,
            path_dirs,
            run_mark: marks.as_ref().map(|marks| marks.mark.clone()),
        };
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

fn push_unique(list: &mut Vec<String>, item: String) {
    if !list.contains(&item) {
        list.push(item);
    }
}

/// Per-UID `.TemporaryItems` dir on macOS secondary volumes (required by Unity/Carbon).
pub(crate) fn secondary_volume_temp_dir(worktree: &Path, home: Option<&str>) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(worktree).unwrap_or_else(|_| worktree.to_path_buf());
    let uid = owner_uid(worktree).or_else(|| home.and_then(|h| owner_uid(Path::new(h))));
    secondary_volume_temp_dir_in(&canonical, uid, Path::exists)
}

#[cfg(unix)]
fn owner_uid(path: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|meta| meta.uid())
}

#[cfg(not(unix))]
fn owner_uid(_path: &Path) -> Option<u32> {
    None
}

pub(crate) fn secondary_volume_temp_dir_in(
    canonical: &Path,
    uid: Option<u32>,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    use std::path::Component;
    let mut comps = canonical.components();
    if comps.next() != Some(Component::RootDir)
        || comps.next() != Some(Component::Normal(OsStr::new("Volumes")))
    {
        return None;
    }
    let Component::Normal(vol) = comps.next()? else {
        return None;
    };
    if vol.is_empty() {
        return None;
    }
    let base = PathBuf::from("/Volumes").join(vol).join(".TemporaryItems");
    if let Some(uid) = uid {
        let per_uid = base.join(format!("folders.{uid}"));
        if exists(&per_uid) {
            return Some(per_uid);
        }
    }
    exists(&base).then_some(base)
}
