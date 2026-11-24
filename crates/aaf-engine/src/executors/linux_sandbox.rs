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
    /// Running inside an OCI/container runtime (Docker, Podman, Kubernetes, containerd, LXC).
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
            Self::Bubblewrap { bwrap_bin } => format!("bubblewrap ({})", bwrap_bin.display()),
            Self::HardenedContainer { indicator } => format!("hardened container ({indicator})"),
            Self::None => {
                "none (install bubblewrap `bwrap` or run inside a hardened container)".to_string()
            }
        }
    }
}

/// Detects whether the current process appears to be running inside a Linux container.
pub fn detect_container(
    has_dockerenv: bool,
    cgroup_text: Option<&str>,
    env_container: Option<&str>,
) -> Option<&'static str> {
    if has_dockerenv {
        return Some("docker");
    }
    if let Some(trimmed) = env_container.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(match trimmed.to_ascii_lowercase().as_str() {
            "podman" => "podman",
            "docker" => "docker",
            "oci" => "oci",
            "lxc" | "lxd" => "lxc",
            "kubepods" | "kubernetes" => "kubernetes",
            "containerd" => "containerd",
            _ => "container",
        });
    }
    if let Some(lower) = cgroup_text.map(str::to_ascii_lowercase) {
        const MARKERS: [(&str, &str); 6] = [
            ("kubepods", "kubernetes"),
            ("containerd", "containerd"),
            ("docker", "docker"),
            ("podman", "podman"),
            ("libpod", "podman"),
            ("lxc", "lxc"),
        ];
        if let Some((_, label)) = MARKERS.iter().find(|(needle, _)| lower.contains(needle)) {
            return Some(label);
        }
    }
    None
}

/// Resolves the effective [`LinuxIsolation`] from deterministic probe inputs.
pub fn linux_sandbox_status(
    bwrap_bin: Option<PathBuf>,
    has_dockerenv: bool,
    cgroup_text: Option<&str>,
    env_container: Option<&str>,
) -> LinuxIsolation {
    if let Some(bwrap_bin) = bwrap_bin {
        return LinuxIsolation::Bubblewrap { bwrap_bin };
    }
    detect_container(has_dockerenv, cgroup_text, env_container)
        .map_or(LinuxIsolation::None, |indicator| {
            LinuxIsolation::HardenedContainer { indicator }
        })
}

/// Probes the current host environment for Linux sandbox capabilities.
pub fn detect_host_linux_isolation() -> LinuxIsolation {
    let cgroup_text = std::fs::read_to_string("/proc/1/cgroup")
        .or_else(|_| std::fs::read_to_string("/proc/self/cgroup"))
        .ok();
    let env_container = std::env::var("container")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            Path::new("/run/.containerenv")
                .exists()
                .then(|| "podman".into())
        });
    linux_sandbox_status(
        find_bwrap_binary(),
        Path::new("/.dockerenv").exists(),
        cgroup_text.as_deref(),
        env_container.as_deref(),
    )
}

fn find_bwrap_binary() -> Option<PathBuf> {
    std::env::var_os("PATH")
        .and_then(|p| super::find_on_path(&p, "bwrap"))
        .or_else(|| {
            ["/usr/bin/bwrap", "/bin/bwrap"]
                .iter()
                .map(PathBuf::from)
                .find(|p| p.is_file())
        })
}

const BASE_BWRAP_ARGS: &str = concat!(
    "--die-with-parent --unshare-pid --unshare-ipc --new-session ",
    "--ro-bind / / --dev /dev --proc /proc --tmpfs /tmp",
);

/// Build-cache roots that stay read-only (via `--ro-bind / /`) so agents can
/// never create `init.gradle.kts`, `init.d/*`, `.cargo/config.toml`, `bin/*`,
/// or `adbkey*`; only existing safe data subdirectories are bound writable.
fn ro_cache_safe_subdirs(path: &Path, canon: &Path) -> Option<&'static [&'static str]> {
    let name = path
        .file_name()
        .or_else(|| canon.file_name())
        .and_then(|n| n.to_str())?;
    match name {
        ".gradle" => Some(&["caches", "wrapper", "daemon"]),
        ".cargo" => Some(&["registry", "git"]),
        ".m2" => Some(&["repository"]),
        ".pub-cache" => Some(&["hosted", "git"]),
        ".android" => Some(&["cache", "build-cache"]),
        _ => None,
    }
}

