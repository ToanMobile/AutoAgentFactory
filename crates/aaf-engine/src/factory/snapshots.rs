//! Pre-turn git snapshots, locked rollback (`undo`), and human task steering (`steer`).

use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{EventSink, RunStore, TaskStore};
use aaf_domain::task::TaskState;
use serde_json::json;

use crate::db::{Db, TurnSnapshot, private};
use crate::executors::reaper::validate_run_id;
use crate::git::GitError;
use crate::git::cmd::git;
use crate::git::worktree::{self, BRANCH_PREFIX, SNAPSHOT_REF_PREFIX};

const MAX_STEER_NOTE_BYTES: usize = 2000;

struct RunLockGuard {
    file: File,
}

impl Drop for RunLockGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn acquire_repo_run_lock(data_dir: &Path, repo_root: &Path) -> Result<RunLockGuard, String> {
    let locks_dir = data_dir.join("locks");
    private::create_dir(&locks_dir).map_err(|e| format!("locks dir: {e}"))?;
    let lock_path = locks_dir.join(format!("{}.run.lock", aaf_fp::repo_id(repo_root)));
    let file = private::open_file(&lock_path)
        .map_err(|e| format!("run lock {}: {e}", lock_path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(RunLockGuard { file }),
        Err(TryLockError::WouldBlock) => {
            Err("repository run lock is held by another aaf process".to_string())
        }
        Err(TryLockError::Error(err)) => Err(format!("run lock {}: {err}", lock_path.display())),
    }
}

/// Records `refs/aaf/snapshots/<task_id>/<ts>-<run_id>` pointing at the
/// worktree's current `HEAD` commit oid.
pub fn create_turn_snapshot_ref(
    repo_root: &Path,
    worktree: &Path,
    task_id: &str,
    run_id: &str,
    now: i64,
) -> Result<(String, String), GitError> {
    worktree::validate_task_id(task_id)?;
    validate_run_id(run_id).map_err(|e| GitError::Invalid(e.to_string()))?;
    let oid = git(worktree, &["rev-parse", "HEAD"])?;
    if oid.is_empty() || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(GitError::Invalid(format!(
            "worktree HEAD resolved to invalid commit oid {oid:?}"
        )));
    }
    let ref_name = format!("{SNAPSHOT_REF_PREFIX}{task_id}/{:020}-{run_id}", now.max(0));
    crate::git::ensure_plain_reflogs_all(&[repo_root, worktree])?;
    git(repo_root, &["update-ref", &ref_name, &oid])?;
    Ok((ref_name, oid))
}

fn validate_snapshot_fields(task_id: &str, snap: &TurnSnapshot) -> Result<(), String> {
    let expected_prefix = format!("{SNAPSHOT_REF_PREFIX}{task_id}/");
    if !snap.ref_name.starts_with(&expected_prefix)
        || snap.ref_name.starts_with('-')
        || snap.ref_name.contains("..")
        || snap.ref_name.chars().any(char::is_whitespace)
    {
        return Err(format!("invalid snapshot ref name {:?}", snap.ref_name));
    }
    if snap.commit_oid.is_empty()
        || snap.commit_oid.starts_with('-')
        || !snap.commit_oid.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(format!("invalid snapshot commit oid {:?}", snap.commit_oid));
    }
    Ok(())
}

/// Validates that `task_id` can be rolled back and returns its latest verified
/// [`TurnSnapshot`], failing closed on forbidden task states or ref/DB mismatch.
pub fn preview_latest_undo(
    db: &Db,
    repo_root: &Path,
    task_id: &str,
) -> Result<TurnSnapshot, String> {
    worktree::validate_task_id(task_id).map_err(|e| e.to_string())?;
    let task = db.get_task(task_id).map_err(|e| e.to_string())?;
    if !matches!(
        task.state,
        TaskState::Ready | TaskState::Review | TaskState::Blocked
    ) {
        return Err(format!(
            "cannot undo task {task_id} in state {} (only ready, review, or blocked allowed)",
            task.state.as_str()
        ));
    }
    let snap = db
        .latest_snapshot(task_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no turn snapshot found for task {task_id}"))?;
    validate_snapshot_fields(task_id, &snap)?;
    let resolved = git(repo_root, &["rev-parse", "--verify", &snap.ref_name]).map_err(|e| {
        format!(
            "snapshot ref {} is missing or unreadable: {e}",
            snap.ref_name
        )
    })?;
    if resolved != snap.commit_oid {
        return Err(format!(
            "snapshot ref {} mismatch: git has {}, db expects {}",
            snap.ref_name, resolved, snap.commit_oid
        ));
    }
    Ok(snap)
}

fn verify_registered_worktree(
    repo_root: &Path,
    worktrees_root: &Path,
    task_id: &str,
) -> Result<PathBuf, String> {
    if let Some((wt, _)) = worktree::existing_registered(repo_root, worktrees_root, task_id)
        .map_err(|e| e.to_string())?
    {
        return Ok(wt.path);
    }
    let wt_path =
        worktree::worktree_path(repo_root, worktrees_root, task_id).map_err(|e| e.to_string())?;
    let expected_branch = format!("{BRANCH_PREFIX}{task_id}");
    if !wt_path.exists() {
        return Err(format!(
            "worktree for task {task_id} not found at {}",
            wt_path.display()
        ));
    }
    Err(format!(
        "{} is not a registered git worktree for branch {expected_branch}",
        wt_path.display()
    ))
}

fn clear_stale_receipts(
    db: &Db,
    data_dir: &Path,
    repo_root: &Path,
    worktrees_root: &Path,
    wt_path: &Path,
    task_id: &str,
) {
    let main_id = aaf_fp::repo_id(repo_root);
    let main_approvals_dir = data_dir.join("approvals").join(&main_id);
    let mut roots = vec![wt_path.to_path_buf()];
    if let Ok(expected) = worktree::worktree_path(repo_root, worktrees_root, task_id) {
        roots.push(expected);
    }
    if let Ok(runs) = db.runs_for_task(task_id) {
        for run in runs {
            if !run.worktree_path.is_empty() {
                roots.push(PathBuf::from(run.worktree_path));
            }
        }
    }
    for root in roots {
        let rid = aaf_fp::repo_id(&root);
        let _ = fs::remove_file(aaf_fp::receipt_path(data_dir, &root));
        let _ = fs::remove_dir_all(data_dir.join("audits").join(&rid));
        if rid == main_id {
            continue;
        }
        let wt_approvals = data_dir.join("approvals").join(&rid);
        if let Ok(entries) = fs::read_dir(&wt_approvals) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().ends_with(".json") {
                    let _ = fs::remove_file(main_approvals_dir.join(name));
                }
            }
        }
        let _ = fs::remove_dir_all(wt_approvals);
    }
}

