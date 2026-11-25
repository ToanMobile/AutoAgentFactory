//! S1–S4 seed execution, injectable clone backend, and doctor dry-run.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use aaf_domain::worktree_config::WorktreeConfig;

use super::checks::{
    SeedCandidate, SeedKind, SeedTarget, UNITY_EXCLUDED_ENTRIES, assert_tracked_gitignore_timed,
    check_npm_lock_match, check_pods_lock_match, check_unity_editor_not_open,
    check_unity_state_match_timed, detect_seed_candidates_timed, git_blob_at_base_timed,
    overlaps_worktree_config, read_regular_file_opt,
};
use super::git_bytes::remaining_time;
use super::verify::{check_hidden_files_for_target, post_clone_verify};
use crate::git::hydration::cleanup::make_writable_no_symlinks;
use crate::git::hydration::device::{check_no_nested_mount, same_cow_device};
use crate::git::hydration::stage::{
    CloneSkip, cow_clone_dir_strict_with, cow_clone_entries_strict_with, cp_program,
    remove_partial_dst, truncate_stderr_first_line,
};
use crate::git::hydration::{assert_no_escaping_symlink, assert_no_escaping_wt_prefix};
use crate::git::{GitError, cmd};

pub const SEED_TIMEOUT: Duration = Duration::from_secs(60);
const PS_TIMEOUT: Duration = Duration::from_secs(5);

pub trait SeedBackend {
    fn same_cow_device(&self, src: &Path, dst_parent: &Path) -> Result<bool, String>;
    fn clone_dir_strict(&self, src: &Path, dst: &Path, timeout: Duration) -> Result<(), CloneSkip>;
    fn clone_entries_strict(
        &self,
        src_dir: &Path,
        entries: &[OsString],
        dst_dir: &Path,
        timeout: Duration,
    ) -> Result<(), CloneSkip>;
    fn ps_listing(&self, timeout: Duration) -> Result<String, String>;
    fn check_no_nested_mount(
        &self,
        src: &Path,
        sub_entries: Option<&[OsString]>,
        deadline: Instant,
    ) -> Result<(), String> {
        check_no_nested_mount(src, sub_entries, deadline)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct RealSeedBackend;

fn ps_program() -> &'static str {
    if Path::new("/bin/ps").is_file() {
        "/bin/ps"
    } else if Path::new("/usr/bin/ps").is_file() {
        "/usr/bin/ps"
    } else {
        "ps"
    }
}

impl SeedBackend for RealSeedBackend {
    fn same_cow_device(&self, src: &Path, dst_parent: &Path) -> Result<bool, String> {
        same_cow_device(src, dst_parent).map_err(|e| e.to_string())
    }

    fn clone_dir_strict(&self, src: &Path, dst: &Path, timeout: Duration) -> Result<(), CloneSkip> {
        cow_clone_dir_strict_with(src, dst, timeout, Ok(true), cp_program(), None)
    }

    fn clone_entries_strict(
        &self,
        src_dir: &Path,
        entries: &[OsString],
        dst_dir: &Path,
        timeout: Duration,
    ) -> Result<(), CloneSkip> {
        cow_clone_entries_strict_with(
            src_dir,
            entries,
            dst_dir,
            timeout,
            Ok(true),
            cp_program(),
            None,
        )
    }

