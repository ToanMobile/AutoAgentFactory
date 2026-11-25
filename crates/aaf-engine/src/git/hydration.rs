//! Safe host-side worktree hydration (`[worktree]` in immutable `base_sha`).
//!
//! Clones gitignored dependency directories with copy-on-write (never
//! hardlinks), copies non-secret gitignored files, and stages secret files
//! exclusively on the host during `aaf gate` via RAII [`GateSecretStage`].

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use aaf_domain::config::RepoConfig;
use aaf_domain::worktree_config::{WorktreeConfig, is_secret_file};
mod cleanup;
pub mod device;
pub mod seed;
pub mod stage;

pub use device::{MountEntry, same_cow_device};
pub(crate) use stage::stage_for_gate_from_base;
pub use stage::{CloneSkip, GateSecretStage, remove_partial_dst, stage_for_gate};
use stage::{cow_clone_dir, run_setup_cmd};

use super::cmd::{self, git, git_raw};
use super::worktree::{self, TaskWorktree};
use super::{GitError, path_str};
use crate::confine_core::git_refs::{AUTO_FALLBACKS, origin_head_branch, stats_repo_id};
use aaf_domain::engine_contracts::REPO_CONFIG_REL;

/// Paths materialized into a task worktree that must remain read-only
/// (`SandboxPaths::write_denied`) during sandboxed agent turns.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HydratedPaths {
    pub write_denied: Vec<PathBuf>,
}

fn invalid(msg: impl Into<String>) -> GitError {
    GitError::Invalid(msg.into())
}

/// Validates a relative worktree hydration entry.
///
/// Rejects empty strings, absolute paths, leading `/` or `\`, NUL or control
/// characters, `.` / `..` components, backslashes, and `.git` / `.git/**`.
pub fn validate_relative_entry(entry: &str) -> Result<PathBuf, GitError> {
    if entry.is_empty()
        || entry.starts_with('/')
        || entry.starts_with('\\')
        || entry.contains('\0')
        || entry.contains('\\')
        || entry.chars().any(char::is_control)
    {
        return Err(invalid(format!(
            "invalid worktree hydration path {entry:?}"
        )));
    }
    let trimmed = entry.trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(invalid(format!(
            "invalid worktree hydration path {entry:?}"
        )));
    }
    let mut clean = PathBuf::new();
    for comp in Path::new(trimmed).components() {
        match comp {
            Component::Normal(name) => {
                let text = name
                    .to_str()
                    .ok_or_else(|| invalid(format!("non-UTF-8 hydration path {entry:?}")))?;
                if text.eq_ignore_ascii_case(".git") {
                    return Err(invalid(format!(
                        "hydration path must not touch .git: {entry:?}"
                    )));
                }
                clean.push(name);
            }
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(invalid(format!(
                    "hydration path must be a clean relative path: {entry:?}"
                )));
            }
        }
    }
    if clean.as_os_str().is_empty() || clean.is_absolute() {
        return Err(invalid(format!(
            "invalid worktree hydration path {entry:?}"
        )));
    }
    Ok(clean)
}

/// Fails closed unless `rel` is ignored by a git-tracked `.gitignore` file in
/// `repo_root` (never solely by `.git/info/exclude`, global `core.excludesfile`,
/// or an untracked `.gitignore`) and contains no git-tracked files.
pub fn assert_tracked_gitignore(repo_root: &Path, rel: &Path) -> Result<(), GitError> {
    assert_tracked_gitignore_inner(repo_root, rel, false)
}

