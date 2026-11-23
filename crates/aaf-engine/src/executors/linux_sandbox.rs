//! Linux Bubblewrap (`bwrap`) and hardened-container sandbox support.
//!
//! This module is intentionally cross-platform pure Rust (no Linux-only
//! syscalls and no `unsafe`) so its policy builder, container detector, and
//! tests run identically on macOS and Linux.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use aaf_domain::ports::{PortError, PortResult};

use super::pty::CommandSpec;
use super::sandbox::SandboxPaths;

/// Detected or configured isolation mechanism on Linux hosts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinuxIsolation {
    /// Per-run namespace and mount confinement via `bwrap` (Bubblewrap).
    Bubblewrap { bwrap_bin: PathBuf },
    /// Running inside an OCI/container runtime (Docker, Podman, Kubernetes,
    /// containerd, LXC) where the container boundary provides isolation.
    HardenedContainer { indicator: &'static str },
    /// Neither Bubblewrap nor a recognized container runtime was detected.
    None,
}

impl LinuxIsolation {
    /// Returns `true` when at least one Linux isolation layer is active.
    pub fn is_isolated(&self) -> bool {
        !matches!(self, Self::None)
    }

    /// Returns a concise human-readable summary suitable for `aaf doctor`.
    pub fn summary(&self) -> String {
        match self {
            Self::Bubblewrap { bwrap_bin } => {
                format!("bubblewrap ({})", bwrap_bin.display())
            }
            Self::HardenedContainer { indicator } => {
                format!("hardened container ({indicator})")
            }
            Self::None => {
                "none (install bubblewrap `bwrap` or run inside a hardened container)".to_string()
            }
        }
    }
}

/// Detects whether the current process appears to be running inside a Linux
/// container from pure probe inputs (safe for deterministic unit tests).
///
/// Checks Docker (`/.dockerenv`), OCI/Podman `container` environment variable,
/// and `/proc/1/cgroup` (or `/proc/self/cgroup`) markers (`kubepods`,
/// `containerd`, `docker`, `podman`/`libpod`, `lxc`).
pub fn detect_container(
    has_dockerenv: bool,
    cgroup_text: Option<&str>,
    env_container: Option<&str>,
) -> Option<&'static str> {
    if has_dockerenv {
        return Some("docker");
    }
    if let Some(raw) = env_container {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            let lower = trimmed.to_ascii_lowercase();
            return Some(match lower.as_str() {
                "podman" => "podman",
                "docker" => "docker",
                "oci" => "oci",
                "lxc" | "lxd" => "lxc",
                "kubepods" | "kubernetes" => "kubernetes",
                "containerd" => "containerd",
                _ => "container",
            });
        }
    }
    if let Some(cgroup) = cgroup_text {
        let lower = cgroup.to_ascii_lowercase();
        if lower.contains("kubepods") {
            return Some("kubernetes");
        }
        if lower.contains("containerd") {
            return Some("containerd");
        }
        if lower.contains("docker") {
            return Some("docker");
        }
        if lower.contains("podman") || lower.contains("libpod") {
            return Some("podman");
        }
        if lower.contains("lxc") {
            return Some("lxc");
        }
    }
    None
}

/// Resolves the effective [`LinuxIsolation`] from deterministic probe inputs.
///
/// Bubblewrap is preferred when available because it enforces per-run worktree
/// and `AAF_DATA_DIR` isolation; otherwise falls back to container detection.
pub fn linux_sandbox_status(
    bwrap_bin: Option<PathBuf>,
    has_dockerenv: bool,
    cgroup_text: Option<&str>,
    env_container: Option<&str>,
) -> LinuxIsolation {
    if let Some(bwrap_bin) = bwrap_bin {
        return LinuxIsolation::Bubblewrap { bwrap_bin };
    }
    if let Some(indicator) = detect_container(has_dockerenv, cgroup_text, env_container) {
        return LinuxIsolation::HardenedContainer { indicator };
    }
    LinuxIsolation::None
}

