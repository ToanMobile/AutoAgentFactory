//! Fixed-path base-only CoW warm worktree slots (`slot-0`, `base-snaps/`,
//! `ephemeral-*`) for `aaf merge` and the red->green oracle.
//! `slot-0.lock` is never unlinked; `Drop` wipes `slot-0` via `reset --hard` + `clean -ffdx`.

use std::ffi::OsStr;
use std::fs::{self, File, TryLockError};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use super::cmd::{git, git_raw};
pub(crate) use super::warm_slot_ops::sweep_expired;
use super::warm_slot_ops::{
    MAX_WALK_DEPTH, append_lock_ext, cow_copy, has_symlink_prefix, is_real_dir,
    path_contains_symlink, read_dir_paths, remove_path_any, unlock_and_remove,
};
use super::{GitError, path_str};
use crate::db::private;

const MAX_BASE_SNAPS: usize = 2;
/// Hooks never run in a slot: a repo-relative `core.hooksPath` resolves to
/// files of the commit checked out there (a PR head, an agent's HEAD), and a
/// `post-checkout` / `reference-transaction` hook would run with our trust.
const NO_HOOKS: [&str; 2] = ["-c", "core.hooksPath=/dev/null"];
static EPHEMERAL_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotKind {
    Merge,
    Oracle,
}

impl SlotKind {
    pub fn dir_name(self) -> &'static str {
        match self {
            SlotKind::Merge => "merge",
            SlotKind::Oracle => "oracle",
        }
    }
}

fn paths_match(a: &Path, b: &Path) -> bool {
    a == b || matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(x), Ok(y)) if x == y)
}

/// `AAF_WARM_SLOTS_DIR` if non-empty and absolute; else
/// `$HOME/.cache/autofactory/warm-slots` for default `$HOME/.aaf` or
/// `$HOME/.local/share/autofactory`; else `<data_dir>/warm-slots`.
pub fn warm_slots_root(data_dir: &Path) -> PathBuf {
    if let Some(v) = std::env::var_os("AAF_WARM_SLOTS_DIR")
        .filter(|v| !v.is_empty() && Path::new(v).is_absolute())
    {
        return PathBuf::from(v);
    }
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        let home = PathBuf::from(home);
        if paths_match(data_dir, &home.join(".aaf"))
            || paths_match(data_dir, &home.join(".local/share/autofactory"))
        {
            return home.join(".cache/autofactory/warm-slots");
        }
    }
    data_dir.join("warm-slots")
}

/// Exclusive handle to a detached worktree (`slot-0` or contended `ephemeral-*`).
pub struct WarmSlot {
    repo_root: PathBuf,
    path: PathBuf,
    lock_path: PathBuf,
    lock_file: Option<File>,
    persistent: bool,
}

impl WarmSlot {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_persistent(&self) -> bool {
        self.persistent
    }

    pub fn acquire(
        repo_root: &Path,
        data_dir: &Path,
        common_id: &str,
        kind: SlotKind,
        commit_sha: &str,
        ephemeral_tag: &str,
    ) -> Result<WarmSlot, GitError> {
        if !is_safe_segment(common_id) || !is_valid_sha(commit_sha) {
            return Err(GitError::Invalid(format!(
                "invalid warm-slot id {common_id:?} or sha {commit_sha:?}"
            )));
        }
        let slot_parent = warm_slots_root(data_dir)
            .join(kind.dir_name())
            .join(common_id);
        private::create_dir(&slot_parent)?;
        let slot0_lock = slot_parent.join("slot-0.lock");
        let lock_file = private::open_file(&slot0_lock)?;
        match lock_file.try_lock() {
            Ok(()) => {
                let _ = lock_file.set_modified(SystemTime::now());
                let slot_dir = slot_parent.join("slot-0");
                if let Err(err) = prepare_persistent_slot(repo_root, &slot_dir, commit_sha) {
                    let _ = lock_file.unlock();
                    return Err(err);
                }
                let snaps = slot_parent.join("base-snaps");
                if is_real_dir(&snaps)
                    && let Some(snap) = sorted_snapshot_dirs(&snaps, Some(&snaps.join(commit_sha)))
                        .into_iter()
                        .find(|p| !path_contains_symlink(p, 0))
                {
                    restore_snapshot_dir(&slot_dir, &snap, Path::new(""), 0);
                }
                Ok(WarmSlot {
                    repo_root: repo_root.to_path_buf(),
                    path: fs::canonicalize(&slot_dir).unwrap_or(slot_dir),
                    lock_path: slot0_lock,
                    lock_file: Some(lock_file),
                    persistent: true,
                })
            }
            Err(TryLockError::WouldBlock) => {
                drop(lock_file);
                acquire_ephemeral(repo_root, &slot_parent, commit_sha, ephemeral_tag)
            }
            Err(TryLockError::Error(err)) => Err(err.into()),
        }
    }
}