pub(super) fn assert_tracked_gitignore_inner(
    repo_root: &Path,
    rel: &Path,
    expect_dir: bool,
) -> Result<(), GitError> {
    let clean = validate_relative_entry(path_str(rel)?)?;
    let rel_str = path_str(&clean)?;

    if !git(repo_root, &["ls-files", "--", rel_str])?.is_empty() {
        return Err(invalid(format!(
            "hydration path {rel_str:?} contains git-tracked files"
        )));
    }
    let probe = if expect_dir || repo_root.join(&clean).is_dir() {
        format!("{rel_str}/")
    } else {
        rel_str.to_string()
    };
    let out = git_raw(
        repo_root,
        &[
            "-c",
            "core.excludesfile=/dev/null",
            "check-ignore",
            "--no-index",
            "--verbose",
            "--",
            &probe,
        ],
    )?;
    if !out.success() {
        return Err(invalid(format!(
            "hydration path {rel_str:?} is not ignored by a tracked .gitignore"
        )));
    }
    let source = parse_check_ignore_source(&out.stdout).ok_or_else(|| {
        invalid(format!(
            "could not determine gitignore source for {rel_str:?}"
        ))
    })?;
    if !gitignore_source_is_valid(source) {
        return Err(invalid(format!(
            "hydration path {rel_str:?} must be ignored by a repo .gitignore (matched {source:?})"
        )));
    }
    if !git_raw(repo_root, &["ls-files", "--error-unmatch", "--", source])?.success() {
        return Err(invalid(format!(
            "gitignore source {source:?} for {rel_str:?} is not tracked in git"
        )));
    }
    Ok(())
}

/// Validates that a `git check-ignore --verbose` source path is a repo-relative `.gitignore`
/// (rejecting absolute paths, `info/exclude`, `.git` components, and non-`.gitignore` names).
#[doc(hidden)]
pub fn gitignore_source_is_valid(source: &str) -> bool {
    let source_path = Path::new(source);
    !source_path.is_absolute()
        && !source.ends_with("/info/exclude")
        && source != "info/exclude"
        && source_path.file_name() == Some(OsStr::new(".gitignore"))
        && source_path.components().all(|c| {
            matches!(
                c,
                Component::Normal(n)
                    if n.to_str().is_some_and(|s| !s.eq_ignore_ascii_case(".git"))
            )
        })
}

pub(in crate::git::hydration) fn parse_check_ignore_source(stdout: &str) -> Option<&str> {
    let line = stdout.lines().find(|l| !l.trim().is_empty())?;
    let (meta, _) = line.split_once('\t')?;
    let (source, rest) = meta.split_once(':')?;
    let (linenum, _) = rest.split_once(':')?;
    let digits = !linenum.is_empty() && linenum.bytes().all(|b| b.is_ascii_digit());
    (!source.is_empty() && digits).then_some(source)
}

/// Walks every component of `rel` under `root` via `symlink_metadata`,
/// rejecting any symlink along the path, and verifies that the canonicalized
/// target resides strictly inside `canonicalize(root)`.
pub fn assert_no_escaping_symlink(root: &Path, rel: &Path) -> Result<PathBuf, GitError> {
    let clean = validate_relative_entry(path_str(rel)?)?;
    let canon_root = fs::canonicalize(root)?;
    let mut cur = canon_root.clone();
    for comp in clean.components() {
        let Component::Normal(name) = comp else {
            return Err(invalid(format!(
                "hydration path must contain only normal components: {}",
                rel.display()
            )));
        };
        cur.push(name);
        let meta = fs::symlink_metadata(&cur)
            .map_err(|err| invalid(format!("hydration source missing {}: {err}", cur.display())))?;
        if meta.file_type().is_symlink() {
            return Err(invalid(format!(
                "hydration path contains symlink component: {}",
                cur.display()
            )));
        }
    }
    let canon_src = fs::canonicalize(&cur)?;
    if !canon_src.starts_with(&canon_root) || canon_src == canon_root {
        return Err(invalid(format!(
            "hydration path escapes repository root: {}",
            rel.display()
        )));
    }
    Ok(canon_src)
}

pub(super) fn assert_no_escaping_wt_prefix(wt_root: &Path, rel: &Path) -> Result<(), GitError> {
    let canon_wt = fs::canonicalize(wt_root)?;
    let mut cur = canon_wt.clone();
    for comp in rel.components() {
        let Component::Normal(name) = comp else {
            return Err(invalid(format!(
                "invalid worktree destination component: {}",
                rel.display()
            )));
        };
        cur.push(name);
        match fs::symlink_metadata(&cur) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(invalid(format!(
                    "worktree destination contains symlink component: {}",
                    cur.display()
                )));
            }
            Ok(_) => {
                if !fs::canonicalize(&cur)?.starts_with(&canon_wt) {
                    return Err(invalid(format!(
                        "worktree destination escapes worktree root: {}",
                        cur.display()
                    )));
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => break,
            Err(err) => return Err(GitError::Io(err)),
        }
    }
    Ok(())
}

