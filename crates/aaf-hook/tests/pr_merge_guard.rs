#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Corpus tests for `pr_merge_guard`: blocks direct `gh pr merge` and
//! `gh api` PR merge bypasses while allowing read-only `gh pr` and `aaf merge`.

use aaf_hook::{Decision, HookEnv, decide};
use serde_json::json;

fn env() -> HookEnv<'static> {
    HookEnv {
        repo_root: &|_| None,
        git_config: &|_| None,
        data_dir: None,
        config_dir: None,
        home: None,
        cwd: None,
    }
}

fn check(command: &str) -> Decision {
    let payload = json!({
        "tool_name": "Bash",
        "tool_input": { "command": command },
    });
    decide(payload.to_string().as_bytes(), &env())
}

#[test]
fn direct_gh_pr_merge_and_api_merge_are_denied() {
    for cmd in [
        "gh pr merge 563 --squash",
        "gh -R ToanMobile/OfficeReader pr merge 563",
        "gh --repo=ToanMobile/OfficeReader pr merge --auto 563",
        "/opt/homebrew/bin/gh pr merge 563",
        "gh api repos/ToanMobile/OfficeReader/pulls/563/merge -X PUT",
        "gh api -X PUT /repos/ToanMobile/OfficeReader/pulls/563/merge",
        "gh api graphql -f query='mutation { mergePullRequest(input: {pullRequestId: \"x\"}) { clientMutationId } }'",
    ] {
        match check(cmd) {
            Decision::Deny(d) => {
                assert_eq!(d.rule, "pr_merge_guard", "cmd={cmd}");
                assert!(d.fix.contains("aaf merge"), "cmd={cmd}, fix={}", d.fix);
            }
            Decision::Allow => panic!("expected DENY for `{cmd}`"),
        }
    }
}

#[test]
fn read_only_gh_and_aaf_merge_are_allowed() {
    for cmd in [
        "gh pr view 563 --json headRefOid",
        "gh pr checks 563",
        "gh pr list --state open",
        "gh api repos/ToanMobile/OfficeReader/pulls/563",
        "aaf merge 563 --expect-head 0123456789abcdef0123456789abcdef01234567",
    ] {
        assert_eq!(check(cmd), Decision::Allow, "expected ALLOW for `{cmd}`");
    }
}
