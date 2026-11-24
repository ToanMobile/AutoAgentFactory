//! Enforces the `🤖 aaf-bot:` marker on agent-authored GitHub writes across
//! `gh` CLI subcommands, `gh api`, `gh alias`, HTTP clients / interpreters
//! targeting GitHub APIs, and GitHub MCP tools.

mod flags;
mod http;
mod mcp;

use crate::{Denial, pr_merge_guard, shell};

pub const RULE: &str = "gh_bot_marker_guard";
pub const BOT_MARKER: &str = "🤖 aaf-bot:";

const FIX: &str = "prefix the GitHub body with `🤖 aaf-bot: ` (e.g. `--body '🤖 aaf-bot: ...'` or `--body-file - <<'EOF'` with `🤖 aaf-bot: ...` on the first line)";

pub use mcp::check_mcp;
pub(crate) use mcp::is_github_mcp_tool;

/// True when `body` begins with [`BOT_MARKER`] after optional leading whitespace.
pub fn has_bot_marker(body: &str) -> bool {
    body.trim_start().starts_with(BOT_MARKER)
}

pub(super) fn deny(reason: impl Into<String>) -> Denial {
    Denial::new(RULE, reason, FIX)
}

/// Checks one simple command for unlabelled GitHub writes.
pub fn check(cmd: &shell::Cmd) -> Option<Denial> {
    if pr_merge_guard::is_gh(cmd) {
        let args = cmd.args();
        let arg_dynamic = cmd.arg_dynamic.get(1..).unwrap_or(&[]);
        let arg_cat_heredoc = cmd.arg_cat_heredoc.get(1..).unwrap_or(&[]);
        return check_gh_args(args, arg_dynamic, arg_cat_heredoc, cmd, false);
    }
    http::check_http(cmd)
}

/// Checks `gh` arguments (aligned with `arg_dynamic` and `arg_cat_heredoc`).
///
/// For normal commands, `args` drops `words[0]` (`gh`); for alias expansions,
/// `args` is the full `parsed.words` and `in_alias` is `true`, in which case
/// any write subcommand or write/GraphQL `api` endpoint is denied immediately.
fn check_gh_args(
    args: &[String],
    arg_dynamic: &[bool],
    arg_cat_heredoc: &[Option<String>],
    cmd: &shell::Cmd,
    in_alias: bool,
) -> Option<Denial> {
    let sub1_idx = find_sub1(args)?;
    let sub1 = args[sub1_idx].as_str();

    match sub1 {
        "alias" if !in_alias => check_alias(&args[sub1_idx + 1..], cmd),
        "api" => check_api(
            &args[sub1_idx + 1..],
            arg_dynamic.get(sub1_idx + 1..).unwrap_or(&[]),
            cmd,
            in_alias,
        ),
        "pr" | "issue" => {
            let after_sub1 = &args[sub1_idx + 1..];
            let sub2_rel = match find_sub2(after_sub1) {
                Sub2Lookup::Found(rel) => rel,
                Sub2Lookup::AmbiguousWrite => {
                    return Some(deny(format!(
                        "flags before `gh {sub1}` subcommand make body inspection ambiguous"
                    )));
                }
                Sub2Lookup::None => return None,
            };
            let sub2 = after_sub1[sub2_rel].as_str();
            let write_sub = flags::WriteSub::parse(sub1, sub2)?;
            if in_alias {
                return Some(deny(format!(
                    "gh alias targeting `gh {sub1} {sub2}` can bypass `{BOT_MARKER}` enforcement"
                )));
            }
            let tail_start = sub1_idx + 1 + sub2_rel + 1;
            check_write_sub(
                write_sub,
                args.get(tail_start..).unwrap_or(&[]),
                arg_dynamic.get(tail_start..).unwrap_or(&[]),
                arg_cat_heredoc.get(tail_start..).unwrap_or(&[]),
                cmd,
            )
        }
        _ => None,
    }
}