/// Validates and resolves a single hydration entry against `repo_root` and `wt_root`.
pub(super) fn resolve_entry(
    repo_root: &Path,
    wt_root: &Path,
    entry: &str,
    expect_dir: bool,
    label: &str,
) -> Result<(PathBuf, PathBuf), GitError> {
    let rel = validate_relative_entry(entry)?;
    assert_tracked_gitignore(repo_root, &rel)?;
    let src = assert_no_escaping_symlink(repo_root, &rel)?;
    let kind_ok = if expect_dir {
        src.is_dir()
    } else {
        src.is_file()
    };
    if !kind_ok {
        let expected = if expect_dir {
            "a directory"
        } else {
            "a regular file"
        };
        return Err(invalid(format!(
            "{label} entry {entry:?} is not {expected}: {}",
            src.display()
        )));
    }
    assert_no_escaping_wt_prefix(wt_root, &rel)?;
    Ok((src, wt_root.join(&rel)))
}

/// Reads `[worktree]` strictly from `git show <base_sha>:.autofactory/config.toml`.
///
/// Returns `WorktreeConfig::default()` if `.autofactory/config.toml` does not
/// exist at `base_sha`. Fails closed on invalid `base_sha`, non-blob entries,
/// git errors, or invalid TOML.
pub fn load_base_worktree_config(
    repo_root: &Path,
    base_sha: &str,
) -> Result<WorktreeConfig, GitError> {
    let sha = base_sha.trim();
    if sha.is_empty()
        || sha.starts_with('-')
        || sha.contains(':')
        || sha.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(invalid(format!("invalid base_sha {base_sha:?}")));
    }
    let listed = git_raw(repo_root, &["ls-tree", sha, "--", REPO_CONFIG_REL])?;
    if !listed.success() {
        return Err(cmd::failure(
            &["ls-tree", sha, "--", REPO_CONFIG_REL],
            &listed,
        ));
    }
    let line = listed.stdout.trim();
    if line.is_empty() {
        return Ok(WorktreeConfig::default());
    }
    let mode = line.split_whitespace().next().unwrap_or("");
    if !matches!(mode, "100644" | "100755") {
        return Err(invalid(format!(
            "{REPO_CONFIG_REL} at {sha} must be a regular file blob (got mode {mode:?})"
        )));
    }
    let spec = format!("{sha}:{REPO_CONFIG_REL}");
    let shown = git_raw(repo_root, &["show", &spec])?;
    if !shown.success() {
        return Err(cmd::failure(&["show", &spec], &shown));
    }
    let repo_cfg: RepoConfig = toml::from_str(&shown.stdout)
        .map_err(|err| invalid(format!("invalid TOML in {spec}: {}", err.message())))?;
    Ok(repo_cfg.worktree)
}

/// Hydrates a freshly created task worktree for agent turns:
/// - `clone_dirs`: CoW-cloned (never hardlinked) and added to `write_denied`.
/// - `copy_files`: non-secret gitignored files copied and added to `write_denied`.
/// - `setup_cmd`: executed once on a clean worktree only.
///
/// `gate_copy_files` are intentionally NOT staged here; call [`stage_for_gate`]
/// on the host only while running `aaf gate`.
pub fn hydrate_for_agent(
    repo_root: &Path,
    wt_root: &Path,
    cfg: &WorktreeConfig,
) -> Result<HydratedPaths, GitError> {
    let data_dir = aaf_fp::data_dir().ok();
    hydrate_for_agent_inner(
        repo_root,
        wt_root,
        cfg,
        None,
        None,
        data_dir.as_deref(),
        false,
        &seed::RealSeedBackend,
    )
}

