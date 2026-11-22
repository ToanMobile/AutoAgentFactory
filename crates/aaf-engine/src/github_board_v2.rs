//! GitHub Projects v2 metadata loading, collision-safe filtering, auto-add, and status edits.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use aaf_domain::config::GithubBoardConfig;
use aaf_domain::ports::{PortError, PortResult};
use serde_json::Value;

use crate::process::CommandRunner;

const PROJECT_ITEM_LIMIT: usize = 200;

pub(super) struct ProjectItemInfo {
    pub(super) item_id: String,
    pub(super) current_status: Option<String>,
}

pub(super) struct ProjectV2Meta {
    pub(super) project_number: u32,
    pub(super) owner: String,
    pub(super) project_id: String,
    pub(super) status_field_id: String,
    pub(super) option_ids: HashMap<String, String>,
    pub(super) issue_item_ids: HashMap<u64, ProjectItemInfo>,
    pub(super) ambiguous_numbers: HashSet<u64>,
    pub(super) truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProjectSyncStatus {
    Synced,
    Unchanged,
}

fn non_empty_str(value: &Value) -> Option<&str> {
    value.as_str().map(str::trim).filter(|s| !s.is_empty())
}

pub(super) fn resolve_project_owner(cfg: &GithubBoardConfig) -> String {
    let explicit = cfg.project_owner.trim();
    if !explicit.is_empty() {
        return explicit.to_string();
    }
    if let Some((owner, _)) = cfg.repo.trim().split_once('/') {
        let owner = owner.trim();
        if !owner.is_empty() {
            return owner.to_string();
        }
    }
    "@me".to_string()
}

pub(super) fn validate_column_option(meta: &ProjectV2Meta, column: &str) -> PortResult<()> {
    if meta
        .option_ids
        .contains_key(&column.trim().to_ascii_lowercase())
    {
        Ok(())
    } else {
        Err(PortError::Invalid(format!(
            "GitHub project #{} Status field has no option matching column '{column}'",
            meta.project_number
        )))
    }
}

fn run_gh_json(
    runner: &dyn CommandRunner,
    cwd: &Path,
    args: &[&str],
    ctx: &str,
) -> PortResult<Value> {
    let out = runner
        .run("gh", args, cwd)
        .map_err(|e| PortError::Io(format!("{ctx}: {e}")))?;
    if !out.success() {
        return Err(PortError::Io(format!(
            "{ctx} failed: {}",
            out.stderr.trim()
        )));
    }
    serde_json::from_str(&out.stdout)
        .map_err(|e| PortError::Invalid(format!("{ctx} returned invalid json: {e}")))
}

pub(super) fn load_project_v2_meta(
    runner: &dyn CommandRunner,
    cwd: &Path,
    cfg: &GithubBoardConfig,
    project_number: u32,
    owner: &str,
) -> PortResult<ProjectV2Meta> {
    let num = project_number.to_string();
    let view = run_gh_json(
        runner,
        cwd,
        &[
            "project", "view", &num, "--owner", owner, "--format", "json",
        ],
        &format!("gh project view #{project_number}"),
    )?;
    let project_id = non_empty_str(&view["id"])
        .ok_or_else(|| {
            PortError::Invalid(format!("gh project #{project_number} response missing id"))
        })?
        .to_string();

    let field_list = run_gh_json(
        runner,
        cwd,
        &[
            "project",
            "field-list",
            &num,
            "--owner",
            owner,
            "--format",
            "json",
        ],
        &format!("gh project field-list #{project_number}"),
    )?;
    let status_field = field_list["fields"]
        .as_array()
        .and_then(|fields| {
            fields.iter().find(|f| {
                f["name"]
                    .as_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case("status"))
            })
        })
        .ok_or_else(|| {
            PortError::Invalid(format!(
                "GitHub project #{project_number} has no Status field"
            ))
        })?;
    let status_field_id = non_empty_str(&status_field["id"])
        .ok_or_else(|| {
            PortError::Invalid(format!(
                "GitHub project #{project_number} Status field is missing id"
            ))
        })?
        .to_string();

    let mut option_ids = HashMap::new();
    if let Some(options) = status_field["options"].as_array() {
        for opt in options {
            if let (Some(id), Some(name)) = (non_empty_str(&opt["id"]), non_empty_str(&opt["name"]))
            {
                option_ids.insert(name.to_ascii_lowercase(), id.to_string());
            }
        }
    }
    if option_ids.is_empty() {
        return Err(PortError::Invalid(format!(
            "GitHub project #{project_number} Status field has no options"
        )));
    }

    let limit = PROJECT_ITEM_LIMIT.to_string();
    let item_list = run_gh_json(
        runner,
        cwd,
        &[
            "project",
            "item-list",
            &num,
            "--owner",
            owner,
            "--format",
            "json",
            "--limit",
            &limit,
        ],
        &format!("gh project item-list #{project_number}"),
    )?;

    let raw_items = item_list["items"].as_array();
    let item_count = raw_items.map_or(0, Vec::len);
    let truncated = item_list["totalCount"]
        .as_u64()
        .is_some_and(|total| total > item_count as u64)
        || item_count >= PROJECT_ITEM_LIMIT;