/// Skips global `gh` flags (`-R`/`--repo`, `-H`/`--hostname`, `-h`/`--help`)
/// before subcommand level 1.
fn find_sub1(args: &[String]) -> Option<usize> {
    let mut i = 0;
    while let Some(arg) = args.get(i).map(String::as_str) {
        if arg == "--" {
            return (i + 1 < args.len()).then_some(i + 1);
        }
        if let Some(step) = global_flag_step(arg) {
            i += step;
            continue;
        }
        if arg.starts_with('-') && arg.len() > 1 {
            return None;
        }
        return Some(i);
    }
    None
}

enum Sub2Lookup {
    Found(usize),
    AmbiguousWrite,
    None,
}

/// Skips global `gh` flags between `pr|issue` and subcommand level 2.
/// If a non-global flag appears before subcommand level 2 and any later token
/// names a write subcommand, returns [`Sub2Lookup::AmbiguousWrite`].
fn find_sub2(args: &[String]) -> Sub2Lookup {
    let mut i = 0;
    while let Some(arg) = args.get(i).map(String::as_str) {
        if arg == "--" {
            return if i + 1 < args.len() {
                Sub2Lookup::Found(i + 1)
            } else {
                Sub2Lookup::None
            };
        }
        if let Some(step) = global_flag_step(arg) {
            i += step;
            continue;
        }
        if arg.starts_with('-') && arg.len() > 1 {
            let has_write_later = args[i + 1..].iter().any(|w| {
                matches!(
                    w.as_str(),
                    "comment" | "create" | "edit" | "review" | "close" | "reopen"
                )
            });
            return if has_write_later {
                Sub2Lookup::AmbiguousWrite
            } else {
                Sub2Lookup::None
            };
        }
        return Sub2Lookup::Found(i);
    }
    Sub2Lookup::None
}

fn global_flag_step(arg: &str) -> Option<usize> {
    if matches!(arg, "-h" | "--help") {
        return Some(1);
    }
    if matches!(arg, "-R" | "--repo" | "-H" | "--hostname") {
        return Some(2);
    }
    if arg.starts_with("--repo=")
        || arg.starts_with("--hostname=")
        || (arg.starts_with("-R") && arg.len() > 2)
        || (arg.starts_with("-H") && arg.len() > 2)
    {
        return Some(1);
    }
    None
}