    fn ps_listing(&self, timeout: Duration) -> Result<String, String> {
        let mut c = Command::new(ps_program());
        c.args(["-A", "-ww", "-o", "pid=,args="]);
        let out = cmd::run(&mut c, timeout.min(PS_TIMEOUT)).map_err(|e| e.to_string())?;
        if !out.success() {
            return Err(format!(
                "ps exited {:?}: {}",
                out.code,
                truncate_stderr_first_line(&out.stderr)
            ));
        }
        Ok(out.stdout)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedEntryReport {
    pub rel_dir: &'static str,
    pub kind: Option<SeedKind>,
    pub seeded: bool,
    pub files: usize,
    pub duration_ms: u128,
    pub skip_reason: Option<String>,
    /// Set whenever a post-creation failure (`created == true`) could not be cleaned up;
    /// escalates hydrate to `Err` (fail-closed) so no unverified tree remains.
    pub fatal_cleanup_error: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SeedReport {
    pub entries: Vec<SeedEntryReport>,
    pub duration_ms: u128,
}

impl SeedReport {
    pub fn summary_line(&self) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let parts: Vec<String> = self
            .entries
            .iter()
            .map(|e| {
                if e.seeded {
                    format!("{} ({} files, {}ms)", e.rel_dir, e.files, e.duration_ms)
                } else {
                    format!(
                        "{} skipped ({})",
                        e.rel_dir,
                        e.skip_reason.as_deref().unwrap_or("unknown")
                    )
                }
            })
            .collect();
        Some(format!(
            "aaf: seed {} [{}ms]",
            parts.join("; "),
            self.duration_ms
        ))
    }

    pub fn fatal_cleanup_error(&self) -> Option<&str> {
        self.entries
            .iter()
            .find_map(|e| e.fatal_cleanup_error.as_deref())
    }
}

/// Dry-run outcome for `aaf doctor` (S7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DryRunItem {
    pub rel_dir: &'static str,
    pub kind: Option<SeedKind>,
    pub would_seed: bool,
    pub reason: Option<String>,
}

/// Invoked by `hydrate_for_agent_inner`: runs seeding, prints summary line, and
/// fails closed (`Err`) if any post-creation cleanup failed (restoring owner write
/// permissions on `dst` first so `worktree::remove` succeeds).
pub(crate) fn run_seed_during_hydrate(
    repo_root: &Path,
    wt_root: &Path,
    cfg: &WorktreeConfig,
    base_sha: &str,
    backend: &dyn SeedBackend,
) -> Result<(), GitError> {
    let report = seed_worktree_with(repo_root, wt_root, cfg, base_sha, SEED_TIMEOUT, backend);
    if let Some(line) = report.summary_line() {
        eprintln!("{line}");
    }
    if let Some(fatal) = report.fatal_cleanup_error() {
        for entry in &report.entries {
            if entry.fatal_cleanup_error.is_some() {
                make_writable_no_symlinks(&wt_root.join(entry.rel_dir), 0, None);
            }
        }
        return Err(GitError::Invalid(format!(
            "seed post-clone cleanup failed: {fatal}"
        )));
    }
    Ok(())
}

/// Seeds a fresh task worktree using `RealSeedBackend` and `SEED_TIMEOUT`.
pub fn seed_worktree(
    repo_root: &Path,
    wt_root: &Path,
    cfg: &WorktreeConfig,
    base_sha: &str,
) -> SeedReport {
    seed_worktree_with(
        repo_root,
        wt_root,
        cfg,
        base_sha,
        SEED_TIMEOUT,
        &RealSeedBackend,
    )
}

/// Testable core of [`seed_worktree`] with injectable backend and timeout.
pub fn seed_worktree_with(
    repo_root: &Path,
    wt_root: &Path,
    cfg: &WorktreeConfig,
    base_sha: &str,
    timeout: Duration,
    backend: &dyn SeedBackend,
) -> SeedReport {
    let started = Instant::now();
    let deadline = started + timeout;
    let candidates = match detect_seed_candidates_timed(repo_root, base_sha, deadline) {
        Ok(c) => c,
        Err(reason) => {
            return SeedReport {
                entries: vec![SeedEntryReport {
                    rel_dir: ".",
                    kind: None,
                    seeded: false,
                    files: 0,
                    duration_ms: started.elapsed().as_millis(),
                    skip_reason: Some(reason),
                    fatal_cleanup_error: None,
                }],
                duration_ms: started.elapsed().as_millis(),
            };
        }
    };

    let mut entries = Vec::with_capacity(candidates.len());
    for cand in candidates {
        match cand {
            SeedCandidate::Skipped { rel_dir, reason } => entries.push(SeedEntryReport {
                rel_dir,
                kind: None,
                seeded: false,
                files: 0,
                duration_ms: 0,
                skip_reason: Some(reason),
                fatal_cleanup_error: None,
            }),
            SeedCandidate::Target(target) => {
                entries.push(seed_one_target(
                    repo_root, wt_root, cfg, base_sha, &target, deadline, backend,
                ));
            }
        }
    }

    // S2.7b (Finding #7): run `aaf_fp::hidden_files` ONCE per seed pass across all cloned targets.
    let seeded_indices: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter_map(|(idx, e)| e.seeded.then_some(idx))
        .collect();
    if !seeded_indices.is_empty() {
        let hidden_res = remaining_time(deadline).and_then(|rem| {
            aaf_fp::hidden_files(wt_root, rem).map_err(|e| format!("hidden_scan_failed: {e}"))
        });
        for idx in seeded_indices {
            let check = match &hidden_res {
                Ok(hidden) => check_hidden_files_for_target(hidden, entries[idx].rel_dir),
                Err(err) => Err(err.clone()),
            };
            if let Err(reason) = check {
                let dst = wt_root.join(entries[idx].rel_dir);
                entries[idx].seeded = false;
                entries[idx].files = 0;
                match remove_partial_dst(&dst) {
                    Ok(()) => entries[idx].skip_reason = Some(reason),
                    Err(cleanup_err) => {
                        let msg = format!("cleanup_failed: {cleanup_err} (after {reason})");
                        entries[idx].skip_reason = Some(msg.clone());
                        entries[idx].fatal_cleanup_error = Some(msg);
                    }
                }
            }
        }
    }

    SeedReport {
        entries,
        duration_ms: started.elapsed().as_millis(),
    }
}

/// Runs S1–S2 pre-clone checks for `aaf doctor` against `HEAD` without cloning or writing.
pub fn dry_run_seed(
    repo_root: &Path,
    cfg: &WorktreeConfig,
    worktrees_dir: &Path,
) -> Result<Vec<DryRunItem>, String> {
    dry_run_seed_with(repo_root, cfg, worktrees_dir, "HEAD", &RealSeedBackend)
}

/// Testable core of [`dry_run_seed`].
pub fn dry_run_seed_with(
    repo_root: &Path,
    cfg: &WorktreeConfig,
    worktrees_dir: &Path,
    base_ref: &str,
    backend: &dyn SeedBackend,
) -> Result<Vec<DryRunItem>, String> {
    let resolved_sha = cmd::git(
        repo_root,
        &["rev-parse", "--verify", &format!("{base_ref}^{{commit}}")],
    )
    .map_err(|e| format!("no commit at {base_ref}: {e}"))?;
    let deadline = Instant::now() + SEED_TIMEOUT;
    let candidates = detect_seed_candidates_timed(repo_root, &resolved_sha, deadline)?;
    let mut items = Vec::with_capacity(candidates.len());
    for cand in candidates {
        match cand {
            SeedCandidate::Skipped { rel_dir, reason } => items.push(DryRunItem {
                rel_dir,
                kind: None,
                would_seed: false,
                reason: Some(reason),
            }),
            SeedCandidate::Target(target) => {
                let res = precheck_target(
                    repo_root,
                    None,
                    worktrees_dir,
                    cfg,
                    &resolved_sha,
                    &target,
                    deadline,
                    backend,
                );
                match res {
                    Ok(_) => items.push(DryRunItem {
                        rel_dir: target.rel_dir,
                        kind: Some(target.kind),
                        would_seed: true,
                        reason: None,
                    }),
                    Err(reason) => items.push(DryRunItem {
                        rel_dir: target.rel_dir,
                        kind: Some(target.kind),
                        would_seed: false,
                        reason: Some(reason),
                    }),
                }
            }
        }
    }
    Ok(items)
}

struct PrecheckContext {
    src: PathBuf,
    base_lock: Option<Vec<u8>>,
    unity_entries: Vec<OsString>,
}

#[allow(clippy::too_many_arguments)]
fn precheck_target(
    repo_root: &Path,
    wt_root: Option<&Path>,
    dst_device_probe: &Path,
    cfg: &WorktreeConfig,
    base_sha: &str,
    target: &SeedTarget,
    deadline: Instant,
    backend: &dyn SeedBackend,
) -> Result<PrecheckContext, String> {
    let rel = Path::new(target.rel_dir);

    if let Some(reason) = overlaps_worktree_config(rel, cfg) {
        return Err(reason);
    }

    if let Some(wt) = wt_root {
        let dst = wt.join(rel);
        match fs::symlink_metadata(&dst) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => return Err("dst_already_exists".to_string()),
            Err(err) => return Err(format!("dst_stat_error: {err}")),
        }
    }

