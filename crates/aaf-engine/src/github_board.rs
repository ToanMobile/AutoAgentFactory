//! GitHub issue intake (allowlisted + 72h dedup) and Super-Board column sync.

#[path = "github_board_v2.rs"]
mod project_v2;

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use aaf_domain::config::{GithubBoardConfig, GithubColumnMap};
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{PortError, PortResult};
use aaf_domain::task::{Priority, Task, TaskState};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use self::project_v2::{
    ProjectSyncStatus, load_project_v2_meta, resolve_project_owner, sync_project_v2_item,
    validate_column_option,
};
use crate::detector::dedupe::{DedupeDecision, decide};
use crate::detector::github_issues::{fetch_filtered, to_task_spec};
use crate::factory::Store;
use crate::process::CommandRunner;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportOutcome {
    #[serde(default)]
    pub dry_run: bool,
    pub fetched: usize,
    pub created: Vec<String>,
    pub skipped_open: Vec<String>,
    pub suppressed_recent: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncItem {
    pub task_id: String,
    pub issue_number: Option<u64>,
    pub state: TaskState,
    pub column: String,
    pub synced: bool,
    #[serde(default)]
    pub unchanged: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncOutcome {
    pub dry_run: bool,
    pub mode: String,
    pub items: Vec<SyncItem>,
    pub skipped_no_issue: Vec<String>,
    #[serde(default)]
    pub truncated_project_items: bool,
}

pub fn has_trusted_login(allowlist: &[String]) -> bool {
    allowlist.iter().any(|s| !s.trim().is_empty())
}

pub fn map_state_to_column(state: TaskState, map: &GithubColumnMap) -> &str {
    match state {
        TaskState::Backlog => &map.backlog,
        TaskState::Ready => &map.ready,
        TaskState::Running => &map.running,
        TaskState::Gating => &map.gating,
        TaskState::Review => &map.review,
        TaskState::Blocked => &map.blocked,
        TaskState::Done => &map.done,
        TaskState::Cancelled => &map.cancelled,
    }
}

/// Imports open GitHub issues authored by allowlisted logins, deduplicating
/// against open tasks and tasks closed within the last 72 hours.
pub fn import_issues(
    runner: &dyn CommandRunner,
    store: &dyn Store,
    repo_root: &Path,
    allowlist: &[String],
    now: i64,
) -> PortResult<ImportOutcome> {
    let cfg = GithubBoardConfig {
        enabled: true,
        ..GithubBoardConfig::default()
    };
    import_issues_with_config(runner, store, repo_root, allowlist, &cfg, now)
}

/// Config-aware GitHub issue import enforcing `[github].enabled`, `repo`, and
/// `label_filter` alongside the trusted-login allowlist and 72h dedup window.
pub fn import_issues_with_config(
    runner: &dyn CommandRunner,
    store: &dyn Store,
    repo_root: &Path,
    allowlist: &[String],
    cfg: &GithubBoardConfig,
    now: i64,
) -> PortResult<ImportOutcome> {
    import_issues_with_options(runner, store, repo_root, allowlist, cfg, false, now)
}

/// Config-aware GitHub issue import with optional `dry_run` preview.
///
/// The trusted-login `allowlist` always fails closed, even when `dry_run` is
/// `true`. When `dry_run` is `true`, issues are fetched, filtered, and
/// deduplicated normally, candidate task IDs are recorded in `outcome.created`,
/// and zero writes are made to `store` (no `insert_task`, no
/// `append_context_note`, no `emit`).
pub fn import_issues_with_options(
    runner: &dyn CommandRunner,
    store: &dyn Store,
    repo_root: &Path,
    allowlist: &[String],
    cfg: &GithubBoardConfig,
    dry_run: bool,
    now: i64,
) -> PortResult<ImportOutcome> {
    import_issues_inner(runner, Some(store), repo_root, allowlist, cfg, dry_run, now)
}

/// Strictly read-only dry-run preview of GitHub issue intake that also works
/// when `store` is `None` (no `aaf.db` on disk yet).
pub fn import_issues_dry_run(
    runner: &dyn CommandRunner,
    store: Option<&dyn Store>,
    repo_root: &Path,
    allowlist: &[String],
    cfg: &GithubBoardConfig,
    now: i64,
) -> PortResult<ImportOutcome> {
    import_issues_inner(runner, store, repo_root, allowlist, cfg, true, now)
}

fn import_issues_inner(
    runner: &dyn CommandRunner,
    store: Option<&dyn Store>,
    repo_root: &Path,
    allowlist: &[String],
    cfg: &GithubBoardConfig,
    dry_run: bool,
    now: i64,
) -> PortResult<ImportOutcome> {
    if !has_trusted_login(allowlist) {
        return Err(PortError::Invalid(
            "safety.owner_github_allowlist is empty; refusing untrusted GitHub issue import"
                .to_string(),
        ));
    }
    if !dry_run && !cfg.enabled {
        return Err(PortError::Invalid(
            "[github].enabled is false; enable [github] in .autofactory/config.toml before importing issues (or use --dry-run)"
                .to_string(),
        ));
    }
    let canon_root = fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let repo_str = canon_root.to_string_lossy().into_owned();
    let repo_slug = (!cfg.repo.trim().is_empty()).then_some(cfg.repo.trim());
    let label_filter = (!cfg.label_filter.trim().is_empty()).then_some(cfg.label_filter.trim());
    let candidates = fetch_filtered(&canon_root, allowlist, repo_slug, label_filter, runner)
        .map_err(|e| PortError::Io(format!("github issue fetch: {e}")))?;

    let mut outcome = ImportOutcome {
        dry_run,
        fetched: candidates.len(),
        ..ImportOutcome::default()
    };
    for candidate in candidates {
        let fingerprint = format!("gh-issue:#{}", candidate.number);
        let decision = match store {
            Some(s) => decide(&fingerprint, &repo_str, s, now)?,
            None => DedupeDecision::CreateTask,
        };
        match decision {
            DedupeDecision::AttachToOpen { task_id } => {
                if !dry_run && let Some(s) = store {
                    let note = format!("gh#{}: still open ({})", candidate.number, candidate.title);
                    if !s
                        .get_task(&task_id)?
                        .context_notes
                        .iter()
                        .any(|n| n == &note)
                    {
                        s.append_context_note(&task_id, &note, now)?;
                    }
                }
                outcome.skipped_open.push(task_id);
            }
            DedupeDecision::SuppressRecentlyClosed { task_id, .. } => {
                outcome.suppressed_recent.push(task_id);
            }
            DedupeDecision::CreateTask => {
                let (title, spec) = to_task_spec(&candidate);
                let id = match store {
                    Some(s) => next_task_id(s, &repo_str, candidate.number, now)?,
                    None => candidate_task_id(&repo_str, candidate.number, now, 0),
                };
                if !dry_run && let Some(s) = store {
                    let mut task = Task::new_root(
                        id.clone(),
                        repo_str.clone(),
                        title,
                        spec,
                        Priority::P2,
                        now,
                    );
                    task.state = TaskState::Ready;
                    task.fingerprint = Some(fingerprint.clone());
                    s.insert_task(&task)?;

                    let hit = json!({
                        "source": "github_issue", "number": candidate.number,
                        "author": candidate.author, "fingerprint": fingerprint,
                    });
                    let created = json!({"title": task.title, "issue_number": candidate.number});
                    s.emit(
                        &FactoryEvent::new(EventKind::DetectorHit, hit.to_string(), now)
                            .for_task(&id),
                    )?;
                    s.emit(
                        &FactoryEvent::new(EventKind::TaskCreated, created.to_string(), now)
                            .for_task(&id),
                    )?;
                }
                outcome.created.push(id);
            }
        }
    }
    Ok(outcome)
}

/// Maps every task in `repo_root` to its Super-Board column and updates either
/// GitHub Projects v2 Status columns (when `cfg.project_number` is configured)
/// or issue `status:<column>` labels through `runner` unless `dry_run` is true.
pub fn sync_board(
    runner: &dyn CommandRunner,
    store: &dyn Store,
    repo_root: &Path,
    cfg: &GithubBoardConfig,
    dry_run: bool,
) -> PortResult<SyncOutcome> {
    if !dry_run && !cfg.enabled {
        return Err(PortError::Invalid(
            "[github].enabled is false; enable [github] in .autofactory/config.toml before syncing (or use --dry-run)"
                .to_string(),
        ));
    }

    let canon_root = fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let repo_str = canon_root.to_string_lossy().into_owned();
    let mode = if cfg.project_number.is_some() {
        "project_v2"
    } else {
        "issue_label"
    }
    .to_string();
    let planned = plan_sync_tasks(store.list_tasks(Some(&repo_str), None)?, cfg);

    let linked_issue_count = planned
        .iter()
        .filter(|(_, num, _, _)| num.is_some())
        .count();
    let has_linked_issue = linked_issue_count > 0;
    let mut project_meta = match cfg.project_number {
        Some(project_num) if !dry_run && has_linked_issue => {
            let owner = resolve_project_owner(cfg);
            let meta = load_project_v2_meta(runner, &canon_root, cfg, project_num, &owner)?;
            for (_, issue_number, _, column) in &planned {
                if issue_number.is_some() {
                    validate_column_option(&meta, column)?;
                }
            }
            Some(meta)
        }
        _ => None,
    };
    let truncated_project_items = project_meta.as_ref().is_some_and(|meta| meta.truncated);
    let batch_labels =
        if !dry_run && project_meta.is_none() && linked_issue_count >= BATCH_ISSUE_LIST_MIN {
            fetch_issue_labels_batch(runner, &canon_root, cfg)
        } else {
            HashMap::new()
        };

    let mut items = Vec::with_capacity(planned.len());
    let mut skipped_no_issue = Vec::new();
    let mut first_err: Option<PortError> = None;

    for (task_id, issue_number, state, column) in planned {
        let (synced, unchanged, error) = match issue_number {
            None => {
                skipped_no_issue.push(task_id.clone());
                (false, false, None)
            }
            Some(_) if dry_run => (false, false, None),
            Some(num) => {
                let res = if let Some(meta) = project_meta.as_mut() {
                    sync_project_v2_item(runner, &canon_root, meta, cfg, num, &column)
                } else {
                    sync_issue_label(
                        runner,
                        &canon_root,
                        cfg,
                        num,
                        &column,
                        batch_labels.get(&num).map(Vec::as_slice),
                    )
                };
                match res {
                    Ok(ProjectSyncStatus::Synced) => (true, false, None),
                    Ok(ProjectSyncStatus::Unchanged) => (false, true, None),
                    Err(err) => {
                        let msg = err.to_string();
                        first_err.get_or_insert(err);
                        (false, false, Some(msg))
                    }
                }
            }
        };
        items.push(SyncItem {
            task_id,
            issue_number,
            state,
            column,
            synced,
            unchanged,
            error,
        });
    }

    let synced_or_unchanged = items.iter().filter(|i| i.synced || i.unchanged).count();
    if let Some(err) = first_err.filter(|_| synced_or_unchanged == 0) {
        return Err(err);
    }
    Ok(SyncOutcome {
        dry_run,
        mode,
        items,
        skipped_no_issue,
        truncated_project_items,
    })
}

fn is_more_authoritative(candidate: &Task, incumbent: &Task) -> bool {
    fn rank(t: &Task) -> (bool, i64, i64, &str) {
        (
            !t.state.is_terminal(),
            t.updated_at,
            t.created_at,
            t.id.as_str(),
        )
    }
    rank(candidate) > rank(incumbent)
}

/// Keeps the single authoritative task per linked `issue_number` (any non-terminal
/// state beats `Done`/`Cancelled`; ties break by latest `updated_at`, then `created_at`).
fn plan_sync_tasks(
    tasks: Vec<Task>,
    cfg: &GithubBoardConfig,
) -> Vec<(String, Option<u64>, TaskState, String)> {
    let parsed: Vec<Option<u64>> = tasks
        .iter()
        .map(|t| t.fingerprint.as_deref().and_then(parse_issue_number))
        .collect();
    let mut best_by_issue: HashMap<u64, usize> = HashMap::new();
    for (idx, issue_num) in parsed.iter().enumerate() {
        if let Some(num) = *issue_num {
            let replace = best_by_issue
                .get(&num)
                .is_none_or(|&prev| is_more_authoritative(&tasks[idx], &tasks[prev]));
            if replace {
                best_by_issue.insert(num, idx);
            }
        }
    }
    tasks
        .into_iter()
        .zip(parsed)
        .enumerate()
        .filter(|(idx, (_, num))| num.is_none_or(|n| best_by_issue.get(&n) == Some(idx)))
        .map(|(_, (task, issue_number))| {
            let column = map_state_to_column(task.state, &cfg.columns).to_string();
            (task.id, issue_number, task.state, column)
        })
        .collect()
}

fn other_column_labels(columns: &GithubColumnMap, current: &str) -> Vec<String> {
    let all = [
        &columns.backlog,
        &columns.ready,
        &columns.running,
        &columns.gating,
        &columns.review,
        &columns.blocked,
        &columns.done,
        &columns.cancelled,
    ];
    let current_trimmed = current.trim();
    let mut seen = Vec::new();
    let mut labels = Vec::new();
    for col in all {
        let trimmed = col.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case(current_trimmed) {
            continue;
        }
        let key = trimmed.to_ascii_lowercase();
        if !seen.contains(&key) {
            seen.push(key);
            labels.push(format!("status:{trimmed}"));
        }
    }
    labels
}

/// Minimum linked issues before issuing one batched `gh issue list` instead of
/// per-issue `gh issue view` calls.
const BATCH_ISSUE_LIST_MIN: usize = 3;

fn parse_label_names(labels: &[Value]) -> Vec<String> {
    labels
        .iter()
        .filter_map(|label| label["name"].as_str().map(str::trim))
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

fn fetch_issue_labels_batch(
    runner: &dyn CommandRunner,
    cwd: &Path,
    cfg: &GithubBoardConfig,
) -> HashMap<u64, Vec<String>> {
    let mut args = vec![
        "issue",
        "list",
        "--state",
        "all",
        "--limit",
        "200",
        "--json",
        "number,labels",
    ];
    if !cfg.repo.trim().is_empty() {
        args.extend(["--repo", cfg.repo.trim()]);
    }
    let Ok(out) = runner.run("gh", &args, cwd) else {
        return HashMap::new();
    };
    let rows = out
        .success()
        .then(|| serde_json::from_str::<Vec<Value>>(&out.stdout).ok())
        .flatten()
        .unwrap_or_default();
    let mut cache = HashMap::with_capacity(rows.len());
    for row in rows {
        if let (Some(num), Some(labels)) = (row["number"].as_u64(), row["labels"].as_array()) {
            cache.insert(num, parse_label_names(labels));
        }
    }
    cache
}

fn fetch_issue_labels(
    runner: &dyn CommandRunner,
    cwd: &Path,
    cfg: &GithubBoardConfig,
    issue_number: u64,
) -> PortResult<Vec<String>> {
    let num_str = issue_number.to_string();
    let mut args: Vec<&str> = vec!["issue", "view", &num_str, "--json", "labels"];
    if !cfg.repo.trim().is_empty() {
        args.push("--repo");
        args.push(cfg.repo.trim());
    }
    let out = runner
        .run("gh", &args, cwd)
        .map_err(|e| PortError::Io(format!("gh issue view #{issue_number}: {e}")))?;
    if !out.success() {
        return Err(PortError::Io(format!(
            "gh issue view #{issue_number} failed: {}",
            out.stderr.trim()
        )));
    }
    let parsed: Value = serde_json::from_str(&out.stdout).map_err(|e| {
        PortError::Invalid(format!(
            "gh issue view #{issue_number} returned invalid json: {e}"
        ))
    })?;
    let labels = parsed["labels"].as_array().ok_or_else(|| {
        PortError::Invalid(format!(
            "gh issue view #{issue_number} response missing labels array"
        ))
    })?;
    Ok(parse_label_names(labels))
}

fn sync_issue_label(
    runner: &dyn CommandRunner,
    cwd: &Path,
    cfg: &GithubBoardConfig,
    issue_number: u64,
    column: &str,
    cached_labels: Option<&[String]>,
) -> PortResult<ProjectSyncStatus> {
    let current = match cached_labels {
        Some(labels) => labels.to_vec(),
        None => fetch_issue_labels(runner, cwd, cfg, issue_number)?,
    };
    let add_label = format!("status:{column}");
    let has_target = current
        .iter()
        .any(|label| label.eq_ignore_ascii_case(&add_label));
    let configured_stale = other_column_labels(&cfg.columns, column);
    let mut stale_attached: Vec<String> = Vec::new();
    for attached in &current {
        let is_stale = configured_stale
            .iter()
            .any(|stale| stale.eq_ignore_ascii_case(attached));
        if is_stale
            && !stale_attached
                .iter()
                .any(|seen| seen.eq_ignore_ascii_case(attached))
        {
            stale_attached.push(attached.clone());
        }
    }
    if has_target && stale_attached.is_empty() {
        return Ok(ProjectSyncStatus::Unchanged);
    }

    let num_str = issue_number.to_string();
    let mut args: Vec<&str> = Vec::with_capacity(7 + stale_attached.len() * 2);
    args.extend(["issue", "edit", &num_str]);
    if !has_target {
        args.extend(["--add-label", &add_label]);
    }
    for stale in &stale_attached {
        args.push("--remove-label");
        args.push(stale.as_str());
    }
    if !cfg.repo.trim().is_empty() {
        args.push("--repo");
        args.push(cfg.repo.trim());
    }
    let out = runner
        .run("gh", &args, cwd)
        .map_err(|e| PortError::Io(format!("gh issue edit #{issue_number}: {e}")))?;
    if !out.success() {
        return Err(PortError::Io(format!(
            "gh issue edit #{issue_number} failed: {}",
            out.stderr.trim()
        )));
    }
    Ok(ProjectSyncStatus::Synced)
}

fn parse_issue_number(fp: &str) -> Option<u64> {
    fp.strip_prefix("gh-issue:")?
        .rsplit_once('#')?
        .1
        .parse()
        .ok()
}

fn candidate_task_id(repo: &str, number: u64, now: i64, attempt: u32) -> String {
    let seed = format!("{repo}\ngh#{number}\n{now}\n{attempt}");
    format!("t-{}", &aaf_fp::sha256_hex(seed.as_bytes())[..8])
}

fn next_task_id(store: &dyn Store, repo: &str, number: u64, now: i64) -> PortResult<String> {
    for attempt in 0_u32..100 {
        let id = candidate_task_id(repo, number, now, attempt);
        match store.get_task(&id) {
            Err(PortError::NotFound(_)) => return Ok(id),
            Ok(_) => {}
            Err(other) => return Err(other),
        }
    }
    Err(PortError::Conflict(format!(
        "could not allocate unique task id for issue #{number}"
    )))
}