fn check_write_sub(
    sub: flags::WriteSub,
    args: &[String],
    arg_dynamic: &[bool],
    arg_cat_heredoc: &[Option<String>],
    cmd: &shell::Cmd,
) -> Option<Denial> {
    let parsed = match flags::parse_write_flags(sub, args) {
        Ok(parsed) => parsed,
        Err(reason) => return Some(deny(reason)),
    };

    if parsed.editor {
        return Some(deny(
            "interactive `--editor`/`-e` body cannot be verified for the bot marker",
        ));
    }
    if parsed.web {
        return Some(deny(
            "`--web`/`-w` opens a browser form whose body cannot be verified for the bot marker",
        ));
    }
    if sub == flags::WriteSub::PrReview && parsed.approve {
        return Some(deny(
            "agents must not approve pull requests (`gh pr review --approve`/`-a`)",
        ));
    }

    for source in &parsed.sources {
        match source {
            flags::BodySource::Text {
                value,
                val_idx,
                is_glued,
            } => {
                let is_dyn = arg_dynamic.get(*val_idx).copied().unwrap_or(true);
                if !is_dyn {
                    if !has_bot_marker(value) {
                        return Some(deny(format!(
                            "GitHub body is missing the required `{BOT_MARKER}` prefix"
                        )));
                    }
                } else if !is_glued
                    && let Some(body) = arg_cat_heredoc.get(*val_idx).and_then(Option::as_deref)
                {
                    if !has_bot_marker(body) {
                        return Some(deny(format!(
                            "heredoc body in `$(cat <<'...')` is missing the required `{BOT_MARKER}` prefix"
                        )));
                    }
                } else {
                    return Some(deny(
                        "dynamic GitHub body cannot be verified for the bot marker",
                    ));
                }
            }
            flags::BodySource::File { value, val_idx } => {
                let is_dyn = arg_dynamic.get(*val_idx).copied().unwrap_or(true);
                if is_dyn || value != "-" {
                    return Some(deny(
                        "`--body-file`/`-F` pointing to a file cannot be verified safely (use `--body-file -` with a literal heredoc/here-string)",
                    ));
                }
                if has_file_stdin_redirect(cmd) {
                    return Some(deny(
                        "`--body-file -` with file stdin redirection (`<`/`<>`) cannot be verified for the bot marker",
                    ));
                }
                let texts = pr_merge_guard::stdin_texts(cmd);
                if texts.is_empty() {
                    return Some(deny(
                        "`--body-file -` requires a literal heredoc, here-string, or piped echo/printf starting with the bot marker",
                    ));
                }
                if !texts.iter().all(|t| has_bot_marker(t)) {
                    return Some(deny(format!(
                        "`--body-file -` stdin body is missing the required `{BOT_MARKER}` prefix"
                    )));
                }
            }
        }
    }

    if parsed.sources.is_empty() {
        match sub {
            flags::WriteSub::PrCreate | flags::WriteSub::IssueCreate => {
                return Some(deny(
                    "creating a PR/issue requires an explicit `--body`/`--body-file -` with the bot marker (do not rely on `--fill`, templates, or interactive prompts)",
                ));
            }
            flags::WriteSub::PrReview => {
                return Some(deny(
                    "`gh pr review` requires an explicit `--body`/`--body-file -` with the bot marker",
                ));
            }
            flags::WriteSub::PrComment | flags::WriteSub::IssueComment if !parsed.delete_last => {
                return Some(deny(
                    "commenting requires an explicit `--body`/`--body-file -` with the bot marker",
                ));
            }
            _ => {}
        }
    }

    None
}

fn has_file_stdin_redirect(cmd: &shell::Cmd) -> bool {
    cmd.redirects.iter().any(|r| r.op == "<" || r.op == "<>")
}