#[allow(clippy::too_many_arguments)]
fn hydrate_for_agent_inner(
    repo_root: &Path,
    wt_root: &Path,
    cfg: &WorktreeConfig,
    snap_fallback: Option<(&Path, &str)>,
    base_sha: Option<&str>,
    data_dir: Option<&Path>,
    seed_enabled: bool,
    seed_backend: &dyn seed::SeedBackend,
) -> Result<HydratedPaths, GitError> {
    let mut write_denied = Vec::new();
    if !cfg.is_empty() {
        for entry in &cfg.clone_dirs {
            let rel = validate_relative_entry(entry)?;
            assert_tracked_gitignore_inner(repo_root, &rel, true)?;
            assert_no_escaping_wt_prefix(wt_root, &rel)?;
            let dst = wt_root.join(&rel);
            let main_missing = matches!(
                fs::symlink_metadata(repo_root.join(&rel)),
                Err(err) if err.kind() == io::ErrorKind::NotFound
            );
            if main_missing && let Some((snaps_dir, snap_sha)) = snap_fallback {
                if fs::symlink_metadata(&dst).is_err() {
                    let rel_s = path_str(&rel)?;
                    let restored = super::warm_slot::restore_clean_base_snapshot(
                        wt_root,
                        snaps_dir,
                        snap_sha,
                        &[rel_s],
                    )?;
                    if restored == 0 {
                        let _ = resolve_entry(repo_root, wt_root, entry, true, "clone_dirs")?;
                    }
                }
                write_denied.push(dst);
                continue;
            }
            let (src, dst) = resolve_entry(repo_root, wt_root, entry, true, "clone_dirs")?;
            if fs::symlink_metadata(&dst).is_err() {
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }
                cow_clone_dir(&src, &dst)?;
            }
            write_denied.push(dst);
        }
        for entry in &cfg.copy_files {
            let secret_err = || {
                invalid(format!(
                    "refusing to copy secret file {entry:?} into agent worktree; use [worktree].gate_copy_files"
                ))
            };
            if is_secret_file(entry) {
                return Err(secret_err());
            }
            let rel = validate_relative_entry(entry)?;
            if is_secret_file(path_str(&rel)?) {
                return Err(secret_err());
            }
            let (src, dst) = resolve_entry(repo_root, wt_root, entry, false, "copy_files")?;
            if fs::symlink_metadata(&dst).is_err() {
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(&src, &dst)?;
            }
            write_denied.push(dst);
        }
        let setup_cmd = cfg.setup_cmd.trim();
        if !setup_cmd.is_empty() {
            if !git(wt_root, &["status", "--porcelain"])?.is_empty() {
                return Err(GitError::Dirty(wt_root.to_path_buf()));
            }
            run_setup_cmd(wt_root, setup_cmd, data_dir)?;
        }
    }
    if let Some(sha) = base_sha
        && seed_enabled
    {
        seed::run_seed_during_hydrate(repo_root, wt_root, cfg, sha, seed_backend)?;
    }
    Ok(HydratedPaths { write_denied })
}

/// Loads `[worktree]` from `base_sha` and hydrates a freshly created worktree,
/// falling back to [`super::warm_slot::base_snapshots_dir`] when `clone_dirs`
/// are absent from `repo_root`, and removing the worktree and its branch on error.
pub fn hydrate_fresh_worktree_with_snaps(
    repo_root: &Path,
    worktree: &TaskWorktree,
    base_sha: &str,
    data_dir: &Path,
    common_id: &str,
) -> Result<HydratedPaths, GitError> {
    hydrate_fresh_worktree_with_seed_options(
        repo_root,
        worktree,
        base_sha,
        Some((data_dir, common_id)),
        seed::seed_cache_enabled(),
        &seed::RealSeedBackend,
    )
}

/// Testable seam for hydrating a fresh worktree with explicit seed toggle and backend.
#[doc(hidden)]
pub fn hydrate_fresh_worktree_with_seed_options(
    repo_root: &Path,
    worktree: &TaskWorktree,
    base_sha: &str,
    snaps: Option<(&Path, &str)>,
    seed_enabled: bool,
    seed_backend: &dyn seed::SeedBackend,
) -> Result<HydratedPaths, GitError> {
    let default_data_dir = snaps.is_none().then(|| aaf_fp::data_dir().ok()).flatten();
    let result = load_base_worktree_config(repo_root, base_sha).and_then(|cfg| {
        let rev = format!("{base_sha}^{{commit}}");
        let sha = git(repo_root, &["rev-parse", "--verify", &rev])
            .unwrap_or_else(|_| base_sha.to_string());
        let snaps_dir = snaps.map(|(d, id)| super::warm_slot::base_snapshots_dir(d, id));
        let snap_ref = snaps_dir.as_deref().map(|d| (d, sha.as_str()));
        let data_dir = snaps.map(|(d, _)| d).or(default_data_dir.as_deref());
        hydrate_for_agent_inner(
            repo_root,
            &worktree.path,
            &cfg,
            snap_ref,
            Some(sha.as_str()),
            data_dir,
            seed_enabled,
            seed_backend,
        )
    });
    if result.is_err()
        && let Err(rm_err) = worktree::remove(repo_root, worktree, true, true)
    {
        eprintln!(
            "aaf: warning: failed to remove worktree {} after hydration error: {rm_err}",
            worktree.path.display()
        );
    }
    result
}

