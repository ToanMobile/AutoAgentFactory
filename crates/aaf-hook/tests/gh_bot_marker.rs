#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Corpus tests for `gh_bot_marker_guard`: enforces the `🤖 aaf-bot:` marker
//! on every agent-authored GitHub write across `gh`, HTTP clients,
//! interpreters, `gh alias`, and GitHub MCP tools.

use aaf_hook::host::{HookHost, decide_for_host};
use aaf_hook::{Decision, HookEnv, decide};
use serde_json::{Value, json};

const RULE: &str = "gh_bot_marker_guard";

fn env() -> HookEnv<'static> {
    HookEnv {
        repo_root: &|_| None,
        git_config: &|_| None,
        data_dir: None,
        config_dir: None,
        home: None,
        cwd: None,
        in_factory_run: false,
        trusted_aaf_bins: Vec::new(),
    }
}

fn check(command: &str) -> Decision {
    let payload = json!({
        "tool_name": "Bash",
        "tool_input": { "command": command },
    });
    decide(payload.to_string().as_bytes(), &env())
}

fn check_mcp_all_paths(payload: &Value) -> (Decision, Decision, Decision) {
    let raw = payload.to_string();
    let e = env();
    let d_lib = decide(raw.as_bytes(), &e);
    let (d_claude, _) = decide_for_host(raw.as_bytes(), &e, HookHost::Claude);
    let (d_auto, _) = decide_for_host(raw.as_bytes(), &e, HookHost::Auto);
    (d_lib, d_claude, d_auto)
}