fn check_api(
    args: &[String],
    arg_dynamic: &[bool],
    cmd: &shell::Cmd,
    in_alias: bool,
) -> Option<Denial> {
    let mut method: Option<String> = None;
    let mut has_implicit_post = false;
    let mut input_values: Vec<(String, bool)> = Vec::new();
    let mut typed_fields: Vec<String> = Vec::new();
    let mut positionals: Vec<&str> = Vec::new();

    let mut i = 0;
    while let Some(arg) = args.get(i).map(String::as_str) {
        if arg == "--" {
            positionals.extend(args[i + 1..].iter().map(String::as_str));
            break;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, glued) = long
                .split_once('=')
                .map_or((long, None), |(n, v)| (n, Some(v)));
            match name {
                "method" => {
                    if let Some(val) = glued.or_else(|| args.get(i + 1).map(String::as_str)) {
                        method = Some(val.to_string());
                    }
                    i += if glued.is_some() { 1 } else { 2 };
                }
                "input" => {
                    has_implicit_post = true;
                    if let Some(val) = glued {
                        let is_dyn = arg_dynamic.get(i).copied().unwrap_or(true);
                        input_values.push((val.to_string(), is_dyn));
                        i += 1;
                    } else if let Some(val) = args.get(i + 1) {
                        let is_dyn = arg_dynamic.get(i + 1).copied().unwrap_or(true);
                        input_values.push((val.clone(), is_dyn));
                        i += 2;
                    } else {
                        input_values.push((String::new(), true));
                        i += 1;
                    }
                }
                "field" => {
                    has_implicit_post = true;
                    if let Some(val) = glued.or_else(|| args.get(i + 1).map(String::as_str)) {
                        typed_fields.push(val.to_string());
                    }
                    i += if glued.is_some() { 1 } else { 2 };
                }
                "raw-field" => {
                    has_implicit_post = true;
                    i += if glued.is_some() { 1 } else { 2 };
                }
                "cache" | "header" | "hostname" | "jq" | "preview" | "template" => {
                    i += if glued.is_some() { 1 } else { 2 };
                }
                _ => {
                    i += 1;
                }
            }
            continue;
        }
        if arg.starts_with('-') && arg.len() > 1 {
            let cluster = &arg[1..];
            let mut step = 1;
            for (offset, ch) in cluster.char_indices() {
                let rem = &cluster[offset + ch.len_utf8()..];
                let rem_val = rem.strip_prefix('=').unwrap_or(rem);
                match ch {
                    'X' => {
                        if !rem_val.is_empty() {
                            method = Some(rem_val.to_string());
                        } else if let Some(val) = args.get(i + 1) {
                            method = Some(val.clone());
                            step = 2;
                        }
                        break;
                    }
                    'F' => {
                        has_implicit_post = true;
                        if !rem_val.is_empty() {
                            typed_fields.push(rem_val.to_string());
                        } else if let Some(val) = args.get(i + 1) {
                            typed_fields.push(val.clone());
                            step = 2;
                        }
                        break;
                    }
                    'f' => {
                        has_implicit_post = true;
                        if rem_val.is_empty() {
                            step = 2;
                        }
                        break;
                    }
                    'H' | 'q' | 'p' | 't' => {
                        if rem_val.is_empty() {
                            step = 2;
                        }
                        break;
                    }
                    _ => {}
                }
            }
            i += step;
            continue;
        }
        positionals.push(arg);
        i += 1;
    }

    let mut is_graphql = false;
    let mut has_write_endpoint = false;
    for endpoint in &positionals {
        let norm = pr_merge_guard::normalize_endpoint(endpoint);
        if norm.rsplit('/').next() == Some("graphql") {
            is_graphql = true;
        }
        if norm.split('/').any(|seg| {
            matches!(
                seg,
                "issues" | "pulls" | "comments" | "reviews" | "discussions" | "notes"
            )
        }) {
            has_write_endpoint = true;
        }
    }

    if in_alias {
        if has_write_endpoint || is_graphql {
            return Some(deny(format!(
                "gh alias targeting `gh api` write/GraphQL endpoint can bypass `{BOT_MARKER}` enforcement"
            )));
        }
        return None;
    }

    if is_graphql {
        for (val, is_dyn) in &input_values {
            if *is_dyn || val != "-" {
                return Some(deny(
                    "`gh api graphql --input <file>` cannot be verified for mutations",
                ));
            }
            if has_file_stdin_redirect(cmd) || pr_merge_guard::stdin_texts(cmd).is_empty() {
                return Some(deny(
                    "`gh api graphql --input -` without literal stdin cannot be verified for mutations",
                ));
            }
        }
        if typed_fields.iter().any(|field| {
            field
                .split_once('=')
                .is_some_and(|(_, rhs)| rhs.starts_with('@'))
                || field.starts_with('@')
        }) {
            return Some(deny(
                "`gh api graphql -F/--field` reading from `@file` cannot be verified for mutations",
            ));
        }
        let has_mutation = |text: &str| text.to_ascii_lowercase().contains("mutation");
        if args.iter().any(|a| has_mutation(a))
            || pr_merge_guard::stdin_texts(cmd)
                .iter()
                .any(|t| has_mutation(t))
        {
            return Some(deny(
                "GraphQL mutations via `gh api graphql` bypass bot marker enforcement",
            ));
        }
        return None;
    }

    let is_read_only = match method.as_deref() {
        Some(m) => m.eq_ignore_ascii_case("GET") || m.eq_ignore_ascii_case("HEAD"),
        None => !has_implicit_post,
    };
    if has_write_endpoint && !is_read_only {
        return Some(deny(
            "direct `gh api` writes to issues/pulls/comments/reviews/discussions/notes bypass bot marker enforcement",
        ));
    }

    None
}

