//! Verified squash-merge matching for the factory review sweep.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use crate::git::GitError;
use crate::git::cmd::git_raw;
use crate::git::worktree::BRANCH_PREFIX;
use crate::process::CommandRunner;

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
