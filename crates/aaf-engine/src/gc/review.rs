//! Verified merged `Review` task and orphan `aaf/*` branch sweeping for `aaf gc`.
//!
//! - Keeps DB inspection strictly read-only until `!dry_run` and at least one
//!   `Review` task is verified merged, then opens `Db::open` (RW) to run atomic
//!   CAS `complete_review_task` (`Review -> Done`) **before** touching disk.
//! - Never runs `git fetch` during `--dry-run`.
//! - Never touches `aaf/batch-*` branches, never applies `--force` to dirty
//!   worktrees, and guards orphan `aaf/*` branches against 0-commit false
//!   positives (`branch_oid == base_oid` without a merged PR).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, SystemTime};

use aaf_domain::task::{Task, TaskState};
use serde_json::json;

use super::refs::RepoRefs;
use super::squash::verify_squash_merges_with_warnings;
use super::task_clean::{ResolvedBase, resolve_repo_base};
use super::{GcOptions, GcReport, TaskCleanOpts, clean_task_artifacts_by_id};
use crate::db::{Db, complete_review_task};
use crate::git::cmd::{git, git_raw};
use crate::git::worktree::{self, BRANCH_PREFIX, WorktreeEntry};
use crate::process::SystemRunner;

const GH_LIST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
struct Candidate {
    id: String,
    head_oid: String,
    is_orphan: bool,
}

#[derive(Debug, Clone)]
struct VerifiedMerge {
    id: String,
    head_oid: String,
    is_orphan: bool,
    is_squash: bool,
}

fn fetch_remote_base(repo: &Path, remote: &str, branch: &str) -> Result<(), crate::git::GitError> {
    worktree::ensure_plain_reflogs(repo)?;
    let refspec = format!("+refs/heads/{branch}:refs/remotes/{remote}/{branch}");
    git(repo, &["fetch", "--quiet", "--", remote, &refspec]).map(|_| ())
}

fn resolve_branch_oid(repo: &Path, branch_ref: &str) -> Option<String> {
    let out = git_raw(repo, &["rev-parse", "--verify", "--quiet", branch_ref]).ok()?;
    if !out.success() {
        return None;
    }
    let oid = out.stdout.trim();
    (!oid.is_empty()).then(|| oid.to_string())
}