    let src =
        assert_no_escaping_symlink(repo_root, rel).map_err(|e| format!("source_invalid: {e}"))?;
    if !src.is_dir() {
        return Err("source_not_dir".to_string());
    }
    if let Some(wt) = wt_root {
        assert_no_escaping_wt_prefix(wt, rel).map_err(|e| format!("wt_prefix_invalid: {e}"))?;
    }

    let ignore_root = wt_root.unwrap_or(repo_root);
    assert_tracked_gitignore_timed(ignore_root, rel, deadline)
        .map_err(|e| format!("not_tracked_gitignored: {e}"))?;

    let same_dev = backend
        .same_cow_device(&src, dst_device_probe)
        .map_err(|e| format!("device_check_failed: {e}"))?;
    if !same_dev {
        return Err(if cfg!(target_os = "macos") {
            "cross_device_or_non_apfs".to_string()
        } else {
            "cross_device".to_string()
        });
    }

    let mut base_lock = None;
    let mut unity_entries = Vec::new();
    match target.kind {
        SeedKind::Npm => {
            let l =
                git_blob_at_base_timed(repo_root, base_sha, "package-lock.json", false, deadline)?
                    .ok_or_else(|| "missing_base_package_lock".to_string())?;
            let i = read_regular_file_opt(&src.join(".package-lock.json"))?
                .ok_or_else(|| "missing_installed_package_lock".to_string())?;
            check_npm_lock_match(&l, &i)?;
            base_lock = Some(l);
        }
        SeedKind::CocoaPods { lock_rel } => {
            let l = git_blob_at_base_timed(repo_root, base_sha, lock_rel, false, deadline)?
                .ok_or_else(|| format!("missing_base_podfile_lock: {lock_rel}"))?;
            let i = read_regular_file_opt(&src.join("Manifest.lock"))?
                .ok_or_else(|| "missing_pods_manifest_lock".to_string())?;
            check_pods_lock_match(&l, &i)?;
            base_lock = Some(l);
        }
        SeedKind::Unity => {
            check_unity_state_match_timed(repo_root, base_sha, deadline)?;
            let ps_rem = remaining_time(deadline)?;
            let ps_res = backend.ps_listing(ps_rem);
            check_unity_editor_not_open(repo_root, ps_res.as_deref().map_err(String::as_str))?;
            unity_entries = list_unity_seed_entries(&src)?;
            if unity_entries.is_empty() {
                return Err("unity_library_empty_after_exclude".to_string());
            }
        }
    }