#[test]
fn gh_writes_without_bot_marker_are_denied() {
    let cases = [
        "gh pr comment 5 --body 'fixed'",
        "gh pr comment 5 -b fixed",
        "gh issue comment 3 --body='x'",
        "gh pr create --title t --body 'x'",
        "gh pr create --fill",
        "gh pr create -f",
        "gh pr create --title t",
        "gh pr edit 5 --body 'x'",
        "gh pr review 5 --comment -b 'x'",
        "gh pr review 5",
        "gh pr review 5 --comment",
        "gh pr review 5 --approve",
        "gh pr review 5 --approve -b '🤖 aaf-bot: lgtm'",
        "gh pr review 5 -a -b '🤖 aaf-bot: lgtm'",
        "gh pr close 5 --comment 'x'",
        "gh pr close 5 -c 'x'",
        "gh pr close 5 -cx",
        "gh pr reopen 5 --comment 'x'",
        "gh issue close 5 --comment 'x'",
        "gh pr comment 5 --body-file notes.md",
        "gh pr comment 5 -Fnotes.md",
        "gh pr comment 5 --body-file - < notes.md",
        "gh pr comment 5 --body-file - <> notes.md",
        "gh pr comment 5 --body-file - <<< '🤖 aaf-bot: x' < notes.md",
        "cat notes.md | gh pr comment 5 --body-file -",
        "gh pr comment 5 --body \"$MSG\"",
        "gh pr comment 5 --editor",
        "gh pr comment 5 -e",
        "gh pr comment 5 --web",
        "gh pr comment 5",
        "gh -R o/r pr comment 5 --body x",
        "/opt/homebrew/bin/gh pr comment 5 -b x",
        "gh pr --body x comment 5",
        "gh pr create --body \"$(cat <<'EOF'\nfixed\nEOF\n)\"",
        "gh pr create --body \"$(cat <<EOF\n🤖 aaf-bot: x\nEOF\n)\"",
        "gh pr create --body \"`cat <<'EOF'\n🤖 aaf-bot: x\nEOF\n`\"",
        "gh pr create --body \"prefix $(cat <<'EOF'\n🤖 aaf-bot: x\nEOF\n)\"",
        "gh pr create --body \"$(cat <<'EOF'\n🤖 aaf-bot: x\nEOF\n)suffix\"",
        "gh pr comment 5 --body-file - <<'EOF'\ndon't run git push --force; AAF_DATA_DIR stays closed\nEOF\n",
        "gh pr comment 5 -b 'fix 🤖 aaf-bot:'",
        // Short flag clusters and unknown flags (V3-5 / v4)
        "gh pr comment 5 -b '🤖 aaf-bot: x' -ew",
        "gh pr create -t x -b '🤖 aaf-bot: x' -de",
        "gh pr edit 5 -a @me -b '🤖 aaf-bot: x'",
        "gh issue edit 5 -a @me -b '🤖 aaf-bot: x'",
        "gh pr comment 5 --unknown-flag -b '🤖 aaf-bot: x'",
        // gh api REST & GraphQL
        "gh api repos/o/r/issues/5/comments -f body=x",
        "gh api repos/o/r/issues/5/comments -fbody=x",
        "gh api -X PATCH repos/o/r/issues/comments/9 -f body=x",
        "gh api -XPOST repos/o/r/issues/5/comments",
        "gh api --method=POST /repos/o/r/pulls/5/reviews --input r.json",
        "gh api graphql -f query='mutation { addComment(input:{subjectId:\"x\", body:\"y\"}) { clientMutationId } }'",
        "gh api graphql --input q.json",
        "gh api graphql --input - < q.json",
        "gh api graphql --input -",
        "gh api graphql -F query=@q.graphql",
        "gh api graphql --field query=@q.graphql",
        "gh api graphql -Fquery=@q.graphql",
        // HTTP clients and interpreters
        "curl -X POST https://api.github.com/repos/o/r/issues/5/comments -d '{\"body\":\"x\"}'",
        "curl -d @b.json https://api.github.com/repos/o/r/issues/5/comments",
        "curl -X POST https://uploads.github.com/repos/o/r/releases/1/assets -T a.bin",
        "wget --post-data='{\"body\":\"x\"}' https://api.github.com/repos/o/r/issues/5/comments",
        "wget2 --post-data='{\"body\":\"x\"}' https://api.github.com/repos/o/r/issues/5/comments",
        "python3 -c \"import requests; requests.post('https://api.github.com/repos/o/r/issues/5/comments', json={'body':'x'})\"",
        "deno eval \"fetch('https://api.github.com/repos/o/r/issues/5/comments', {method:'POST'})\"",
        "bun -e \"fetch('https://api.github.com/repos/o/r/issues/5/comments', {method:'POST'})\"",
        "lwp-request -m POST https://api.github.com/repos/o/r/issues/5/comments",
        "http POST https://api.github.com/repos/o/r/issues/5/comments body=x",
        "xh https://api.github.com/repos/o/r/issues/5/comments body=x",
        // gh alias set targeting any write subcommand or api write endpoint
        "gh alias set cc 'pr comment'",
        "gh alias set cc 'pr comment -b \"🤖 aaf-bot: x\"'",
        "gh alias set pcr 'pr create'",
        "gh alias set rv 'pr review'",
        "gh alias set pe 'pr edit'",
        "gh alias set pc 'pr close'",
        "gh alias set apiw 'api repos/o/r/issues/1/comments -f body=x'",
        "gh alias set apiw 'api repos/o/r/issues/1/comments'",
        "gh alias set gq 'api graphql'",
    ];

    for cmd in cases {
        match check(cmd) {
            Decision::Deny(d) => {
                assert_eq!(d.rule, RULE, "unexpected rule for `{cmd}`: {d:?}");
            }
            Decision::Allow => panic!("expected DENY ({RULE}) for `{cmd}`"),
        }
    }

    // Shell alias is already refused by `pr_merge_guard`; assert DENY regardless of rule.
    assert!(
        check("gh alias set --shell x 'gh pr comment 1 -b y'").is_deny(),
        "expected DENY for shell alias"
    );
}

#[test]
fn github_mcp_writes_without_bot_marker_are_denied() {
    let cases = [
        json!({
            "tool_name": "mcp__github__add_issue_comment",
            "tool_input": { "body": "x" }
        }),
        json!({
            "tool_name": "mcp__github__create_pull_request_review",
            "tool_input": {}
        }),
        json!({
            "tool_name": "mcp__github__create_issue",
            "tool_input": { "title": "t" }
        }),
        json!({
            "tool_name": "mcp__github__create_pull_request",
            "tool_input": { "title": "t" }
        }),
        json!({
            "tool_name": "mcp__github__create_pull_request_review",
            "tool_input": { "event": "APPROVE", "body": "🤖 aaf-bot: lgtm" }
        }),
        json!({
            "tool_name": "mcp__github__submit_pending_pull_request_review",
            "tool_input": { "event": "approve", "body": "🤖 aaf-bot: lgtm" }
        }),
    ];

    for payload in &cases {
        let (d_lib, d_claude, d_auto) = check_mcp_all_paths(payload);
        for (label, d) in [("decide", d_lib), ("Claude", d_claude), ("Auto", d_auto)] {
            match d {
                Decision::Deny(denial) => {
                    assert_eq!(
                        denial.rule, RULE,
                        "unexpected rule via {label} for {payload}: {denial:?}"
                    );
                }
                Decision::Allow => panic!("expected DENY via {label} for {payload}"),
            }
        }
    }
}

