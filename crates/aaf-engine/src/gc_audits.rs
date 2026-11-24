//! Protected garbage collection for signed post-gate audit and partial dual-review
//! cache files under `data_dir/audits/<wt_id>/` (R3-PROTO-6).

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use aaf_domain::ports::TaskStore;
use aaf_domain::task::Task;

use crate::db::Db;
use crate::git::worktree::{self, WorktreeEntry};

/// Summary of an expired-audit sweep under `data_dir/audits/`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcAuditStats {
    pub removed_files: Vec<String>,
    pub warnings: Vec<String>,
    pub skipped_locked: bool,
}

fn insert_path_id(ids: &mut HashSet<String>, path: &Path) {
    ids.insert(aaf_fp::repo_id(path));
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
        && let Ok(canon) = fs::canonicalize(parent)
    {
        ids.insert(aaf_fp::repo_id(&canon.join(name)));
    }
}

fn sorted_real_entries(dir: &Path) -> Vec<PathBuf> {
    if !fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink()) {
        return Vec::new();
    }
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|entry| entry.path()))
        .collect();
    paths.sort();
    paths
}

fn repo_worktrees<'a>(
    cache: &'a mut HashMap<PathBuf, Vec<WorktreeEntry>>,
    repo: &Path,
) -> &'a [WorktreeEntry] {
    let key = fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
    cache
        .entry(key)
        .or_insert_with(|| worktree::list(repo).unwrap_or_default())
}

/// Collects protected `<wt_id>` / `<repo_id>` directory names for every
/// non-terminal task and every busy repository in `skipped_busy_repos`.
pub fn collect_protected_audit_ids_for_tasks(
    tasks: &[Task],
    skipped_busy_repos: &HashSet<String>,
    worktrees_dir: &Path,
) -> HashSet<String> {
    let mut protected = HashSet::new();
    let mut wt_cache: HashMap<PathBuf, Vec<WorktreeEntry>> = HashMap::new();
    for busy in skipped_busy_repos.iter().filter(|s| !s.trim().is_empty()) {
        protected.insert(busy.clone());
        let repo = Path::new(busy);
        insert_path_id(&mut protected, repo);
        for wt in sorted_real_entries(&worktrees_dir.join(aaf_fp::repo_id(repo))) {
            insert_path_id(&mut protected, &wt);
        }
        for entry in repo_worktrees(&mut wt_cache, repo) {
            insert_path_id(&mut protected, &entry.path);
        }
    }
    for task in tasks
        .iter()
        .filter(|t| !t.state.is_terminal() && !t.repo_root.trim().is_empty())
    {
        let repo = Path::new(&task.repo_root);
        insert_path_id(&mut protected, repo);
        if let Ok(wt) = worktree::worktree_path(repo, worktrees_dir, &task.id) {
            insert_path_id(&mut protected, &wt);
        }
        for entry in repo_worktrees(&mut wt_cache, repo)
            .iter()
            .filter(|e| e.task_id() == Some(task.id.as_str()))
        {
            insert_path_id(&mut protected, &entry.path);
        }
    }
    protected
}

/// Loads tasks from an optional read-only [`Db`] handle and collects protected
/// audit directory ids using default `data_dir/worktrees`.
pub fn collect_protected_audit_ids(
    db: Option<&Db>,
    skipped_busy_repos: &HashSet<String>,
    data_dir: &Path,
) -> HashSet<String> {
    let tasks = db
        .and_then(|d| d.list_tasks(None, None).ok())
        .unwrap_or_default();
    collect_protected_audit_ids_for_tasks(&tasks, skipped_busy_repos, &data_dir.join("worktrees"))
}

/// Returns `true` if any `data_dir/locks/*.run.lock` file is currently locked.
pub fn any_run_lock_held(data_dir: &Path) -> bool {
    for path in sorted_real_entries(&data_dir.join("locks")) {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let real =
            fs::symlink_metadata(&path).is_ok_and(|m| m.is_file() && !m.file_type().is_symlink());
        if !name.ends_with(".run.lock") || !real {
            continue;
        }
        if let Ok(file) = File::open(&path) {
            match file.try_lock_shared() {
                Ok(()) => {
                    let _ = file.unlock();
                }
                Err(TryLockError::WouldBlock) => return true,
                Err(TryLockError::Error(_)) => {}
            }
        }
    }
    false
}

/// Sweeps expired `.partial.json` and `.audit.json` files under `data_dir/audits/<wt_id>/`
/// while protecting active/busy dirs and skipping when `older_than_days == 0` with active locks.
pub fn clean_expired_audits(
    data_dir: &Path,
    cutoff: SystemTime,
    older_than_days: u64,
    protected_wt_ids: &HashSet<String>,
    dry_run: bool,
) -> GcAuditStats {
    let mut stats = GcAuditStats::default();
    if older_than_days == 0 && any_run_lock_held(data_dir) {
        stats.skipped_locked = true;
        return stats;
    }
    for wt_dir in sorted_real_entries(&data_dir.join("audits")) {
        let Some(wt_id) = wt_dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if protected_wt_ids.contains(wt_id) {
            continue;
        }
        let mut removed_any = false;
        for file_path in sorted_real_entries(&wt_dir) {
            let name = file_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let is_audit = name.ends_with(".partial.json") || name.ends_with(".audit.json");
            let expired = is_audit
                && fs::symlink_metadata(&file_path).is_ok_and(|m| {
                    m.is_file()
                        && !m.file_type().is_symlink()
                        && m.modified().ok().is_some_and(|t| t <= cutoff)
                });
            if !expired {
                continue;
            }
            if !dry_run && let Err(err) = fs::remove_file(&file_path) {
                stats
                    .warnings
                    .push(format!("{}: {err}", file_path.display()));
                continue;
            }
            stats.removed_files.push(file_path.display().to_string());
            removed_any = true;
        }
        if !dry_run && removed_any {
            let _ = fs::remove_dir(&wt_dir);
        }
    }
    stats
}