/// Builds a least-privilege Bubblewrap (`bwrap`) argv for `program` with `args`.
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

    let mut argv: Vec<String> = BASE_BWRAP_ARGS
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let worktrees_root = paths.worktrees_root.as_deref().map(Path::new);
    let worktrees_root_canon = worktrees_root.map(canonical_or_self);
    let read_denied_canon: Vec<PathBuf> = paths
        .read_denied
        .iter()
        .map(|raw| canonical_or_self(Path::new(raw)))
        .collect();
    let mut bound_rw: BTreeSet<String> = BTreeSet::new();
    let tmp_canon = canonical_or_self(Path::new("/tmp"));
    let is_excluded = |canon: &Path| {
        worktrees_root_canon
            .as_ref()
            .is_some_and(|root| canon.starts_with(root))
            || read_denied_canon.iter().any(|d| canon.starts_with(d))
    };
    // 1. Existing writable caches, SDKs, account dirs, and non-/tmp temp dirs.
    for raw in &paths.writable {
        let path = Path::new(raw);
        let canon = canonical_or_self(path);
        if path == Path::new("/tmp") || canon == tmp_canon || is_excluded(&canon) || !path.exists()
        {
            continue;
        }
        if let Some(subdirs) = ro_cache_safe_subdirs(path, &canon) {
            for sub in subdirs {
                let sub_path = path.join(sub);
                if sub_path.exists() {
                    let sub_canon = canonical_or_self(&sub_path);
                    if !is_excluded(&sub_canon) {
                        push_flag_path(&mut argv, &mut bound_rw, "--bind", &sub_path)?;
                    }
                }
            }
        } else {
            push_flag_pair(&mut argv, &mut bound_rw, "--bind", raw);
        }
    }

    // 2. Protect `worktrees_root` before opening a task worktree inside it.
    let (mut bound_ro, mut pinned) = (BTreeSet::new(), BTreeSet::new());
    let worktree_canon = paths
        .worktree
        .as_deref()
        .map(|wt| canonical_or_self(Path::new(wt)));
    let own_is_task_worktree = match (worktree_canon.as_ref(), worktrees_root_canon.as_ref()) {
        (Some(wt), Some(root)) => wt != root && wt.starts_with(root),
        _ => false,
    };
    if !own_is_task_worktree && let Some(worktree) = &paths.worktree {
        push_flag_pair(&mut argv, &mut bound_rw, "--bind", worktree);
    }
    let mut ro = RoOverlay {
        argv: &mut argv,
        bound_rw: &mut bound_rw,
        bound_ro: &mut bound_ro,
        pinned: &mut pinned,
    };
    if let Some(root) = worktrees_root
        && root.exists()
    {
        ro.bind_with_target(&leaf_resolved(root))?;
    }
    if own_is_task_worktree && let Some(worktree) = &paths.worktree {
        push_flag_pair(ro.argv, ro.bound_rw, "--bind", worktree);
    }
    for git_dir in &paths.git_dirs {
        push_flag_pair(ro.argv, ro.bound_rw, "--bind", git_dir);
    }

    // 3. Read-only overlays for sensitive files/dirs inside writable subtrees.
    for git_dir in &paths.git_dirs {
        let gd = Path::new(git_dir);
        let is_primary = worktree_canon
            .as_ref()
            .is_some_and(|wt| canonical_or_self(&wt.join(".git")) == canonical_or_self(gd));
        ro.protect_git_dir(gd, is_primary)?;
    }
    if let Some(worktree) = &paths.worktree {
        let wt = Path::new(worktree);
        // Protect linked worktree `.git` pointer file without locking normal `.git/` dirs.
        let dot_git = wt.join(".git");
        if dot_git.is_file() {
            ro.bind_with_target(&dot_git)?;
        }
        for rel in [
            ".claude/settings.json",
            ".claude/settings.local.json",
            ".gemini/settings.json",
            ".gemini/trusted_hooks.json",
        ] {
            ro.protect_existing(&wt.join(rel))?;
        }
    }
    for denied in &paths.write_denied {
        ro.protect_existing(Path::new(denied))?;
    }
    for prefix in &paths.write_denied_prefix {
        ro.protect_prefix(Path::new(prefix))?;
    }
    for raw in &paths.path_dirs {
        let dir = Path::new(raw);
        if dir.is_absolute() && dir.exists() {
            ro.bind_with_target(dir)?;
        }
    }

    // 4. Mask `read_denied` (including `AAF_DATA_DIR`) to protect `receipt.key`.
    for denied in &paths.read_denied {
        ro.mask_read_denied(Path::new(denied))?;
    }

    // 5. Re-open minimal throttle queue directories inside `read_denied`.
    let (mut reopened_ro, mut reopened_rw) = (BTreeSet::new(), BTreeSet::new());
    for (list, seen, flag) in [
        (&paths.read_reopened, &mut reopened_ro, "--ro-bind"),
        (&paths.write_reopened, &mut reopened_rw, "--bind"),
    ] {
        for raw in list.iter().filter(|r| Path::new(r).exists()) {
            push_flag_pair(&mut argv, seen, flag, raw);
        }
    }
    for denied in &paths.read_denied {
        let throttle = leaf_resolved(Path::new(denied)).join("throttle");
        let guest = throttle.join("guest");
        if guest.exists() {
            if throttle.exists() {
                push_flag_path(&mut argv, &mut reopened_ro, "--ro-bind", &throttle)?;
            }
            push_flag_path(&mut argv, &mut reopened_rw, "--bind", &guest)?;
        }
    }
    if let Some(mark) = &paths.run_mark
        && Path::new(mark).exists()
    {
        push_flag_pair(&mut argv, &mut reopened_rw, "--bind", mark);
    }

    // 6. Working directory and target command.
    if let Some(worktree) = &paths.worktree {
        argv.extend(["--chdir".into(), worktree.clone()]);
    }
    argv.extend(["--".into(), program.to_string()]);
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
    for (opt, label) in [
        (&paths.worktree, "worktree"),
        (&paths.worktrees_root, "worktrees_root"),
        (&paths.run_mark, "run_mark"),
        (&paths.deny_keychain_dir, "deny_keychain_dir"),
    ] {
        if let Some(path) = opt {
            validate_abs_path(path, label)?;
        }
    }
    for (list, label) in [
        (&paths.writable, "writable"),
        (&paths.write_denied, "write_denied"),
        (&paths.write_denied_prefix, "write_denied_prefix"),
        (&paths.read_denied, "read_denied"),
        (&paths.read_reopened, "read_reopened"),
        (&paths.write_reopened, "write_reopened"),
        (&paths.git_dirs, "git_dirs"),
    ] {
        for (idx, item) in list.iter().enumerate() {
            validate_abs_path(item, &format!("{label}[{idx}]"))?;
        }
    }
    for (list, label) in [
        (&paths.path_dirs, "path_dirs"),
        (&paths.writable_regex, "writable_regex"),
    ] {
        for (idx, entry) in list.iter().enumerate() {
            validate_no_nul(entry, &format!("{label}[{idx}]"))?;
        }
    }
    Ok(())
}

