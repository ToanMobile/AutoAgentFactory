//! Sandbox path assembly for confined agent runs (Seatbelt / Bubblewrap).

use std::path::{Path, PathBuf};
use std::thread;

use aaf_domain::ports::{AgentRequest, PortError, PortResult};

use super::env::{push_denied_entries, toolchain_home};
use super::sandbox::{self, SandboxPaths, secondary_volume_temp_dir};
use super::{
    AccountPaths, BUILD_CACHE_DIRS, DEFAULT_WARM_SLOTS_REL, DEFAULT_WORKTREES_REL, ExecEnv,
    should_deny_macos_keychain,
};

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

fn push_unique(list: &mut Vec<String>, item: String) {
    if !list.contains(&item) {
        list.push(item);
    }
}

pub(super) fn build_sandbox_paths(
    request: &AgentRequest,
    env: &ExecEnv,
    account: AccountPaths,
    executor: &str,
    passed_env: &[(String, String)],
    extra_caches: &[String],
    run_mark: Option<String>,
) -> PortResult<SandboxPaths> {
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
    let rustup_home = toolchain_home(env.rustup_home.as_deref(), &env.parent_env, "RUSTUP_HOME");
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
        for cache in extra_caches {
            push_unique(&mut writable, cache.clone());
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
        let hydrated =
            crate::git::hydration::worktree_write_denied_strings(Path::new(&request.cwd))
                .map_err(|err| PortError::Io(format!("worktree hydration: {err}")))?;
        for path in hydrated {
            push_unique(&mut write_denied, path);
        }
    }
    let worktree = (!read_only).then(|| request.cwd.clone());
    let mut read_denied: Vec<String> = env.data_dir.iter().cloned().collect();
    // Defense-in-depth (S-UI-1): block reading any legacy ui-service.token in config_dir
    // even though current builds store it under `AAF_DATA_DIR` (already read-denied above).
    if let Some(cfg_dir) = toolchain_home(None, &env.parent_env, "AAF_CONFIG_DIR") {
        push_unique(
            &mut read_denied,
            format!("{}/ui-service.token", cfg_dir.trim_end_matches('/')),
        );
    }
    if let Some(home) = env.home.as_deref().filter(|h| !h.is_empty()) {
        let home = home.trim_end_matches('/');
        push_unique(
            &mut read_denied,
            format!("{home}/.config/autofactory/ui-service.token"),
        );
        push_unique(
            &mut read_denied,
            format!("{home}/.local/share/autofactory/ui-service.token"),
        );
    }
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
            let _ = aaf_fp::fs_private::create_dir(warm_path);
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
    let deny_keychain_dir = if cfg!(target_os = "macos")
        && should_deny_macos_keychain(env.keychain_policy, executor, passed_env)
    {
        env.home
            .as_deref()
            .filter(|h| !h.is_empty())
            .map(|home| format!("{}/Library/Keychains", home.trim_end_matches('/')))
    } else {
        None
    };
    Ok(SandboxPaths {
        writable,
        writable_regex: account.writable_regex,
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
        run_mark,
        deny_keychain_dir,
    })
}