fn check_alias(args: &[String], cmd: &shell::Cmd) -> Option<Denial> {
    let mut positionals: Vec<&str> = Vec::new();
    let mut shell_alias = false;
    let mut after_dd = false;
    for arg in args {
        if !after_dd && arg == "--" {
            after_dd = true;
            continue;
        }
        if !after_dd && arg.starts_with('-') && arg.len() > 1 {
            if arg == "--shell" || (!arg.starts_with("--") && arg[1..].contains('s')) {
                shell_alias = true;
            }
            continue;
        }
        positionals.push(arg.as_str());
    }

    match positionals.first().copied() {
        Some("import") => Some(deny(
            "`gh alias import` can define aliases that bypass bot marker enforcement",
        )),
        Some("set") => {
            if shell_alias {
                return Some(deny(
                    "`gh alias set --shell` can bypass bot marker enforcement",
                ));
            }
            let expansions: Vec<String> = match positionals.get(2).copied() {
                Some("-") | None => pr_merge_guard::stdin_texts(cmd),
                Some(text) => vec![text.to_string()],
            };
            for text in &expansions {
                if text.trim_start().starts_with('!') {
                    return Some(deny(
                        "shell alias expansion (`!`) can bypass bot marker enforcement",
                    ));
                }
                let script = shell::parse(text);
                if !script.opaque.is_empty() {
                    return Some(deny(
                        "unparsable `gh alias set` expansion cannot be verified for GitHub writes",
                    ));
                }
                for parsed in &script.cmds {
                    if let Some(denial) = check_gh_args(
                        &parsed.words,
                        &parsed.arg_dynamic,
                        &parsed.arg_cat_heredoc,
                        parsed,
                        true,
                    ) {
                        return Some(denial);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bot_marker_allows_leading_whitespace_only() {
        assert!(has_bot_marker("🤖 aaf-bot: hello"));
        assert!(has_bot_marker("  \n\t🤖 aaf-bot: hello"));
        assert!(!has_bot_marker("fix 🤖 aaf-bot: hello"));
        assert!(!has_bot_marker("aaf-bot: missing emoji"));
        assert!(!has_bot_marker(""));
    }

    #[test]
    fn glued_and_ambiguous_flags_unit_checks() {
        let parse_and_check = |line: &str| {
            let script = shell::parse(line);
            script.cmds.iter().find_map(check)
        };
        // Glued body with marker -> allow
        assert!(parse_and_check("gh pr comment 5 -b'🤖 aaf-bot: ok'").is_none());
        // Glued body without marker -> deny
        assert!(parse_and_check("gh pr comment 5 -bno").is_some());
        // -f is boolean on pr create (no body -> deny), valued on api
        assert!(parse_and_check("gh pr create -f -t title").is_some());
        assert!(parse_and_check("gh pr create -f -t title -b '🤖 aaf-bot: ok'").is_none());
        // -a is valued on pr/issue create, boolean approve on pr review, unknown on edit
        assert!(parse_and_check("gh pr create -a @me -t t -b '🤖 aaf-bot: ok'").is_none());
        assert!(parse_and_check("gh issue create -a @me -t t -b '🤖 aaf-bot: ok'").is_none());
        assert!(parse_and_check("gh pr review 5 -a -b '🤖 aaf-bot: ok'").is_some());
        assert!(parse_and_check("gh pr edit 5 -a @me -b '🤖 aaf-bot: ok'").is_some());
        // -r is boolean on pr review, valued on pr create and issue close
        assert!(parse_and_check("gh pr review 5 -r -b '🤖 aaf-bot: ok'").is_none());
        assert!(parse_and_check("gh pr create -r alice -t t -b '🤖 aaf-bot: ok'").is_none());
        assert!(parse_and_check("gh issue close 5 -r completed").is_none());
    }
}
