//! Blocks direct GitHub PR merges (`gh pr merge`, `gh api .../pulls/.../merge`,
//! and GraphQL `mergePullRequest`) so every PR passes through the native
//! merge gate (`aaf merge <PR> --expect-head <sha40>`).

use crate::{Denial, shell};

pub const RULE: &str = "pr_merge_guard";

const FIX: &str = "use `aaf merge <PR> --expect-head <sha40>` instead of merging directly with gh";

/// Flags of `gh` that take one following argument before the subcommand.
fn takes_value(flag: &str) -> bool {
    matches!(
        flag,
        "-R" | "--repo"
            | "-H"
            | "--hostname"
            | "-X"
            | "--method"
            | "-f"
            | "-F"
            | "--field"
            | "--raw-field"
            | "--header"
            | "--jq"
            | "-q"
            | "--template"
            | "-t"
            | "--input"
            | "-p"
            | "--preview"
            | "--cache"
    )
}

fn normalize_endpoint(endpoint: &str) -> String {
    let without_query = endpoint.split(['?', '#']).next().unwrap_or(endpoint);
    without_query.trim_end_matches('/').to_ascii_lowercase()
}

fn is_gh(cmd: &shell::Cmd) -> bool {
    let name = cmd.name();
    name == "gh" || name.ends_with("/gh")
}

/// Checks if a `gh` invocation directly merges a PR via `gh pr merge` or
/// `gh api` (`/pulls/<n>/merge` or GraphQL `mergePullRequest`).
pub fn check(cmd: &shell::Cmd) -> Option<Denial> {
    if !is_gh(cmd) {
        return None;
    }
    let args = cmd.args();
    // Collect positional tokens, skipping flags and their option values.
    let mut positionals: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            positionals.extend(args[i + 1..].iter().map(String::as_str));
            break;
        }
        if arg.starts_with('-') {
            if !arg.contains('=') && takes_value(arg) {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        positionals.push(arg);
        i += 1;
    }

    let first = *positionals.first()?;
    if first == "pr" && positionals.get(1).copied() == Some("merge") {
        return Some(Denial::new(
            RULE,
            "direct `gh pr merge` bypasses the locked merge gate and head-lock checks",
            FIX,
        ));
    }

    if first == "api" {
        let mut is_graphql = false;
        for &endpoint in positionals.iter().skip(1) {
            let norm = normalize_endpoint(endpoint);
            let segs: Vec<&str> = norm.split('/').filter(|s| !s.is_empty()).collect();
            if matches!(segs.as_slice(), [.., "pulls", _, "merge"]) {
                return Some(Denial::new(
                    RULE,
                    format!("direct `gh api {endpoint}` bypasses the locked merge gate"),
                    FIX,
                ));
            }
            is_graphql |= segs.last() == Some(&"graphql");
        }
        if is_graphql {
            let has_merge = |text: &str| text.to_ascii_lowercase().contains("mergepullrequest");
            let in_args = args.iter().any(|a| has_merge(a));
            let in_stdin = shell::stdin_scripts(&cmd.redirects, cmd.feeder.as_deref())
                .iter()
                .any(|s| has_merge(s));
            let in_heredocs = cmd.heredocs.iter().any(|h| has_merge(h));
            if in_args || in_stdin || in_heredocs {
                return Some(Denial::new(
                    RULE,
                    "direct GraphQL `mergePullRequest` bypasses the locked merge gate",
                    FIX,
                ));
            }
        }
    }

    None
}
