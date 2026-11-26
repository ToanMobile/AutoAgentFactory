//! Bubblewrap (`bwrap`) argument assembly and path validation.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::super::sandbox::SandboxPaths;
use super::ro_overlay::RoOverlay;

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
    let worktrees_roots: Vec<&Path> = paths
        .worktrees_root
        .iter()
        .chain(&paths.extra_worktrees_roots)
        .map(Path::new)
        .collect();
    let worktrees_roots_canon: Vec<PathBuf> = worktrees_roots
        .iter()
        .map(|root| canonical_or_self(root))
        .collect();
    let read_denied_canon: Vec<PathBuf> = paths
        .read_denied
        .iter()
        .map(|raw| canonical_or_self(Path::new(raw)))
        .collect();
    let mut bound_rw: BTreeSet<String> = BTreeSet::new();
    let tmp_canon = canonical_or_self(Path::new("/tmp"));
    let is_excluded = |canon: &Path| {
        worktrees_roots_canon
            .iter()
            .any(|root| canon.starts_with(root))
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

    // 2. Protect every worktrees root before opening a task worktree inside it.
    let (mut bound_ro, mut pinned) = (BTreeSet::new(), BTreeSet::new());
    let worktree_canon = paths
        .worktree
        .as_deref()
        .map(|wt| canonical_or_self(Path::new(wt)));
    let own_is_task_worktree = worktree_canon.as_ref().is_some_and(|wt| {
        worktrees_roots_canon
            .iter()
            .any(|root| wt != root && wt.starts_with(root))
    });
    if !own_is_task_worktree && let Some(worktree) = &paths.worktree {
        push_flag_pair(&mut argv, &mut bound_rw, "--bind", worktree);
    }
    let mut ro = RoOverlay::new(&mut argv, &mut bound_rw, &mut bound_ro, &mut pinned);
    for root in &worktrees_roots {
        if root.exists() {
            ro.bind_with_target(&leaf_resolved(root))?;
        }
    }
    if own_is_task_worktree && let Some(worktree) = &paths.worktree {
        ro.bind_rw(worktree);
    }
    for git_dir in &paths.git_dirs {
        ro.bind_rw(git_dir);
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
            ".autofactory/config.toml",
            ".autofactory/memory.md",
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

pub(super) fn validate_abs_path(path: &str, label: &str) -> Result<(), String> {
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

pub(super) fn canonical_or_self(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub(super) fn is_leaf_symlink(path: &Path) -> bool {
    let normalized: PathBuf = path.components().collect();
    std::fs::symlink_metadata(&normalized).is_ok_and(|m| m.file_type().is_symlink())
}

pub(super) fn leaf_resolved(path: &Path) -> PathBuf {
    if is_leaf_symlink(path) {
        canonical_or_self(path)
    } else {
        path.to_path_buf()
    }
}

pub(super) fn abs_path_str<'a>(path: &'a Path, label: &str) -> Result<&'a str, String> {
    let text = path
        .to_str()
        .ok_or_else(|| format!("path is not valid UTF-8: {}", path.display()))?;
    validate_abs_path(text, label)?;
    Ok(text)
}

pub(super) fn push_flag_pair(
    argv: &mut Vec<String>,
    seen: &mut BTreeSet<String>,
    flag: &str,
    path: &str,
) {
    if seen.insert(path.to_string()) {
        argv.extend([flag.into(), path.to_string(), path.to_string()]);
    }
}

pub(super) fn push_flag_path(
    argv: &mut Vec<String>,
    seen: &mut BTreeSet<String>,
    flag: &str,
    path: &Path,
) -> Result<(), String> {
    push_flag_pair(argv, seen, flag, abs_path_str(path, "bind path")?);
    Ok(())
}