/// Probes the current host environment for Linux sandbox capabilities
/// (`bwrap` binary, `/.dockerenv`, `/run/.containerenv`, `/proc/1/cgroup`, and
/// the `container` environment variable).
pub fn detect_host_linux_isolation() -> LinuxIsolation {
    let bwrap_bin = find_bwrap_binary();
    let has_dockerenv = Path::new("/.dockerenv").exists();
    let cgroup_text = std::fs::read_to_string("/proc/1/cgroup")
        .or_else(|_| std::fs::read_to_string("/proc/self/cgroup"))
        .ok();
    let env_container = std::env::var("container")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            Path::new("/run/.containerenv")
                .exists()
                .then(|| "podman".to_string())
        });

    linux_sandbox_status(
        bwrap_bin,
        has_dockerenv,
        cgroup_text.as_deref(),
        env_container.as_deref(),
    )
}

fn find_bwrap_binary() -> Option<PathBuf> {
    if let Some(path_var) = std::env::var_os("PATH")
        && let Some(found) = super::find_on_path(&path_var, "bwrap")
    {
        return Some(found);
    }
    for standard in ["/usr/bin/bwrap", "/bin/bwrap"] {
        let candidate = PathBuf::from(standard);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Builds a least-privilege Bubblewrap (`bwrap`) argument vector for running
/// `program` with `args` under the confinement described by `paths`.
///
/// Mount order is security-critical because Bubblewrap applies mounts
/// sequentially:
/// 1. Process/IPC/session isolation (`--die-with-parent`, `--unshare-pid`,
///    `--unshare-ipc`, `--new-session`).
/// 2. Read-only rootfs (`--ro-bind / /`), device nodes (`--dev /dev`),
///    procfs (`--proc /proc`), and private `--tmpfs /tmp`.
/// 3. Writable binds for existing build caches / account dirs, the run's own
///    worktree, and git common directories.
/// 4. Read-only re-mounts over sensitive subpaths inside writable trees (git
///    `hooks`/`config`/`refs/aaf`, linked worktree metadata, `.claude/settings.json`,
///    `write_denied`, `write_denied_prefix`, and writable `PATH` entries).
/// 5. Complete masking of `read_denied` paths (including `AAF_DATA_DIR`, hiding
///    `receipt.key`, receipts, and approvals) via `--tmpfs <dir>`.
/// 6. Minimal re-binds inside `read_denied` for `<data_dir>/throttle` (read-only),
///    `<data_dir>/throttle/guest` (writable), and `run_mark` when present.
pub fn build_bwrap_args(
    paths: &SandboxPaths,
    program: &str,
    args: &[String],
) -> Result<Vec<String>, String> {
    validate_non_empty_no_nul(program, "program")?;
    for (idx, arg) in args.iter().enumerate() {
        validate_no_nul(arg, &format!("args[{idx}]"))?;
    }
    validate_sandbox_paths(paths)?;

    let mut argv: Vec<String> = vec![
        "--die-with-parent".to_string(),
        "--unshare-pid".to_string(),
        "--unshare-ipc".to_string(),
        "--new-session".to_string(),
        "--ro-bind".to_string(),
        "/".to_string(),
        "/".to_string(),
        "--dev".to_string(),
        "/dev".to_string(),
        "--proc".to_string(),
        "/proc".to_string(),
        "--tmpfs".to_string(),
        "/tmp".to_string(),
    ];

    let worktrees_root = paths.worktrees_root.as_deref().map(Path::new);
    let read_denied_paths: Vec<&Path> = paths.read_denied.iter().map(Path::new).collect();
    let mut bound_rw: BTreeSet<String> = BTreeSet::new();

    // 1. Existing writable build caches, SDKs, account directories, and non-/tmp temp dirs.
    for raw in &paths.writable {
        if raw == "/tmp" {
            continue;
        }
        let path = Path::new(raw);
        if worktrees_root.is_some_and(|root| path.starts_with(root)) {
            continue;
        }
        if read_denied_paths
            .iter()
            .any(|denied| path.starts_with(denied))
        {
            continue;
        }
        if path.exists() {
            push_bind_once(&mut argv, &mut bound_rw, raw);
        }
    }

    // 2. The run's own worktree and git common directories.
    if let Some(worktree) = &paths.worktree {
        push_bind_once(&mut argv, &mut bound_rw, worktree);
    }
    for git_dir in &paths.git_dirs {
        push_bind_once(&mut argv, &mut bound_rw, git_dir);
    }

    // 3. Read-only overlays for sensitive files/dirs inside writable subtrees.
    let mut bound_ro: BTreeSet<String> = BTreeSet::new();
    for git_dir in &paths.git_dirs {
        protect_git_dir(Path::new(git_dir), &mut argv, &mut bound_ro)?;
    }
    if let Some(worktree) = &paths.worktree {
        let wt = Path::new(worktree);
        // Protect linked worktree `.git` pointer file without locking normal `.git/` dirs.
        let dot_git = wt.join(".git");
        if dot_git.is_file() {
            push_ro_bind_path(&mut argv, &mut bound_ro, &dot_git)?;
        }
        let claude_settings = wt.join(".claude").join("settings.json");
        if claude_settings.exists() {
            push_ro_bind_path(&mut argv, &mut bound_ro, &claude_settings)?;
        }
    }
    for denied in &paths.write_denied {
        if Path::new(denied).exists() {
            push_ro_bind_once(&mut argv, &mut bound_ro, denied);
        }
    }
    for prefix in &paths.write_denied_prefix {
        protect_write_denied_prefix(Path::new(prefix), &mut argv, &mut bound_ro)?;
    }
    for raw in &paths.path_dirs {
        let dir = Path::new(raw);
        if dir.is_absolute() && dir.exists() {
            push_ro_bind_once(&mut argv, &mut bound_ro, raw);
        }
    }

    // 4. Mask `read_denied` (including `AAF_DATA_DIR`) so the agent can never
    //    read `receipt.key` or forge receipts.
    for denied in &paths.read_denied {
        if Path::new(denied).is_file() {
            argv.push("--ro-bind".to_string());
            argv.push("/dev/null".to_string());
            argv.push(denied.clone());
        } else {
            argv.push("--tmpfs".to_string());
            argv.push(denied.clone());
        }
    }

    // 5. Re-open minimal throttle queue directories inside `read_denied`.
    let mut reopened_ro: BTreeSet<String> = BTreeSet::new();
    let mut reopened_rw: BTreeSet<String> = BTreeSet::new();
    for raw in &paths.read_reopened {
        if Path::new(raw).exists() {
            push_ro_bind_once(&mut argv, &mut reopened_ro, raw);
        }
    }
    for raw in &paths.write_reopened {
        if Path::new(raw).exists() {
            push_bind_once(&mut argv, &mut reopened_rw, raw);
        }
    }
    for denied in &paths.read_denied {
        let throttle = Path::new(denied).join("throttle");
        let guest = throttle.join("guest");
        if guest.exists() {
            if throttle.exists() {
                push_ro_bind_path(&mut argv, &mut reopened_ro, &throttle)?;
            }
            push_bind_path(&mut argv, &mut reopened_rw, &guest)?;
        }
    }
    if let Some(mark) = &paths.run_mark
        && Path::new(mark).exists()
    {
        push_bind_once(&mut argv, &mut reopened_rw, mark);
    }

    // 6. Working directory and target command.
    if let Some(worktree) = &paths.worktree {
        argv.push("--chdir".to_string());
        argv.push(worktree.clone());
    }
    argv.push("--".to_string());
    argv.push(program.to_string());
    argv.extend(args.iter().cloned());

    Ok(argv)
}

/// Rewrites `command` to execute under Bubblewrap (`bwrap_bin`) with `paths`.
pub fn wrap(
    bwrap_bin: &Path,
    paths: &SandboxPaths,
    command: &CommandSpec,
) -> PortResult<CommandSpec> {
    let bwrap_str = bwrap_bin
        .to_str()
        .ok_or_else(|| PortError::Invalid("bwrap binary path is not valid UTF-8".to_string()))?;
    validate_abs_path(bwrap_str, "bwrap_bin").map_err(PortError::Invalid)?;
    let args =
        build_bwrap_args(paths, &command.program, &command.args).map_err(PortError::Invalid)?;
    Ok(CommandSpec {
        program: bwrap_str.to_string(),
        args,
        cwd: command.cwd.clone(),
        env: command.env.clone(),
        env_clear: command.env_clear,
    })
}

fn validate_sandbox_paths(paths: &SandboxPaths) -> Result<(), String> {
    if let Some(wt) = &paths.worktree {
        validate_abs_path(wt, "worktree")?;
    }
    if let Some(root) = &paths.worktrees_root {
        validate_abs_path(root, "worktrees_root")?;
    }
    if let Some(mark) = &paths.run_mark {
        validate_abs_path(mark, "run_mark")?;
    }
    validate_abs_list(&paths.writable, "writable")?;
    validate_abs_list(&paths.write_denied, "write_denied")?;
    validate_abs_list(&paths.write_denied_prefix, "write_denied_prefix")?;
    validate_abs_list(&paths.read_denied, "read_denied")?;
    validate_abs_list(&paths.read_reopened, "read_reopened")?;
    validate_abs_list(&paths.write_reopened, "write_reopened")?;
    validate_abs_list(&paths.git_dirs, "git_dirs")?;
    for (idx, entry) in paths.path_dirs.iter().enumerate() {
        validate_no_nul(entry, &format!("path_dirs[{idx}]"))?;
    }
    Ok(())
}

fn validate_abs_list(list: &[String], label: &str) -> Result<(), String> {
    for (idx, item) in list.iter().enumerate() {
        validate_abs_path(item, &format!("{label}[{idx}]"))?;
    }
    Ok(())
}

fn validate_abs_path(path: &str, label: &str) -> Result<(), String> {
    validate_non_empty_no_nul(path, label)?;
    if !Path::new(path).is_absolute() {
        return Err(format!("{label} must be an absolute path: {path:?}"));
    }
    Ok(())
}

fn validate_non_empty_no_nul(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{label} must not be empty"));
    }
    validate_no_nul(value, label)
}

fn validate_no_nul(value: &str, label: &str) -> Result<(), String> {
    if value.as_bytes().contains(&0) {
        return Err(format!("{label} contains a NUL byte"));
    }
    Ok(())
}

fn protect_git_dir(
    git_dir: &Path,
    argv: &mut Vec<String>,
    bound_ro: &mut BTreeSet<String>,
) -> Result<(), String> {
    for rel in ["hooks", "config", "refs/aaf"] {
        let candidate = git_dir.join(rel);
        if candidate.exists() {
            push_ro_bind_path(argv, bound_ro, &candidate)?;
        }
    }
    let worktrees_dir = git_dir.join("worktrees");
    if let Ok(entries) = std::fs::read_dir(&worktrees_dir) {
        let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        children.sort();
        for child in children {
            for rel in ["hooks", "config", "config.worktree", "commondir", "gitdir"] {
                let candidate = child.join(rel);
                if candidate.exists() {
                    push_ro_bind_path(argv, bound_ro, &candidate)?;
                }
            }
        }
    }
    Ok(())
}

fn protect_write_denied_prefix(
    prefix_path: &Path,
    argv: &mut Vec<String>,
    bound_ro: &mut BTreeSet<String>,
) -> Result<(), String> {
    let Some(parent) = prefix_path.parent() else {
        return Ok(());
    };
    let Some(prefix_name) = prefix_path.file_name().and_then(|n| n.to_str()) else {
        return Ok(());
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Ok(());
    };
    let mut matches: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(prefix_name))
        })
        .map(|entry| entry.path())
        .collect();
    matches.sort();
    for path in matches {
        push_ro_bind_path(argv, bound_ro, &path)?;
    }
    Ok(())
}

