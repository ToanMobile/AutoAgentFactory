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
            | "-h"
            | "--jq"
            | "-q"
            | "--template"
            | "-t"
            | "--input"
    )
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
        let endpoint = positionals.get(1).copied().unwrap_or("");
        let lower_ep = endpoint.to_ascii_lowercase();
        let is_rest_merge = lower_ep.contains("/pulls/") && lower_ep.ends_with("/merge")
            || lower_ep.starts_with("pulls/") && lower_ep.ends_with("/merge");
        if is_rest_merge {
            return Some(Denial::new(
                RULE,
                format!("direct `gh api {endpoint}` bypasses the locked merge gate"),
                FIX,
            ));
        }
        if lower_ep == "graphql"
            && args
                .iter()
                .any(|a| a.to_ascii_lowercase().contains("mergepullrequest"))
        {
            return Some(Denial::new(
                RULE,
                "direct GraphQL `mergePullRequest` bypasses the locked merge gate",
                FIX,
            ));
        }
    }

    None
}