fn validate_abs_path(path: &str, label: &str) -> Result<(), String> {
    validate_non_empty_no_nul(path, label)?;
    Path::new(path)
        .is_absolute()
        .then_some(())
        .ok_or_else(|| format!("{label} must be an absolute path: {path:?}"))
}

fn validate_non_empty_no_nul(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{label} must not be empty"));
    }
    validate_no_nul(value, label)
}

fn validate_no_nul(value: &str, label: &str) -> Result<(), String> {
    (!value.as_bytes().contains(&0))
        .then_some(())
        .ok_or_else(|| format!("{label} contains a NUL byte"))
}

fn canonical_or_self(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn is_leaf_symlink(path: &Path) -> bool {
    let normalized: PathBuf = path.components().collect();
    std::fs::symlink_metadata(&normalized).is_ok_and(|m| m.file_type().is_symlink())
}

fn leaf_resolved(path: &Path) -> PathBuf {
    if is_leaf_symlink(path) {
        canonical_or_self(path)
    } else {
        path.to_path_buf()
    }
}

struct RoOverlay<'a> {
    argv: &'a mut Vec<String>,
    bound_rw: &'a mut BTreeSet<String>,
    bound_ro: &'a mut BTreeSet<String>,
    pinned: &'a mut BTreeSet<String>,
}

fn deepest_enclosing(set: &BTreeSet<String>, target_canon: &Path) -> Option<PathBuf> {
    set.iter()
        .map(|entry| canonical_or_self(Path::new(entry)))
        .filter(|cand| target_canon.starts_with(cand))
        .max_by_key(|cand| cand.components().count())
}

fn contains_canon(set: &BTreeSet<String>, target_canon: &Path) -> bool {
    target_canon.to_str().is_some_and(|text| set.contains(text))
        || set
            .iter()
            .any(|entry| canonical_or_self(Path::new(entry)) == target_canon)
}