impl Drop for WarmSlot {
    fn drop(&mut self) {
        if self.persistent {
            abort_in_progress_ops(&self.path);
            let _ = git_raw(&self.path, &[NO_HOOKS[0], NO_HOOKS[1], "reset", "--hard"]);
            let _ = git_raw(&self.path, &["clean", "-ffdx"]);
            if let Some(f) = self.lock_file.take() {
                let _ = f.unlock(); // CRITICAL: NEVER unlink `slot-0.lock`.
            }
        } else {
            if let Some(p) = self.path.to_str() {
                let _ = git_raw(&self.repo_root, &["worktree", "remove", "--force", p]);
            }
            remove_path_any(&self.path);
            let _ = git_raw(&self.repo_root, &["worktree", "prune"]);
            if let Some(f) = self.lock_file.take() {
                unlock_and_remove(f, &self.lock_path);
            }
        }
    }
}

fn resolve_worktree_git_dir(slot_dir: &Path) -> Option<PathBuf> {
    let dot_git = slot_dir.join(".git");
    let meta = fs::symlink_metadata(&dot_git).ok()?;
    if meta.file_type().is_symlink() {
        return None;
    }
    if meta.is_dir() {
        return Some(dot_git);
    }
    if !meta.is_file() || meta.len() > 8192 {
        return None;
    }
    let raw = fs::read_to_string(&dot_git).ok()?;
    let rest = raw.lines().next()?.trim().strip_prefix("gitdir:")?.trim();
    if rest.is_empty() {
        return None;
    }
    let p = Path::new(rest);
    let resolved = if p.is_absolute() {
        p.to_path_buf()
    } else {
        slot_dir.join(p)
    };
    resolved.is_dir().then_some(resolved)
}

fn abort_in_progress_ops(slot_dir: &Path) {
    let git_dir = resolve_worktree_git_dir(slot_dir);
    if git_dir
        .as_ref()
        .is_none_or(|d| d.join("MERGE_HEAD").exists())
    {
        let _ = git_raw(slot_dir, &[NO_HOOKS[0], NO_HOOKS[1], "merge", "--abort"]);
    }
    if git_dir
        .as_ref()
        .is_none_or(|d| d.join("rebase-merge").exists() || d.join("rebase-apply").exists())
    {
        let _ = git_raw(slot_dir, &[NO_HOOKS[0], NO_HOOKS[1], "rebase", "--abort"]);
    }
}