#[test]
fn marked_gh_writes_and_read_only_commands_are_allowed() {
    let cases = [
        "gh pr comment 5 --body '🤖 aaf-bot: fixed'",
        "gh pr comment 5 -b '🤖 aaf-bot: x'",
        "gh pr comment 5 -b'🤖 aaf-bot: x'",
        "gh pr comment 5 --body '  \n🤖 aaf-bot: leading whitespace'",
        "gh issue comment 3 --body='🤖 aaf-bot: x'",
        "gh pr create --title t --body '🤖 aaf-bot: x'",
        "gh pr create --title t --body \"$(cat <<'EOF'\n🤖 aaf-bot: x\nmore\nEOF\n)\"",
        "gh pr create --title t --body $(cat <<'EOF'\n🤖 aaf-bot: x\nEOF\n)",
        "gh pr create --title t --body \"$(cat <<\"EOF\"\n🤖 aaf-bot: x\nEOF\n)\"",
        "gh pr comment 5 --body-file - <<'EOF'\n🤖 aaf-bot: x\nEOF\n",
        "gh pr comment 5 --body-file - <<'EOF'\n🤖 aaf-bot: don't run git push --force; AAF_DATA_DIR stays closed\nEOF\n",
        "gh pr review 5 -c --body-file - <<'EOF'\n🤖 aaf-bot: don't git push --force\nEOF\n",
        "gh pr comment 5 --body-file - <<< '🤖 aaf-bot: x'",
        "gh pr review 5 --request-changes -b '🤖 aaf-bot: x'",
        "gh pr review 5 -r -b '🤖 aaf-bot: x'",
        "gh issue create -a @me -t x -b '🤖 aaf-bot: y'",
        "gh issue close 5 -r completed",
        "gh pr close 5 --comment '🤖 aaf-bot: done'",
        "gh pr close 5",
        "gh pr edit 5 --title new --add-label bug",
        "gh pr comment 5 --delete-last --yes",
        "gh pr view 5 --comments",
        "gh pr list",
        "gh pr checks 5",
        "gh pr diff 5",
        "gh api repos/o/r/issues/5/comments",
        "gh api repos/o/r/pulls/5 --jq .head.sha",
        "gh api graphql -f query='query { viewer { login } }'",
        "echo '{\"query\":\"query { viewer { login } }\"}' | gh api graphql --input -",
        "curl https://api.github.com/repos/o/r/issues/5/comments",
        "curl -X POST https://example.com/x -d y",
        "wget https://api.github.com/repos/o/r/issues/5/comments",
        "lwp-request -m GET https://api.github.com/repos/o/r/issues/5/comments",
        "http GET https://api.github.com/repos/o/r/issues/5/comments",
    ];

    for cmd in cases {
        assert_eq!(check(cmd), Decision::Allow, "expected ALLOW for `{cmd}`");
    }

    let mcp_allow = [
        json!({
            "tool_name": "mcp__github__get_issue",
            "tool_input": { "issue_number": 1 }
        }),
        json!({
            "tool_name": "mcp__github__list_pull_requests",
            "tool_input": {}
        }),
        json!({
            "tool_name": "mcp__github__add_issue_comment",
            "tool_input": { "body": "🤖 aaf-bot: x" }
        }),
        json!({
            "tool_name": "mcp__github__update_issue",
            "tool_input": { "labels": ["bug"] }
        }),
        json!({
            "tool_name": "mcp__github__create_pull_request_review",
            "tool_input": { "event": "COMMENT", "body": "🤖 aaf-bot: note" }
        }),
    ];
    for payload in &mcp_allow {
        let (d_lib, d_claude, d_auto) = check_mcp_all_paths(payload);
        for (label, d) in [("decide", d_lib), ("Claude", d_claude), ("Auto", d_auto)] {
            assert_eq!(
                d,
                Decision::Allow,
                "expected ALLOW via {label} for {payload}"
            );
        }
    }
}
