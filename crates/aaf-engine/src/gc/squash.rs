//! Verified squash-merge matching for the factory review sweep.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::Deserialize;

use crate::git::GitError;
use crate::git::cmd::git_raw;
use crate::git::worktree::BRANCH_PREFIX;
use crate::process::CommandRunner;

const BULK_PR_LIMIT: usize = 100;
const PR_JSON_FIELDS: &str = "headRefName,headRefOid,baseRefName,mergeCommit,isCrossRepository";

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

fn is_gh_auth_or_rate_limit(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    [
        "rate limit",
        "secondary rate",
        "abuse detection",
        "403",
        "429",
        "401",
        "not logged",
        "gh auth login",
        "authentication",
        "bad credentials",
        "unauthorized",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn emit_once(seen: &mut HashSet<String>, on_warning: &mut dyn FnMut(String), msg: String) {
    if seen.insert(msg.clone()) {
        on_warning(msg);
    }
}

fn match_row(
    row: MergedPrRow,
    base_branch: &str,
    expected_head_ref: &str,
    local_head_oid: &str,
) -> Option<String> {
    if row.is_cross_repository
        || row.base_ref_name != base_branch
        || row.head_ref_name != expected_head_ref
        || row.head_ref_oid.is_empty()
        || row.head_ref_oid != local_head_oid
    {
        return None;
    }
    row.merge_commit
        .map(|c| c.oid)
        .filter(|oid| is_hex_oid(oid))
}

/// Verifies squash-merged PRs for `(task_id, local_head_oid)` candidates in one
/// batched `gh pr list` call plus local `git merge-base --is-ancestor` checks.
///
/// Offline or failing `gh` calls return an empty map so `--auto` never crashes.
#[allow(dead_code)]
pub(crate) fn verify_squash_merges(
    repo_root: &Path,
    base_branch: &str,
    base_start: &str,
    candidates: &[(String, String)],
    fetch_base: &dyn Fn() -> Result<(), GitError>,
    runner: &dyn CommandRunner,
) -> Result<HashMap<String, String>, GitError> {
    verify_squash_merges_with_warnings(
        repo_root,
        base_branch,
        base_start,
        candidates,
        fetch_base,
        runner,
        &mut |_| {},
    )
}

/// Like [`verify_squash_merges`], emitting deduplicated `gh` warnings to
/// `on_warning`. When the bulk `--limit 100` listing saturates (`rows.len() == 100`),
/// falls back to per-branch `gh pr list --head <branch> --base <base> --state merged --limit 5`
/// for unmatched candidates, stopping immediately on auth or rate-limit errors.
pub(crate) fn verify_squash_merges_with_warnings(
    repo_root: &Path,
    base_branch: &str,
    base_start: &str,
    candidates: &[(String, String)],
    fetch_base: &dyn Fn() -> Result<(), GitError>,
    runner: &dyn CommandRunner,
    on_warning: &mut dyn FnMut(String),
) -> Result<HashMap<String, String>, GitError> {
    if candidates.is_empty() {
        return Ok(HashMap::new());
    }
    let branch_candidates: Vec<(String, String)> = candidates
        .iter()
        .map(|(id, oid)| (format!("{BRANCH_PREFIX}{id}"), oid.clone()))
        .collect();
    let verified_branches = verify_branch_squash_merges_with_warnings(
        repo_root,
        base_branch,
        base_start,
        &branch_candidates,
        fetch_base,
        runner,
        on_warning,
    )?;
    Ok(verified_branches
        .into_iter()
        .filter_map(|(branch, oid)| {
            branch
                .strip_prefix(BRANCH_PREFIX)
                .map(|id| (id.to_string(), oid))
        })
        .collect())
}

/// Verifies squash-merged PRs for full branch names `(branch_name, local_head_oid)`
/// in one batched `gh pr list` call plus local `git merge-base --is-ancestor` checks.
///
/// Offline or failing `gh` calls return an empty map and emit deduplicated warnings.
pub(crate) fn verify_branch_squash_merges_with_warnings(
    repo_root: &Path,
    base_branch: &str,
    base_start: &str,
    candidates: &[(String, String)],
    fetch_base: &dyn Fn() -> Result<(), GitError>,
    runner: &dyn CommandRunner,
    on_warning: &mut dyn FnMut(String),
) -> Result<HashMap<String, String>, GitError> {
    if candidates.is_empty() {
        return Ok(HashMap::new());
    }
    let mut seen_warnings = HashSet::new();
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
        PR_JSON_FIELDS,
    ];
    let out = match runner.run("gh", &args, repo_root) {
        Ok(out) => out,
        Err(err) => {
            emit_once(
                &mut seen_warnings,
                on_warning,
                format!("gh pr list failed: {err}"),
            );
            return Ok(HashMap::new());
        }
    };
    if !out.success() {
        emit_once(
            &mut seen_warnings,
            on_warning,
            format!("gh pr list failed: {}", out.stderr.trim()),
        );
        return Ok(HashMap::new());
    }
    let rows = match serde_json::from_str::<Vec<MergedPrRow>>(&out.stdout) {
        Ok(rows) => rows,
        Err(err) => {
            emit_once(
                &mut seen_warnings,
                on_warning,
                format!("gh pr list invalid JSON: {err}"),
            );
            return Ok(HashMap::new());
        }
    };
    let saturated = rows.len() >= BULK_PR_LIMIT;

    let by_ref: HashMap<&str, &str> = candidates
        .iter()
        .map(|(branch, oid)| (branch.as_str(), oid.as_str()))
        .collect();

    let mut matched: HashMap<String, (String, String)> = HashMap::new();
    for row in rows {
        let Some(&local_head_oid) = by_ref.get(row.head_ref_name.as_str()) else {
            continue;
        };
        let expected_head = row.head_ref_name.clone();
        if let Some(merge_oid) = match_row(row, base_branch, &expected_head, local_head_oid) {
            matched.insert(expected_head, (local_head_oid.to_string(), merge_oid));
        }
    }

    if saturated {
        for (head_branch, local_head_oid) in candidates {
            if matched.contains_key(head_branch) {
                continue;
            }
            let per_args = [
                "pr",
                "list",
                "--head",
                head_branch.as_str(),
                "--base",
                base_branch,
                "--state",
                "merged",
                "--limit",
                "5",
                "--json",
                PR_JSON_FIELDS,
            ];
            let per_out = match runner.run("gh", &per_args, repo_root) {
                Ok(out) => out,
                Err(err) => {
                    emit_once(
                        &mut seen_warnings,
                        on_warning,
                        format!("gh pr list fallback failed: {err}"),
                    );
                    break;
                }
            };
            if !per_out.success() {
                let stderr = per_out.stderr.trim();
                emit_once(
                    &mut seen_warnings,
                    on_warning,
                    format!("gh pr list fallback failed: {stderr}"),
                );
                if is_gh_auth_or_rate_limit(stderr) {
                    break;
                }
                continue;
            }
            let Ok(per_rows) = serde_json::from_str::<Vec<MergedPrRow>>(&per_out.stdout) else {
                continue;
            };
            for row in per_rows {
                if let Some(merge_oid) = match_row(row, base_branch, head_branch, local_head_oid) {
                    matched.insert(head_branch.clone(), (local_head_oid.clone(), merge_oid));
                    break;
                }
            }
        }
    }

    if matched.is_empty() {
        return Ok(HashMap::new());
    }
    if fetch_base().is_err() {
        return Ok(HashMap::new());
    }

    let mut verified = HashMap::new();
    for (branch_name, (local_head_oid, merge_oid)) in matched {
        let ok = git_raw(
            repo_root,
            &["merge-base", "--is-ancestor", &merge_oid, base_start],
        )
        .is_ok_and(|res| res.code == Some(0));
        if ok {
            verified.insert(branch_name, local_head_oid);
        }
    }
    Ok(verified)
}