fn acquire_ephemeral(
    repo_root: &Path,
    slot_parent: &Path,
    commit_sha: &str,
    ephemeral_tag: &str,
) -> Result<WarmSlot, GitError> {
    let seq = EPHEMERAL_SEQ.fetch_add(1, Ordering::Relaxed);
    let tag = sanitize_tag(ephemeral_tag);
    let path = slot_parent.join(format!("ephemeral-{tag}-{}-{seq}", std::process::id()));
    let lock_path = append_lock_ext(&path);
    let lock_file = private::open_file(&lock_path)?;
    lock_file.try_lock().map_err(|e| match e {
        TryLockError::WouldBlock => GitError::LockTimeout(lock_path.clone()),
        TryLockError::Error(io) => GitError::Io(io),
    })?;
    let res = path_str(&path).and_then(|s| {
        git(
            repo_root,
            &[
                NO_HOOKS[0],
                NO_HOOKS[1],
                "worktree",
                "add",
                "--detach",
                s,
                commit_sha,
            ],
        )
    });
    if let Err(err) = res {
        unlock_and_remove(lock_file, &lock_path);
        return Err(err);
    }
    Ok(WarmSlot {
        repo_root: repo_root.to_path_buf(),
        path: fs::canonicalize(&path).unwrap_or(path),
        lock_path,
        lock_file: Some(lock_file),
        persistent: false,
    })
}

fn prepare_persistent_slot(
    repo_root: &Path,
    slot_dir: &Path,
    commit_sha: &str,
) -> Result<(), GitError> {
    let common_args = ["rev-parse", "--path-format=absolute", "--git-common-dir"];
    let valid = is_real_dir(slot_dir)
        && slot_dir.join(".git").exists()
        && matches!(
            (git(repo_root, &common_args), git(slot_dir, &common_args)),
            (Ok(a), Ok(b)) if paths_match(Path::new(&a), Path::new(&b))
        );
    if valid {
        abort_in_progress_ops(slot_dir);
        if git(
            slot_dir,
            &[
                NO_HOOKS[0],
                NO_HOOKS[1],
                "checkout",
                "--detach",
                "--force",
                commit_sha,
            ],
        )
        .is_ok()
            && git(slot_dir, &["clean", "-ffdx"]).is_ok()
        {
            return Ok(());
        }
    }
    let slot_s = path_str(slot_dir)?;
    if fs::symlink_metadata(slot_dir).is_ok() {
        let _ = git_raw(repo_root, &["worktree", "remove", "--force", slot_s]);
        remove_path_any(slot_dir);
    }
    let _ = git_raw(repo_root, &["worktree", "prune"]);
    git(
        repo_root,
        &[
            NO_HOOKS[0],
            NO_HOOKS[1],
            "worktree",
            "add",
            "--detach",
            slot_s,
            commit_sha,
        ],
    )?;
    Ok(())
}

struct StagingGuard(Option<PathBuf>);
impl Drop for StagingGuard {
    fn drop(&mut self) {
        if let Some(dir) = self.0.take() {
            remove_path_any(&dir);
        }
    }
}

/// Atomically saves untracked build dirs from a PURE `base_sha` worktree into
/// `<snaps_dir>/<base_sha>` and keeps at most 2 snapshots. Never call on PR trees.
pub fn promote_clean_base_snapshot(
    slot_dir: &Path,
    snaps_dir: &Path,
    base_sha: &str,
    artifact_dirs: &[&str],
) -> Result<(), GitError> {
    if !is_valid_sha(base_sha) {
        return Err(GitError::Invalid(format!("invalid base_sha {base_sha:?}")));
    }
    let validated: Vec<(&str, PathBuf)> = artifact_dirs
        .iter()
        .map(|&r| Ok((r, validate_artifact_rel(r)?)))
        .collect::<Result<_, GitError>>()?;
    let Ok(canon_slot) = fs::canonicalize(slot_dir) else {
        return Ok(());
    };
    private::create_dir(snaps_dir)?;
    let final_dir = snaps_dir.join(base_sha);
    if fs::symlink_metadata(&final_dir).is_err() {
        let seq = EPHEMERAL_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp_dir = snaps_dir.join(format!(".tmp-{base_sha}-{}-{seq}", std::process::id()));
        private::create_dir(&tmp_dir)?;
        let mut guard = StagingGuard(Some(tmp_dir.clone()));
        let mut copied = 0usize;
        for (rel, rel_path) in &validated {
            let src = slot_dir.join(rel_path);
            if fs::symlink_metadata(&src).is_err()
                || has_symlink_prefix(slot_dir, rel_path)
                || !fs::canonicalize(&src).is_ok_and(|c| c.starts_with(&canon_slot))
                || path_contains_symlink(&src, 0)
                || git_tracks_path(slot_dir, rel)
            {
                continue;
            }
            let dst = tmp_dir.join(rel_path);
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            cow_copy(&src, &dst)?;
            copied += 1;
        }
        if copied == 0 {
            return Ok(());
        }
        match fs::rename(&tmp_dir, &final_dir) {
            Ok(()) => guard.0 = None,
            Err(err) if !final_dir.exists() => return Err(err.into()),
            Err(_) => {}
        }
    }
    for old in sorted_snapshot_dirs(snaps_dir, Some(&final_dir))
        .into_iter()
        .skip(MAX_BASE_SNAPS)
    {
        remove_path_any(&old);
    }
    Ok(())
}