impl RoOverlay<'_> {
    /// Self-binds intermediate dirs between a writable root and `path.parent()`
    /// so `mv` fails with `EBUSY` instead of bypassing RO overlays below them.
    fn pin_ancestors(&mut self, path: &Path) -> Result<(), String> {
        let Some(parent) = path.parent() else {
            return Ok(());
        };
        let parent_canon = canonical_or_self(parent);
        let Some(rw_root) = deepest_enclosing(self.bound_rw, &parent_canon) else {
            return Ok(());
        };
        if let Some(ro_root) = deepest_enclosing(self.bound_ro, &parent_canon)
            && ro_root.components().count() >= rw_root.components().count()
        {
            return Ok(());
        }
        let Ok(rel) = parent_canon.strip_prefix(&rw_root) else {
            return Ok(());
        };
        let mut cur = rw_root;
        for comp in rel.components() {
            cur.push(comp);
            let cur_canon = canonical_or_self(&cur);
            if !cur_canon.is_dir()
                || contains_canon(self.bound_ro, &cur_canon)
                || contains_canon(self.bound_rw, &cur_canon)
            {
                continue;
            }
            let text = abs_path_str(&cur_canon, "pin path")?;
            push_flag_pair(self.argv, self.pinned, "--bind", text);
        }
        Ok(())
    }

    fn mask_one(&mut self, target: &Path, is_dir: bool) -> Result<(), String> {
        self.pin_ancestors(target)?;
        let s = abs_path_str(target, "read_denied path")?.to_string();
        if self.bound_ro.insert(s.clone()) {
            if is_dir {
                self.argv.extend(["--tmpfs".into(), s]);
            } else {
                self.argv
                    .extend(["--ro-bind".into(), "/dev/null".into(), s]);
            }
        }
        Ok(())
    }

    fn mask_read_denied(&mut self, path: &Path) -> Result<(), String> {
        let leaf_sym = is_leaf_symlink(path);
        if !leaf_sym && path.exists() {
            self.mask_one(path, path.is_dir())?;
        }
        if let Ok(resolved) = std::fs::canonicalize(path)
            && (leaf_sym || resolved != path)
            && resolved.exists()
        {
            self.mask_one(&resolved, resolved.is_dir())?;
        }
        Ok(())
    }

    fn bind_with_target(&mut self, path: &Path) -> Result<(), String> {
        self.pin_ancestors(path)?;
        push_flag_path(self.argv, self.bound_ro, "--ro-bind", path)?;
        if let Ok(resolved) = std::fs::canonicalize(path)
            && resolved != path
            && resolved.exists()
        {
            self.pin_ancestors(&resolved)?;
            push_flag_path(self.argv, self.bound_ro, "--ro-bind", &resolved)?;
        }
        Ok(())
    }

    fn protect_existing(&mut self, path: &Path) -> Result<(), String> {
        if path.exists() {
            self.bind_with_target(path)?;
        }
        Ok(())
    }

    fn protect_git_dir(&mut self, git_dir: &Path, is_primary_checkout: bool) -> Result<(), String> {
        for rel in ["hooks", "refs/aaf", "config", "config.worktree"] {
            self.protect_existing(&git_dir.join(rel))?;
        }
        for rel in ["commondir", "info/exclude", "packed-refs"] {
            self.protect_existing(&git_dir.join(rel))?;
        }
        if !is_primary_checkout {
            for rel in ["HEAD", "index", "refs/heads/main", "refs/heads/master"] {
                self.protect_existing(&git_dir.join(rel))?;
            }
            for rel in ["refs/heads/develop", "refs/heads/release"] {
                self.protect_existing(&git_dir.join(rel))?;
            }
        }
        let worktrees = git_dir.join("worktrees");
        if worktrees.is_dir() {
            self.bind_with_target(&worktrees)?;
            if let Ok(entries) = std::fs::read_dir(&worktrees) {
                let mut children: Vec<PathBuf> = entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect();
                children.sort();
                for child in children {
                    push_flag_path(self.argv, self.bound_rw, "--bind", &child)?;
                    for rel in ["hooks", "config", "config.worktree", "commondir", "gitdir"] {
                        self.protect_existing(&child.join(rel))?;
                    }
                }
            }
        }
        Ok(())
    }

    fn protect_prefix(&mut self, prefix_path: &Path) -> Result<(), String> {
        let (Some(parent), Some(prefix_name)) = (
            prefix_path.parent(),
            prefix_path.file_name().and_then(|n| n.to_str()),
        ) else {
            return Ok(());
        };
        if let Ok(entries) = std::fs::read_dir(parent) {
            let mut matches: Vec<PathBuf> = entries
                .flatten()
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|n| n.starts_with(prefix_name))
                })
                .map(|e| e.path())
                .collect();
            matches.sort();
            for path in matches {
                self.protect_existing(&path)?;
            }
        }
        Ok(())
    }
}

fn abs_path_str<'a>(path: &'a Path, label: &str) -> Result<&'a str, String> {
    let text = path
        .to_str()
        .ok_or_else(|| format!("path is not valid UTF-8: {}", path.display()))?;
    validate_abs_path(text, label)?;
    Ok(text)
}

fn push_flag_pair(argv: &mut Vec<String>, seen: &mut BTreeSet<String>, flag: &str, path: &str) {
    if seen.insert(path.to_string()) {
        argv.extend([flag.into(), path.to_string(), path.to_string()]);
    }
}

fn push_flag_path(
    argv: &mut Vec<String>,
    seen: &mut BTreeSet<String>,
    flag: &str,
    path: &Path,
) -> Result<(), String> {
    push_flag_pair(argv, seen, flag, abs_path_str(path, "bind path")?);
    Ok(())
}
