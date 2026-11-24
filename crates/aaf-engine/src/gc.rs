//! Garbage collection for worktrees, branches, snapshot refs, stale
//! fingerprint temp files, and expired stream/report artifacts, plus verified
//! squash-merge matching for the factory review sweep.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{self, File, TryLockError};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use aaf_domain::ports::{PortError, TaskStore};
use aaf_domain::task::{Task, TaskState};
use serde::{Deserialize, Serialize};

use crate::db::{Db, DbError, private};
use crate::git::cmd::{failure, git, git_raw};
use crate::git::worktree::{self, BRANCH_PREFIX, SNAPSHOT_REF_PREFIX};
use crate::git::{GitError, path_str};
use crate::process::CommandRunner;
use crate::telemetry::is_exclusively_locked;

/// Default retention window for stream `.jsonl` and report `.html` artifacts.
pub const DEFAULT_OLDER_THAN_DAYS: u64 = 14;
const MAX_SOP_CHECKPOINT_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum GcError {
    #[error("database: {0}")]
    Db(#[from] DbError),
    #[error("store: {0}")]
    Port(#[from] PortError),
    #[error("git: {0}")]
    Git(#[from] GitError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcOptions {
    pub repo_root: Option<PathBuf>,
    pub all: bool,
    pub dry_run: bool,
    pub include_cancelled: bool,
    pub force: bool,
    pub older_than_days: u64,
}

impl Default for GcOptions {
    fn default() -> Self {
        Self {
            repo_root: None,
            all: false,
            dry_run: false,
            include_cancelled: false,
            force: false,
            older_than_days: DEFAULT_OLDER_THAN_DAYS,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcReport {
    pub dry_run: bool,
    pub repos_scanned: Vec<String>,
    pub skipped_busy: Vec<String>,
    pub skipped_dirty: Vec<String>,
    pub removed_orphan_batches: Vec<String>,
    pub removed_worktrees: Vec<String>,
    pub removed_branches: Vec<String>,
    pub removed_snapshot_refs: usize,
    pub removed_stale_fp: usize,
    pub removed_files: Vec<String>,
    pub pruned_repos: usize,
    pub warnings: Vec<String>,
}

struct RepoLockGuard {
    file: File,
}

impl Drop for RepoLockGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn canonical_or_owned(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn try_lock_repo(
    data_dir: &Path,
    repo_id: &str,
    dry_run: bool,
) -> Result<Option<RepoLockGuard>, GcError> {
    let locks_dir = data_dir.join("locks");
    let lock_path = locks_dir.join(format!("{repo_id}.run.lock"));
    let (file, lock_res) = if dry_run {
        if !lock_path.exists() {
            return Ok(None);
        }
        let file = File::open(&lock_path)?;
        let res = file.try_lock_shared();
        (file, res)
    } else {
        private::create_dir(&locks_dir)?;
        let file = private::open_file(&lock_path)?;
        let res = file.try_lock();
        (file, res)
    };
    match lock_res {
        Ok(()) => Ok(Some(RepoLockGuard { file })),
        Err(TryLockError::WouldBlock) => Err(GcError::Io(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "repository lock is held",
        ))),
        Err(TryLockError::Error(err)) => Err(GcError::Io(err)),
    }
}

fn collect_db_state(data_dir: &Path) -> Result<(Vec<Task>, HashSet<String>), GcError> {
    let db_path = data_dir.join("aaf.db");
    if !db_path.is_file() {
        return Ok((Vec::new(), HashSet::new()));
    }
    let db = Db::open_read_only(&db_path)?;
    let tasks = db.list_tasks(None, None)?;
    let running_runs = db.running_run_ids()?;
    Ok((tasks, running_runs))
}

fn resolve_target_repos(opts: &GcOptions, tasks: &[Task]) -> Vec<PathBuf> {
    let mut set = BTreeSet::new();
    if let Some(repo) = &opts.repo_root {
        set.insert(canonical_or_owned(repo));
    }
    if opts.all {
        for task in tasks {
            if !task.repo_root.trim().is_empty() {
                set.insert(canonical_or_owned(Path::new(&task.repo_root)));
            }
        }
    }
    set.into_iter().collect()
}

/// Runs garbage collection across the selected repository (or all repositories)
/// and expired stream/report files in `data_dir`.
///
/// Never modifies `receipt.key`, `aaf.db`, `events`, `receipts/`, `approvals/`,
/// or `proofs/`.
pub fn run_gc(
    data_dir: &Path,
    worktrees_dir: &Path,
    opts: &GcOptions,
) -> Result<GcReport, GcError> {
    let mut report = GcReport {
        dry_run: opts.dry_run,
        ..GcReport::default()
    };
    let (all_tasks, running_runs) = collect_db_state(data_dir)?;
    let repos = resolve_target_repos(opts, &all_tasks);

    for repo in repos {
        let repo_label = repo.display().to_string();
        if !repo.is_dir() || aaf_fp::repo_root(&repo).is_err() {
            report
                .warnings
                .push(format!("{repo_label}: skipped (not a git repository)"));
            continue;
        }
        let repo_id = aaf_fp::repo_id(&repo);
        let _lock = match try_lock_repo(data_dir, &repo_id, opts.dry_run) {
            Ok(guard) => guard,
            Err(GcError::Io(err)) if err.kind() == std::io::ErrorKind::WouldBlock => {
                report.skipped_busy.push(repo_label);
                continue;
            }
            Err(err) => return Err(err),
        };
        report.repos_scanned.push(repo_label);
        clean_repo(
            &repo,
            &repo_id,
            worktrees_dir,
            &all_tasks,
            opts,
            &mut report,
        )?;
    }

    clean_expired_artifacts(
        data_dir,
        opts,
        &running_runs,
        SystemTime::now(),
        &mut report,
    );
    Ok(report)
}

fn clean_repo(
    repo: &Path,
    repo_id: &str,
    worktrees_dir: &Path,
    all_tasks: &[Task],
    opts: &GcOptions,
    report: &mut GcReport,
) -> Result<(), GcError> {
    let canonical_repo = canonical_or_owned(repo);
    let repo_tasks: Vec<&Task> = all_tasks
        .iter()
        .filter(|t| canonical_or_owned(Path::new(&t.repo_root)) == canonical_repo)
        .collect();
    let active_ids: HashSet<String> = repo_tasks.iter().map(|t| t.id.clone()).collect();

    // 1. Orphaned `aaf/batch-*` worktrees and branches.
    match worktree::remove_orphan_batch_worktrees(
        repo,
        worktrees_dir,
        repo_id,
        &active_ids,
        opts.dry_run,
    ) {
        Ok(orphans) => report.removed_orphan_batches.extend(orphans),
        Err(err) => report
            .warnings
            .push(format!("{}: orphan batch sweep: {err}", repo.display())),
    }

    // 2. Done tasks (and Cancelled tasks when `include_cancelled`).
    let registered = worktree::list(repo)?;
    for task in repo_tasks {
        let eligible = match task.state {
            TaskState::Done => true,
            TaskState::Cancelled => opts.include_cancelled,
            _ => false,
        };
        if !eligible {
            continue;
        }
        if worktree::validate_task_id(&task.id).is_err() {
            report
                .warnings
                .push(format!("skipped invalid task id {:?}", task.id));
            continue;
        }
        clean_task_artifacts(repo, worktrees_dir, task, &registered, opts, report)?;
    }

    // 3. `worktree::prune` + `aaf_fp::sweep_stale`.
    if !opts.dry_run {
        match worktree::prune(repo) {
            Ok(()) => report.pruned_repos += 1,
            Err(err) => report
                .warnings
                .push(format!("{}: worktree prune: {err}", repo.display())),
        }
        match aaf_fp::sweep_stale(repo) {
            Ok(n) => report.removed_stale_fp += n,
            Err(err) => report
                .warnings
                .push(format!("{}: sweep_stale: {err}", repo.display())),
        }
    }
    Ok(())
}

fn clean_task_artifacts(
    repo: &Path,
    worktrees_dir: &Path,
    task: &Task,
    registered: &[worktree::WorktreeEntry],
    opts: &GcOptions,
    report: &mut GcReport,
) -> Result<(), GcError> {
    let branch = format!("{BRANCH_PREFIX}{}", task.id);
    let expected_path = worktree::worktree_path(repo, worktrees_dir, &task.id)?;
    let reg_entry = registered
        .iter()
        .find(|e| e.branch.as_deref() == Some(branch.as_str()));
    let existing_wt_path = reg_entry
        .filter(|e| e.path.exists())
        .map(|e| e.path.clone())
        .or_else(|| expected_path.exists().then_some(expected_path.clone()));

    if let Some(wt_path) = &existing_wt_path {
        let dirty = git_raw(wt_path, &["status", "--porcelain"])
            .map(|o| !o.success() || !o.stdout.trim().is_empty())
            .unwrap_or(true);
        if dirty && !opts.force {
            report.skipped_dirty.push(task.id.clone());
            return Ok(());
        }
        if !opts.dry_run {
            if reg_entry.is_some() {
                let p = path_str(wt_path)?;
                if opts.force {
                    git(repo, &["worktree", "remove", "--force", p])?;
                } else {
                    git(repo, &["worktree", "remove", p])?;
                }
            } else if let Ok(meta) = fs::symlink_metadata(wt_path)
                && !meta.file_type().is_symlink()
                && meta.is_dir()
            {
                fs::remove_dir_all(wt_path)?;
            }
        }
        report.removed_worktrees.push(wt_path.display().to_string());
    } else if reg_entry.is_some() && !opts.dry_run {
        worktree::prune(repo)?;
    }

    let branch_ref = format!("refs/heads/{branch}");
    if git_raw(repo, &["show-ref", "--verify", "--quiet", &branch_ref])?.success() {
        if !opts.dry_run {
            let del = git_raw(repo, &["branch", "-D", &branch])?;
            if !del.success() && !del.stderr.to_ascii_lowercase().contains("not found") {
                return Err(GcError::Git(failure(&["branch", "-D", &branch], &del)));
            }
        }
        report.removed_branches.push(branch);
    }

    let snap_prefix = format!("{SNAPSHOT_REF_PREFIX}{}/", task.id);
    if opts.dry_run {
        let refs = git(repo, &["for-each-ref", "--format=%(refname)", &snap_prefix])?;
        report.removed_snapshot_refs += refs.lines().filter(|l| !l.is_empty()).count();
    } else {
        report.removed_snapshot_refs += worktree::delete_snapshot_refs(repo, &task.id)?;
    }
    Ok(())
}

fn remove_and_record(path: &Path, is_dir: bool, dry_run: bool, report: &mut GcReport) {
    if !dry_run {
        let res = if is_dir {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        };
        if let Err(err) = res {
            report.warnings.push(format!("{}: {err}", path.display()));
            return;
        }
    }
    report.removed_files.push(path.display().to_string());
}

fn clean_expired_artifacts(
    data_dir: &Path,
    opts: &GcOptions,
    running_runs: &HashSet<String>,
    now: SystemTime,
    report: &mut GcReport,
) {
    let max_age = Duration::from_secs(opts.older_than_days.saturating_mul(86_400));
    for subdir in ["streams", "reports"] {
        for path in sorted_real_dir_entries(&data_dir.join(subdir)) {
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            let ext = path.extension().and_then(|e| e.to_str());
            if !meta.is_file()
                || meta.file_type().is_symlink()
                || !matches!(ext, Some("jsonl" | "html"))
            {
                continue;
            }
            if ext == Some("jsonl")
                && path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| running_runs.contains(s))
            {
                continue;
            }
            if is_older_than(&meta, max_age, now) {
                remove_and_record(&path, false, opts.dry_run, report);
            }
        }
    }
    for path in sorted_real_dir_entries(&data_dir.join("sops")) {
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_file()
            && !meta.file_type().is_symlink()
            && path.extension().and_then(|e| e.to_str()) == Some("json")
            && is_older_than(&meta, max_age, now)
            && is_terminal_sop_file(&path)
        {
            remove_and_record(&path, false, opts.dry_run, report);
        }
    }
    let locks_dir = data_dir.join("locks");
    for path in sorted_real_dir_entries(&data_dir.join("merge-scratch")) {
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_dir() || meta.file_type().is_symlink() {
            if !opts.dry_run
                && meta.is_file()
                && path.extension().and_then(|e| e.to_str()) == Some("lock")
                && !path.with_extension("").exists()
                && !is_exclusively_locked(&path)
                && is_older_than(&meta, max_age, now)
            {
                let _ = fs::remove_file(&path);
            }
            continue;
        }
        let mut lock_os = path.as_os_str().to_os_string();
        lock_os.push(".lock");
        let lock_path = PathBuf::from(lock_os);
        let merge_locked = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.split_once("-pr").map(|(id, _)| id))
            .filter(|id| {
                !id.is_empty()
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            })
            .is_some_and(|id| is_exclusively_locked(&locks_dir.join(format!("{id}.merge.lock"))));
        if is_exclusively_locked(&lock_path) || merge_locked {
            continue;
        }
        let has_lock = fs::symlink_metadata(&lock_path).is_ok_and(|m| m.is_file());
        if has_lock || is_older_than(&meta, max_age, now) {
            remove_and_record(&path, true, opts.dry_run, report);
            if !opts.dry_run && has_lock && !path.exists() {
                let _ = fs::remove_file(&lock_path);
            }
        }
    }
}

#[derive(Deserialize)]
struct SopStatusPeek {
    status: String,
}

fn is_older_than(meta: &fs::Metadata, max_age: Duration, now: SystemTime) -> bool {
    meta.modified()
        .ok()
        .is_some_and(|m| now.duration_since(m).unwrap_or(Duration::ZERO) >= max_age)
}

fn is_terminal_sop_file(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut buf = Vec::new();
    if file
        .take(MAX_SOP_CHECKPOINT_BYTES + 1)
        .read_to_end(&mut buf)
        .is_err()
        || u64::try_from(buf.len()).unwrap_or(u64::MAX) > MAX_SOP_CHECKPOINT_BYTES
    {
        return false;
    }
    serde_json::from_slice::<SopStatusPeek>(&buf).is_ok_and(|peek| {
        matches!(
            peek.status.trim().to_ascii_lowercase().as_str(),
            "completed" | "aborted" | "failed"
        )
    })
}

fn sorted_real_dir_entries(dir: &Path) -> Vec<PathBuf> {
    if !fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink()) {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    paths.sort();
    paths
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergedPrRow {
    #[serde(default)]
    head_ref_name: String,
    #[serde(default)]
    head_ref_oid: String,
    #[serde(default)]
    base_ref_name: String,
    #[serde(default)]
    merge_commit: Option<MergeCommitRef>,
    #[serde(default)]
    is_cross_repository: bool,
}

#[derive(Debug, Deserialize)]
struct MergeCommitRef {
    #[serde(default)]
    oid: String,
}

fn is_hex_oid(oid: &str) -> bool {
    (40..=64).contains(&oid.len()) && oid.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Verifies squash-merged PRs for `(task_id, local_head_oid)` candidates in one
/// batched `gh pr list` call plus local `git merge-base --is-ancestor` checks.
///
/// Offline or failing `gh` calls return an empty map so `--auto` never crashes.
pub(crate) fn verify_squash_merges(
    repo_root: &Path,
    base_branch: &str,
    base_start: &str,
    candidates: &[(String, String)],
    fetch_base: &dyn Fn() -> Result<(), GitError>,
    runner: &dyn CommandRunner,
) -> Result<HashMap<String, String>, GitError> {
    if candidates.is_empty() {
        return Ok(HashMap::new());
    }
    let args = [
        "pr",
        "list",
        "--state",
        "merged",
        "--base",
        base_branch,
        "--limit",
        "100",
        "--json",
        "headRefName,headRefOid,baseRefName,mergeCommit,isCrossRepository",
    ];
    let Ok(out) = runner.run("gh", &args, repo_root) else {
        return Ok(HashMap::new());
    };
    if !out.success() {
        return Ok(HashMap::new());
    }
    let Ok(rows) = serde_json::from_str::<Vec<MergedPrRow>>(&out.stdout) else {
        return Ok(HashMap::new());
    };

    let by_ref: HashMap<String, (&str, &str)> = candidates
        .iter()
        .map(|(id, oid)| (format!("{BRANCH_PREFIX}{id}"), (id.as_str(), oid.as_str())))
        .collect();

    let mut matched: Vec<(String, String, String)> = Vec::new();
    for row in rows {
        if row.is_cross_repository || row.base_ref_name != base_branch {
            continue;
        }
        let Some(&(task_id, local_head_oid)) = by_ref.get(&row.head_ref_name) else {
            continue;
        };
        if row.head_ref_oid.is_empty() || row.head_ref_oid != local_head_oid {
            continue;
        }
        let Some(merge_oid) = row
            .merge_commit
            .map(|c| c.oid)
            .filter(|oid| is_hex_oid(oid))
        else {
            continue;
        };
        matched.push((task_id.to_string(), local_head_oid.to_string(), merge_oid));
    }

    if matched.is_empty() {
        return Ok(HashMap::new());
    }
    if fetch_base().is_err() {
        return Ok(HashMap::new());
    }

    let mut verified = HashMap::new();
    for (task_id, local_head_oid, merge_oid) in matched {
        let ok = git_raw(
            repo_root,
            &["merge-base", "--is-ancestor", &merge_oid, base_start],
        )
        .is_ok_and(|res| res.code == Some(0));
        if ok {
            verified.insert(task_id, local_head_oid);
        }
    }
    Ok(verified)
}
