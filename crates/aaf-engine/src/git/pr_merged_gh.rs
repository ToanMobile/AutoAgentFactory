//! Fail-closed GitHub PR merge verification for `Review` tasks that have a
//! recorded `PrOpened` URL (including PRs without a local worktree or whose
//! remote head branch does not match `aaf/<task_id>`).

use std::path::Path;

use serde::Deserialize;

use super::GitError;
use super::pr_feedback_gh::PrRef;
use crate::process::CommandRunner;

/// Verified metadata for a pull request confirmed `MERGED` into the base ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedPrMerge {
    pub pr_number: u64,
    pub url: String,
    pub head_oid: String,
    pub merge_commit_oid: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct GhMergeCommit {
    oid: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct GhMergedPrView {
    number: u64,
    url: String,
    state: String,
    base_ref_name: String,
    head_ref_oid: String,
    is_cross_repository: bool,
    merge_commit: Option<GhMergeCommit>,
}

fn is_hex_sha40(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Checks whether `pr` is `MERGED` into `base_branch` on GitHub and its merge
/// commit is an ancestor of `base_ref` in `repo_root`.
///
/// Fails closed (`Ok(None)`) when the PR is not merged, comes from a fork,
/// targets a different base branch, has a mismatched URL/number, lacks a valid
/// 40-char merge commit SHA, or the merge commit is not yet reachable from
/// `base_ref`.
pub fn verify_merged_pr(
    runner: &dyn CommandRunner,
    repo_root: &Path,
    pr: &PrRef,
    base_branch: &str,
    base_ref: &str,
) -> Result<Option<VerifiedPrMerge>, GitError> {
    let pr_num = pr.number.to_string();
    let repo_flag = pr.repo_flag();
    let args = [
        "pr",
        "view",
        &pr_num,
        "--repo",
        &repo_flag,
        "--json",
        "number,url,state,baseRefName,headRefOid,isCrossRepository,mergeCommit",
    ];
    let out = runner.run("gh", &args, repo_root)?;
    if !out.success() {
        let stderr = out.stderr.trim();
        let stdout = out.stdout.trim();
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        return Err(GitError::Command {
            command: format!("gh {}", args.join(" ")),
            code: out.exit_code,
            stderr: detail.to_string(),
        });
    }
    let view: GhMergedPrView = serde_json::from_str(out.stdout.trim())
        .map_err(|e| GitError::Invalid(format!("unparsable gh pr view JSON: {e}")))?;

    if !view.state.eq_ignore_ascii_case("MERGED") {
        return Ok(None);
    }
    if view.is_cross_repository || view.number != pr.number {
        return Ok(None);
    }
    let view_url = view.url.trim().trim_end_matches('/');
    let expected_url = pr.url.trim().trim_end_matches('/');
    if view_url != expected_url {
        return Ok(None);
    }
    if view.base_ref_name != base_branch {
        return Ok(None);
    }
    let Some(merge_oid) = view
        .merge_commit
        .as_ref()
        .map(|c| c.oid.trim())
        .filter(|s| is_hex_sha40(s))
    else {
        return Ok(None);
    };

    let anc = runner.run(
        "git",
        &["merge-base", "--is-ancestor", merge_oid, base_ref],
        repo_root,
    )?;
    if !anc.success() {
        return Ok(None);
    }

    let head_oid = if is_hex_sha40(view.head_ref_oid.trim()) {
        view.head_ref_oid.trim().to_string()
    } else {
        merge_oid.to_string()
    };

    Ok(Some(VerifiedPrMerge {
        pr_number: view.number,
        url: view_url.to_string(),
        head_oid,
        merge_commit_oid: merge_oid.to_string(),
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::VecDeque;
    use std::io;
    use std::sync::Mutex;

    use super::*;
    use crate::git::pr_feedback_gh::parse_pr_url;
    use crate::process::CommandOutput;

    const HEAD_SHA: &str = "1111111111111111111111111111111111111111";
    const MERGE_SHA: &str = "2222222222222222222222222222222222222222";
    const PR_URL: &str = "https://github.com/acme/app/pull/455";

    struct FakeRunner(Mutex<VecDeque<io::Result<CommandOutput>>>);

    impl FakeRunner {
        fn new(responses: Vec<CommandOutput>) -> Self {
            Self(Mutex::new(responses.into_iter().map(Ok).collect()))
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, _program: &str, _args: &[&str], _cwd: &Path) -> io::Result<CommandOutput> {
            self.0.lock().unwrap().pop_front().unwrap()
        }
    }

    fn ok(stdout: &str) -> CommandOutput {
        CommandOutput {
            exit_code: Some(0),
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    fn fail(code: i32, stderr: &str) -> CommandOutput {
        CommandOutput {
            exit_code: Some(code),
            stdout: String::new(),
            stderr: stderr.to_string(),
        }
    }

    fn merged_json(state: &str, base: &str, fork: bool) -> String {
        format!(
            r#"{{"number":455,"url":"{PR_URL}","state":"{state}","baseRefName":"{base}","headRefOid":"{HEAD_SHA}","isCrossRepository":{fork},"mergeCommit":{{"oid":"{MERGE_SHA}"}}}}"#
        )
    }

    #[test]
    fn accepts_verified_merged_pr_in_base_ancestry() {
        let pr = parse_pr_url(PR_URL).unwrap();
        let runner = FakeRunner::new(vec![ok(&merged_json("MERGED", "main", false)), ok("")]);
        let res = verify_merged_pr(&runner, Path::new("/tmp"), &pr, "main", "origin/main").unwrap();
        assert_eq!(
            res,
            Some(VerifiedPrMerge {
                pr_number: 455,
                url: PR_URL.to_string(),
                head_oid: HEAD_SHA.to_string(),
                merge_commit_oid: MERGE_SHA.to_string(),
            })
        );
    }

    #[test]
    fn rejects_open_wrong_base_fork_or_non_ancestor() {
        let pr = parse_pr_url(PR_URL).unwrap();

        let open = FakeRunner::new(vec![ok(&merged_json("OPEN", "main", false))]);
        assert_eq!(
            verify_merged_pr(&open, Path::new("/tmp"), &pr, "main", "origin/main").unwrap(),
            None
        );

        let wrong_base = FakeRunner::new(vec![ok(&merged_json("MERGED", "develop", false))]);
        assert_eq!(
            verify_merged_pr(&wrong_base, Path::new("/tmp"), &pr, "main", "origin/main").unwrap(),
            None
        );

        let fork = FakeRunner::new(vec![ok(&merged_json("MERGED", "main", true))]);
        assert_eq!(
            verify_merged_pr(&fork, Path::new("/tmp"), &pr, "main", "origin/main").unwrap(),
            None
        );

        let not_anc = FakeRunner::new(vec![ok(&merged_json("MERGED", "main", false)), fail(1, "")]);
        assert_eq!(
            verify_merged_pr(&not_anc, Path::new("/tmp"), &pr, "main", "origin/main").unwrap(),
            None
        );
    }
}