/// Loads `[worktree]` from `base_sha` and hydrates a freshly created worktree,
/// removing the worktree and its branch if hydration fails.
pub fn hydrate_fresh_worktree(
    repo_root: &Path,
    worktree: &TaskWorktree,
    base_sha: &str,
) -> Result<HydratedPaths, GitError> {
    if let Some(data_dir) = std::env::var_os("AAF_DATA_DIR").filter(|v| !v.is_empty()) {
        let common_id = stats_repo_id(repo_root);
        return hydrate_fresh_worktree_with_snaps(
            repo_root,
            worktree,
            base_sha,
            Path::new(&data_dir),
            &common_id,
        );
    }
    hydrate_fresh_worktree_with_seed_options(
        repo_root,
        worktree,
        base_sha,
        None,
        seed::seed_cache_enabled(),
        &seed::RealSeedBackend,
    )
}

/// Resolves all `clone_dirs` + `copy_files` destinations inside `worktree` from
/// the immutable base commit so sandboxed agent turns cannot mutate them.
pub fn worktree_write_denied_for_cwd(worktree: &Path) -> Result<Vec<PathBuf>, GitError> {
    let top = git_raw(worktree, &["rev-parse", "--show-toplevel"])?;
    let raw_top = top.stdout.trim();
    if !top.success() || raw_top.is_empty() {
        return Ok(Vec::new());
    }
    let wt_root = fs::canonicalize(raw_top).unwrap_or_else(|_| PathBuf::from(raw_top));
    let Some(base_sha) = resolve_base_sha_for_worktree(&wt_root)? else {
        return Ok(Vec::new());
    };
    let cfg = load_base_worktree_config(&wt_root, &base_sha)?;
    let mut denied = Vec::new();
    for entry in cfg.clone_dirs.iter().chain(&cfg.copy_files) {
        let full = wt_root.join(validate_relative_entry(entry)?);
        if !denied.contains(&full) {
            denied.push(full);
        }
    }
    Ok(denied)
}

/// UTF-8 string variant of [`worktree_write_denied_for_cwd`] for sandbox profiles.
pub fn worktree_write_denied_strings(worktree: &Path) -> Result<Vec<String>, GitError> {
    worktree_write_denied_for_cwd(worktree)?
        .into_iter()
        .map(|p| path_str(&p).map(str::to_string))
        .collect()
}

fn resolve_base_sha_for_worktree(wt_root: &Path) -> Result<Option<String>, GitError> {
    let head_out = git_raw(
        wt_root,
        &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
    )?;
    let head = head_out.stdout.trim().to_string();
    if !head_out.success() || head.is_empty() {
        return Ok(None);
    }
    let mut candidates = Vec::new();
    let origin_head = git_raw(
        wt_root,
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
    )?;
    let sym = origin_head.stdout.trim().to_string();
    if origin_head.success() && origin_head_branch(&sym).is_some() {
        candidates.push(sym);
    }
    candidates.extend(AUTO_FALLBACKS.map(String::from));
    for target in candidates {
        let spec = format!("{target}^{{commit}}");
        let rev = git_raw(wt_root, &["rev-parse", "--verify", "--quiet", &spec])?;
        if !rev.success() {
            continue;
        }
        let mb = git_raw(wt_root, &["merge-base", rev.stdout.trim(), &head])?;
        if mb.success() && !mb.stdout.trim().is_empty() {
            return Ok(Some(mb.stdout.trim().to_string()));
        }
    }
    if let Ok(entries) = worktree::list(wt_root)
        && let Some(main_wt) = entries.first()
        && let Some(main_head) = &main_wt.head
        && fs::canonicalize(&main_wt.path).ok().as_deref() != Some(wt_root)
    {
        let mb = git_raw(wt_root, &["merge-base", main_head, &head])?;
        if mb.success() && !mb.stdout.trim().is_empty() {
            return Ok(Some(mb.stdout.trim().to_string()));
        }
    }
    Ok(Some(head))
}