    let sub_entries = matches!(target.kind, SeedKind::Unity).then_some(unity_entries.as_slice());
    backend.check_no_nested_mount(&src, sub_entries, deadline)?;

    Ok(PrecheckContext {
        src,
        base_lock,
        unity_entries,
    })
}

fn list_unity_seed_entries(src_library: &Path) -> Result<Vec<OsString>, String> {
    let mut kept = Vec::new();
    let rd = fs::read_dir(src_library).map_err(|e| format!("read_dir Library: {e}"))?;
    for entry in rd {
        let entry = entry.map_err(|e| format!("read_dir entry: {e}"))?;
        let name = entry.file_name();
        let excluded = name.to_str().is_some_and(|s| {
            UNITY_EXCLUDED_ENTRIES
                .iter()
                .any(|ex| s.eq_ignore_ascii_case(ex))
        });
        if !excluded {
            kept.push(name);
        }
    }
    kept.sort();
    Ok(kept)
}

fn seed_one_target(
    repo_root: &Path,
    wt_root: &Path,
    cfg: &WorktreeConfig,
    base_sha: &str,
    target: &SeedTarget,
    deadline: Instant,
    backend: &dyn SeedBackend,
) -> SeedEntryReport {
    let started = Instant::now();
    let rel = Path::new(target.rel_dir);
    let dst = wt_root.join(rel);
    let dst_parent = dst.parent().unwrap_or(wt_root);

    if let Err(reason) = remaining_time(deadline) {
        return skipped_entry(target, started, reason, None);
    }

    let ctx = match precheck_target(
        repo_root,
        Some(wt_root),
        dst_parent,
        cfg,
        base_sha,
        target,
        deadline,
        backend,
    ) {
        Ok(c) => c,
        Err(reason) => return skipped_entry(target, started, reason, None),
    };

    if let Some(parent) = dst.parent()
        && let Err(err) = fs::create_dir_all(parent)
    {
        return skipped_entry(
            target,
            started,
            format!("create_parent_failed: {err}"),
            None,
        );
    }

    let clone_rem = match remaining_time(deadline) {
        Ok(r) => r,
        Err(reason) => return skipped_entry(target, started, reason, None),
    };

    let clone_res = match target.kind {
        SeedKind::Unity => {
            backend.clone_entries_strict(&ctx.src, &ctx.unity_entries, &dst, clone_rem)
        }
        SeedKind::Npm | SeedKind::CocoaPods { .. } => {
            backend.clone_dir_strict(&ctx.src, &dst, clone_rem)
        }
    };
    if let Err(skip) = clone_res {
        if skip.cleanup_failed {
            let msg = skip.reason;
            return skipped_entry(target, started, msg.clone(), Some(msg));
        }
        if !skip.created {
            // Finding #3: never delete a `dst` that this clone attempt did not create.
            return skipped_entry(target, started, skip.reason, None);
        }
        return match remove_partial_dst(&dst) {
            Ok(()) => skipped_entry(target, started, skip.reason, None),
            Err(cleanup_err) => {
                let msg = format!("cleanup_failed: {cleanup_err} (after {})", skip.reason);
                skipped_entry(target, started, msg.clone(), Some(msg))
            }
        };
    }

    match post_clone_verify(
        repo_root,
        wt_root,
        &dst,
        base_sha,
        target,
        ctx.base_lock.as_deref(),
        deadline,
        backend,
    ) {
        Ok(files) => SeedEntryReport {
            rel_dir: target.rel_dir,
            kind: Some(target.kind.clone()),
            seeded: true,
            files,
            duration_ms: started.elapsed().as_millis(),
            skip_reason: None,
            fatal_cleanup_error: None,
        },
        Err(reason) => match remove_partial_dst(&dst) {
            Ok(()) => skipped_entry(target, started, reason, None),
            Err(cleanup_err) => {
                // Finding #4: ANY post-clone cleanup failure escalates to hydrate Err.
                let msg = format!("cleanup_failed: {cleanup_err} (after {reason})");
                skipped_entry(target, started, msg.clone(), Some(msg))
            }
        },
    }
}

fn skipped_entry(
    target: &SeedTarget,
    started: Instant,
    reason: String,
    fatal_cleanup_error: Option<String>,
) -> SeedEntryReport {
    SeedEntryReport {
        rel_dir: target.rel_dir,
        kind: Some(target.kind.clone()),
        seeded: false,
        files: 0,
        duration_ms: started.elapsed().as_millis(),
        skip_reason: Some(reason),
        fatal_cleanup_error,
    }
}