    let expected_repo = cfg.repo.trim();
    let mut distinct_repos = HashSet::new();
    if let Some(items) = raw_items {
        for item in items {
            if let Some(repo) = non_empty_str(&item["content"]["repository"]) {
                distinct_repos.insert(repo.to_ascii_lowercase());
            }
        }
    }
    let multi_repo_board = distinct_repos.len() > 1
        || (!expected_repo.is_empty()
            && distinct_repos
                .iter()
                .any(|repo| !repo.eq_ignore_ascii_case(expected_repo)));

    let mut issue_item_ids: HashMap<u64, ProjectItemInfo> = HashMap::new();
    let mut ambiguous_numbers = HashSet::new();
    if let Some(items) = raw_items {
        for item in items {
            if item["content"]["type"]
                .as_str()
                .is_some_and(|kind| !kind.trim().eq_ignore_ascii_case("issue"))
            {
                continue;
            }
            if !expected_repo.is_empty() {
                let matches = match non_empty_str(&item["content"]["repository"]) {
                    Some(item_repo) => item_repo.eq_ignore_ascii_case(expected_repo),
                    None => !multi_repo_board,
                };
                if !matches {
                    continue;
                }
            }
            if let (Some(id), Some(number)) = (
                non_empty_str(&item["id"]),
                item["content"]["number"].as_u64(),
            ) {
                if expected_repo.is_empty() {
                    if ambiguous_numbers.contains(&number) {
                        continue;
                    }
                    if let Some(existing) = issue_item_ids.get(&number)
                        && existing.item_id != id
                    {
                        issue_item_ids.remove(&number);
                        ambiguous_numbers.insert(number);
                        continue;
                    }
                }
                issue_item_ids.insert(
                    number,
                    ProjectItemInfo {
                        item_id: id.to_string(),
                        current_status: non_empty_str(&item["status"]).map(str::to_string),
                    },
                );
            }
        }
    }

    Ok(ProjectV2Meta {
        project_number,
        owner: owner.to_string(),
        project_id,
        status_field_id,
        option_ids,
        issue_item_ids,
        ambiguous_numbers,
        truncated,
    })
}

fn add_project_v2_item(
    runner: &dyn CommandRunner,
    cwd: &Path,
    project_number: u32,
    owner: &str,
    repo_slug: &str,
    issue_number: u64,
) -> PortResult<String> {
    let num = project_number.to_string();
    let url = format!("https://github.com/{repo_slug}/issues/{issue_number}");
    let added = run_gh_json(
        runner,
        cwd,
        &[
            "project", "item-add", &num, "--owner", owner, "--url", &url, "--format", "json",
        ],
        &format!("gh project item-add #{issue_number} to project #{project_number}"),
    )?;
    non_empty_str(&added["id"])
        .map(str::to_string)
        .ok_or_else(|| {
            PortError::Invalid(format!(
                "gh project item-add #{issue_number} response missing id"
            ))
        })
}

pub(super) fn sync_project_v2_item(
    runner: &dyn CommandRunner,
    cwd: &Path,
    meta: &mut ProjectV2Meta,
    cfg: &GithubBoardConfig,
    issue_number: u64,
    column: &str,
) -> PortResult<ProjectSyncStatus> {
    let option_id = meta
        .option_ids
        .get(&column.trim().to_ascii_lowercase())
        .cloned()
        .ok_or_else(|| {
            PortError::Invalid(format!(
                "GitHub project #{} Status field has no option matching column '{column}'",
                meta.project_number
            ))
        })?;

    if meta.ambiguous_numbers.contains(&issue_number) {
        return Err(PortError::Invalid(format!(
            "issue #{issue_number} matches multiple items in GitHub project #{}; set [github].repo in .autofactory/config.toml to disambiguate",
            meta.project_number
        )));
    }

    let item_id = if let Some(info) = meta.issue_item_ids.get(&issue_number) {
        if info
            .current_status
            .as_deref()
            .is_some_and(|status| status.trim().eq_ignore_ascii_case(column.trim()))
        {
            return Ok(ProjectSyncStatus::Unchanged);
        }
        info.item_id.clone()
    } else {
        let repo_slug = cfg.repo.trim();
        if repo_slug.is_empty() {
            return Err(PortError::Invalid(format!(
                "issue #{issue_number} is not present in GitHub project #{} and [github].repo is empty",
                meta.project_number
            )));
        }
        add_project_v2_item(
            runner,
            cwd,
            meta.project_number,
            &meta.owner,
            repo_slug,
            issue_number,
        )?
    };

    let args = [
        "project",
        "item-edit",
        "--project-id",
        meta.project_id.as_str(),
        "--id",
        item_id.as_str(),
        "--field-id",
        meta.status_field_id.as_str(),
        "--single-select-option-id",
        option_id.as_str(),
    ];
    let out = runner
        .run("gh", &args, cwd)
        .map_err(|e| PortError::Io(format!("gh project item-edit #{issue_number}: {e}")))?;
    if !out.success() {
        return Err(PortError::Io(format!(
            "gh project item-edit #{issue_number} failed: {}",
            out.stderr.trim()
        )));
    }
    meta.issue_item_ids.insert(
        issue_number,
        ProjectItemInfo {
            item_id,
            current_status: Some(column.to_string()),
        },
    );
    Ok(ProjectSyncStatus::Synced)
}