/// Sweeps verified-merged `Review` tasks and orphan `aaf/*` branches (never
/// `aaf/batch-*`). Non-fatal failures are recorded in `report.warnings`.
#[allow(clippy::too_many_arguments)]
pub(super) fn clean_merged_review_and_orphan_tasks(
    repo: &Path,
    data_dir: &Path,
    worktrees_dir: &Path,
    repo_tasks: &[&Task],
    all_task_ids: &HashSet<String>,
    registered: &[WorktreeEntry],
    refs: &RepoRefs,
    active_ops: &HashSet<String>,
    cached_base: Option<&Result<ResolvedBase, String>>,
    opts: &GcOptions,
    now: SystemTime,
    report: &mut GcReport,
) {
    let mut candidates = Vec::new();
    let mut seen_ids = HashSet::new();

    for task in repo_tasks {
        if task.state != TaskState::Review || worktree::validate_task_id(&task.id).is_err() {
            continue;
        }
        let branch_ref = format!("refs/heads/{BRANCH_PREFIX}{}", task.id);
        if !refs.has_branch(&branch_ref) {
            continue;
        }
        if let Some(head_oid) = refs
            .branch_oid(&branch_ref)
            .map(str::to_string)
            .or_else(|| resolve_branch_oid(repo, &branch_ref))
            && seen_ids.insert(task.id.clone())
        {
            candidates.push(Candidate {
                id: task.id.clone(),
                head_oid,
                is_orphan: false,
            });
        }
    }

    for branch_ref in refs.branch_refs() {
        let Some(id) = branch_ref
            .strip_prefix("refs/heads/")
            .and_then(|short| short.strip_prefix(BRANCH_PREFIX))
        else {
            continue;
        };
        if id.starts_with("batch-")
            || worktree::validate_task_id(id).is_err()
            || all_task_ids.contains(id)
        {
            continue;
        }
        if let Some(head_oid) = refs
            .branch_oid(branch_ref)
            .map(str::to_string)
            .or_else(|| resolve_branch_oid(repo, branch_ref))
            && seen_ids.insert(id.to_string())
        {
            candidates.push(Candidate {
                id: id.to_string(),
                head_oid,
                is_orphan: true,
            });
        }
    }

    if candidates.is_empty() {
        return;
    }

    let owned_base;
    let resolved = match cached_base {
        Some(Ok(resolved)) => resolved,
        Some(Err(err)) => {
            report
                .warnings
                .push(format!("{}: review sweep skipped: {err}", repo.display()));
            return;
        }
        None => match resolve_repo_base(repo) {
            Ok(resolved) => {
                owned_base = resolved;
                &owned_base
            }
            Err(err) => {
                report
                    .warnings
                    .push(format!("{}: review sweep skipped: {err}", repo.display()));
                return;
            }
        },
    };
    let base = &resolved.base;
    let base_start = &resolved.start;

    let fetched_oid;
    let base_oid = if !opts.dry_run
        && let Some(remote) = base.remote.as_deref()
    {
        if let Err(err) = fetch_remote_base(repo, remote, &base.branch) {
            report.warnings.push(format!(
                "{}: review sweep fetch {remote}/{}: {err}",
                repo.display(),
                base.branch
            ));
            resolved.oid.as_str()
        } else {
            match git(
                repo,
                &["rev-parse", "--verify", &format!("{base_start}^{{commit}}")],
            ) {
                Ok(oid) => {
                    fetched_oid = oid;
                    fetched_oid.as_str()
                }
                Err(err) => {
                    report.warnings.push(format!(
                        "{}: review sweep skipped: base {base_start}: {err}",
                        repo.display()
                    ));
                    return;
                }
            }
        }
    } else {
        resolved.oid.as_str()
    };

    let mut verified: Vec<VerifiedMerge> = Vec::new();
    let mut squash_candidates: Vec<(String, String)> = Vec::new();
    let mut by_id: HashMap<String, Candidate> = HashMap::new();

    for cand in candidates {
        let is_ancestor = cand.head_oid.eq_ignore_ascii_case(base_oid)
            || git_raw(
                repo,
                &["merge-base", "--is-ancestor", &cand.head_oid, base_start],
            )
            .is_ok_and(|o| o.code == Some(0));
        // 0-commit guard for orphan `aaf/*` branches outside DB: a fresh branch
        // sitting on `base_oid` is trivially an ancestor, so require a merged PR.
        if is_ancestor && !(cand.is_orphan && cand.head_oid == base_oid) {
            verified.push(VerifiedMerge {
                id: cand.id,
                head_oid: cand.head_oid,
                is_orphan: cand.is_orphan,
                is_squash: false,
            });
        } else {
            squash_candidates.push((cand.id.clone(), cand.head_oid.clone()));
            by_id.insert(cand.id.clone(), cand);
        }
    }

    if !squash_candidates.is_empty() && base.remote.is_some() {
        let runner = SystemRunner {
            timeout: GH_LIST_TIMEOUT,
        };
        let fetch_fn = || {
            if opts.dry_run {
                return Ok(());
            }
            match base.remote.as_deref() {
                Some(remote) => fetch_remote_base(repo, remote, &base.branch),
                None => Ok(()),
            }
        };
        let mut squash_warnings = Vec::new();
        let matched = verify_squash_merges_with_warnings(
            repo,
            &base.branch,
            base_start,
            &squash_candidates,
            &fetch_fn,
            &runner,
            &mut |w| squash_warnings.push(w),
        );
        report.warnings.extend(squash_warnings);
        match matched {
            Ok(map) => {
                for (id, head_oid) in &squash_candidates {
                    if map.get(id).is_some_and(|v| v == head_oid)
                        && let Some(cand) = by_id.remove(id)
                    {
                        verified.push(VerifiedMerge {
                            id: cand.id,
                            head_oid: cand.head_oid,
                            is_orphan: cand.is_orphan,
                            is_squash: true,
                        });
                    }
                }
            }
            Err(err) => report
                .warnings
                .push(format!("{}: squash verify: {err}", repo.display())),
        }
    }

    if verified.is_empty() {
        return;
    }

    let has_review_to_complete = verified.iter().any(|v| !v.is_orphan);
    let db_path = data_dir.join("aaf.db");
    let rw_db = if !opts.dry_run && has_review_to_complete && db_path.is_file() {
        match Db::open(&db_path) {
            Ok(db) => Some(db),
            Err(err) => {
                report.warnings.push(format!(
                    "{}: review sweep could not open db read-write: {err}",
                    repo.display()
                ));
                None
            }
        }
    } else {
        None
    };

    let now_epoch = i64::try_from(
        now.duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    )
    .unwrap_or(i64::MAX);

    for item in verified {
        if !item.is_orphan {
            if opts.dry_run {
                report.completed_review_tasks.push(item.id.clone());
            } else {
                let Some(db) = rw_db.as_ref() else {
                    report.warnings.push(format!(
                        "{}: skipped cleanup because database could not be opened read-write",
                        item.id
                    ));
                    continue;
                };
                let (why, merge_payload) = if item.is_squash {
                    (
                        "pull request squash-merged into the base",
                        Some(json!({
                            "merged": true,
                            "kind": "squash_pr",
                            "head_oid": item.head_oid,
                            "head_sha": item.head_oid,
                            "by": "aaf_gc",
                        })),
                    )
                } else {
                    (
                        "branch is merged into the base",
                        Some(json!({
                            "merged": true,
                            "kind": "ancestry",
                            "head_sha": item.head_oid,
                            "by": "aaf_gc",
                        })),
                    )
                };
                match complete_review_task(db, &item.id, why, merge_payload.as_ref(), now_epoch) {
                    Ok(true) => report.completed_review_tasks.push(item.id.clone()),
                    Ok(false) => {
                        report.warnings.push(format!(
                            "{}: task state changed concurrently (no longer Review): skipped cleanup",
                            item.id
                        ));
                        continue;
                    }
                    Err(err) => {
                        report.warnings.push(format!(
                            "{}: complete_review_task failed: {err}: skipped cleanup",
                            item.id
                        ));
                        continue;
                    }
                }
            }
        }

        // Never apply `--force` to dirty worktrees during review/orphan sweep.
        let clean_opts = TaskCleanOpts {
            dry_run: opts.dry_run,
            force: false,
            require_verified_merge: true,
            base: Some(resolved),
            cwd: opts.cwd.as_deref(),
            expected_head: Some(item.head_oid.as_str()),
        };
        if let Err(err) = clean_task_artifacts_by_id(
            repo,
            worktrees_dir,
            &item.id,
            registered,
            refs,
            active_ops,
            &clean_opts,
            report,
        ) {
            report.warnings.push(format!("{}: {err}", item.id));
        }
    }
}