fn sorted_snapshot_dirs(snaps_dir: &Path, keep: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs: Vec<(SystemTime, PathBuf)> = read_dir_paths(snaps_dir)
        .into_iter()
        .filter_map(|p| {
            let ok = p
                .file_name()
                .and_then(OsStr::to_str)
                .is_some_and(is_valid_sha);
            let m = ok.then(|| fs::symlink_metadata(&p).ok()).flatten()?;
            (m.is_dir() && !m.file_type().is_symlink())
                .then(|| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), p))
        })
        .collect();
    dirs.sort_by(|a, b| {
        keep.is_some_and(|k| k == b.1)
            .cmp(&keep.is_some_and(|k| k == a.1))
            .then_with(|| b.0.cmp(&a.0))
            .then_with(|| b.1.cmp(&a.1))
    });
    dirs.into_iter().map(|(_, p)| p).collect()
}

fn restore_snapshot_dir(slot_dir: &Path, src_dir: &Path, rel_prefix: &Path, depth: usize) {
    if depth > MAX_WALK_DEPTH {
        return;
    }
    for src in read_dir_paths(src_dir) {
        let Some(name) = src.file_name().filter(|n| *n != ".git") else {
            continue;
        };
        let Ok(sm) = fs::symlink_metadata(&src) else {
            continue;
        };
        let rel = rel_prefix.join(name);
        let Some(rel_s) = rel.to_str().filter(|s| !s.starts_with('-')) else {
            continue;
        };
        if sm.file_type().is_symlink() {
            continue;
        }
        let dst = slot_dir.join(&rel);
        if let Ok(dm) = fs::symlink_metadata(&dst) {
            if sm.is_dir() && dm.is_dir() && !dm.file_type().is_symlink() {
                restore_snapshot_dir(slot_dir, &src, &rel, depth + 1);
            }
        } else if !git_tracks_path(slot_dir, rel_s) {
            let _ = cow_copy(&src, &dst);
        }
    }
}

fn git_tracks_path(slot_dir: &Path, rel: &str) -> bool {
    git_raw(slot_dir, &["ls-files", "--error-unmatch", "--", rel]).map_or(true, |o| o.success())
}

fn is_safe_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')
}

fn is_safe_segment(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.bytes().all(is_safe_byte)
}

fn is_valid_sha(s: &str) -> bool {
    (40..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn sanitize_tag(tag: &str) -> String {
    let out: String = tag
        .bytes()
        .take(48)
        .map(|b| if is_safe_byte(b) { char::from(b) } else { '_' })
        .collect();
    if out.is_empty() {
        "slot".to_string()
    } else {
        out
    }
}

fn validate_artifact_rel(rel: &str) -> Result<PathBuf, GitError> {
    let mut out = PathBuf::new();
    for comp in Path::new(rel).components() {
        match comp {
            Component::Normal(s) if s != ".git" => out.push(s),
            Component::CurDir => {}
            _ => return Err(GitError::Invalid(format!("invalid artifact_dirs {rel:?}"))),
        }
    }
    if out.as_os_str().is_empty() {
        return Err(GitError::Invalid(format!("empty artifact_dirs {rel:?}")));
    }
    Ok(out)
}