fn push_bind_once(argv: &mut Vec<String>, seen: &mut BTreeSet<String>, path: &str) {
    if seen.insert(path.to_string()) {
        argv.push("--bind".to_string());
        argv.push(path.to_string());
        argv.push(path.to_string());
    }
}

fn push_ro_bind_once(argv: &mut Vec<String>, seen: &mut BTreeSet<String>, path: &str) {
    if seen.insert(path.to_string()) {
        argv.push("--ro-bind".to_string());
        argv.push(path.to_string());
        argv.push(path.to_string());
    }
}

fn push_bind_path(
    argv: &mut Vec<String>,
    seen: &mut BTreeSet<String>,
    path: &Path,
) -> Result<(), String> {
    let text = path
        .to_str()
        .ok_or_else(|| format!("path is not valid UTF-8: {}", path.display()))?;
    validate_abs_path(text, "bind path")?;
    push_bind_once(argv, seen, text);
    Ok(())
}

fn push_ro_bind_path(
    argv: &mut Vec<String>,
    seen: &mut BTreeSet<String>,
    path: &Path,
) -> Result<(), String> {
    let text = path
        .to_str()
        .ok_or_else(|| format!("path is not valid UTF-8: {}", path.display()))?;
    validate_abs_path(text, "ro-bind path")?;
    push_ro_bind_once(argv, seen, text);
    Ok(())
}