/// Rolls back `task_id`'s worktree to its latest pre-turn snapshot under the
/// repository's exclusive run lock, cleans untracked files, removes the
/// snapshot ref/row and stale receipts/approvals/audits, moves `Review`/`Blocked`
/// back to `Ready`, and records a `TaskStateChanged` event.
pub fn undo_latest_turn(
    db: &Db,
    data_dir: &Path,
    repo_root: &Path,
    worktrees_root: &Path,
    task_id: &str,
    now: i64,
) -> Result<TurnSnapshot, String> {
    let _lock = acquire_repo_run_lock(data_dir, repo_root)?;
    let snap = preview_latest_undo(db, repo_root, task_id)?;
    let task = db.get_task(task_id).map_err(|e| e.to_string())?;
    let wt_path = verify_registered_worktree(repo_root, worktrees_root, task_id)?;
    crate::git::ensure_plain_reflogs_all(&[repo_root, &wt_path]).map_err(|e| e.to_string())?;
    // SEC-125: a planted HEAD symref would let the reset move another branch.
    let branch = format!("{BRANCH_PREFIX}{task_id}");
    worktree::ensure_on_branch(&wt_path, &branch).map_err(|e| e.to_string())?;
    let branch_ref = format!("refs/heads/{branch}");
    crate::git::pseudo_ref_guard::ensure_plain_branch_ref(&wt_path, &branch_ref)
        .map_err(|e| e.to_string())?;
    crate::git::pseudo_ref_guard::ensure_plain_pseudo_refs(&wt_path).map_err(|e| e.to_string())?;

    git(
        &wt_path,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "reset",
            "--hard",
            &snap.commit_oid,
        ],
    )
    .map_err(|e| format!("git reset --hard {}: {e}", snap.commit_oid))?;
    git(&wt_path, &["clean", "-fd"]).map_err(|e| format!("git clean -fd: {e}"))?;

    git(
        repo_root,
        &["update-ref", "-d", "--no-deref", &snap.ref_name],
    )
    .map_err(|e| format!("delete snapshot ref {}: {e}", snap.ref_name))?;
    db.delete_snapshot(snap.id)
        .map_err(|e| format!("delete snapshot row: {e}"))?;

    clear_stale_receipts(db, data_dir, repo_root, worktrees_root, &wt_path, task_id);

    if task.state != TaskState::Ready {
        db.set_state(task_id, TaskState::Ready, None, now)
            .map_err(|e| format!("set state ready: {e}"))?;
    }
    let payload = json!({
        "to": "ready",
        "why": "undo",
        "commit_oid": snap.commit_oid,
        "ref_name": snap.ref_name,
    });
    let event =
        FactoryEvent::new(EventKind::TaskStateChanged, payload.to_string(), now).for_task(task_id);
    db.emit(&event)
        .map_err(|e| format!("emit undo event: {e}"))?;
    Ok(snap)
}

fn sanitize_steer_note(raw: &str) -> Result<String, String> {
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return Err("steer note cannot be empty".to_string());
    }
    let mut end = trimmed.len().min(MAX_STEER_NOTE_BYTES);
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    let truncated = trimmed[..end].trim_end();
    if truncated.is_empty() {
        return Err("steer note cannot be empty".to_string());
    }
    Ok(truncated.to_string())
}

/// Appends a sanitized `steer: <note>` context note to `task_id` without
/// touching its `ImmutableSpec`.
pub fn steer_task(db: &Db, task_id: &str, note: &str, now: i64) -> Result<String, String> {
    worktree::validate_task_id(task_id).map_err(|e| e.to_string())?;
    let clean = sanitize_steer_note(note)?;
    let formatted = format!("steer: {clean}");
    db.append_context_note(task_id, &formatted, now)
        .map_err(|e| e.to_string())?;
    Ok(formatted)
}
