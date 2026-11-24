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

/// Lowercased, query and fragment cut, `%XX` decoded, and `.`/`..`
/// segments resolved the way an HTTP client or server does
/// (`…/pulls/12/./merge`, `…/pulls/12/x/../merge`).
pub(crate) fn normalize_endpoint(endpoint: &str) -> String {
    let without_query = endpoint.split(['?', '#']).next().unwrap_or(endpoint);
    let decoded = percent_decode(without_query).to_ascii_lowercase();
    let mut segs: Vec<&str> = Vec::new();
    for seg in decoded.split('/') {
        match seg {
            "." => {}
            ".." => {
                if segs
                    .last()
                    .is_some_and(|last| !last.is_empty() && !last.ends_with(':'))
                {
                    segs.pop();
                }
            }
            seg => segs.push(seg),
        }
    }
    segs.join("/").trim_end_matches('/').to_string()
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let hex = bytes
            .get(at + 1..at + 3)
            .and_then(|pair| std::str::from_utf8(pair).ok())
            .and_then(|pair| u8::from_str_radix(pair, 16).ok());
        match (bytes[at], hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                at += 3;
            }
            (byte, _) => {
                out.push(byte);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The REST merge endpoint: `…/pulls/<n>/merge`.
fn is_merge_endpoint(endpoint: &str) -> bool {
    let norm = normalize_endpoint(endpoint);
    let segs: Vec<&str> = norm.split('/').filter(|s| !s.is_empty()).collect();
    matches!(segs.as_slice(), [.., "pulls", _, "merge"])
}

/// Commands built into `gh`. Any other first word is an alias or an
/// extension, which can run anything (`gh p merge 12` after
/// `gh alias set p pr`).
const GH_BUILTINS: &[&str] = &[
    "agent-task",
    "alias",
    "api",
    "attestation",
    "auth",
    "browse",
    "cache",
    "codespace",
    "completion",
    "config",
    "extension",
    "gist",
    "gpg-key",
    "help",
    "issue",
    "label",
    "org",
    "pr",
    "preview",
    "project",
    "release",
    "repo",
    "ruleset",
    "run",
    "search",
    "secret",
    "ssh-key",
    "status",
    "variable",
    "version",
    "workflow",
];

/// Programs that send HTTP requests named in their arguments, and the
/// interpreters whose one-liners can (`requests.put('…/merge')`). Text that
/// only mentions the URL (`echo`, `grep`) is no request.
pub(crate) const HTTP_CLIENTS: &[&str] = &[
    "curl",
    "wget",
    "wget2",
    "http",
    "https",
    "httpie",
    "xh",
    "xhs",
    "aria2c",
    "lwp-request",
    "deno",
    "bun",
];

pub(crate) fn is_gh(cmd: &shell::Cmd) -> bool {
    let name = cmd.name();
    name == "gh" || name.ends_with("/gh")
}

/// Checks if a `gh` invocation directly merges a PR via `gh pr merge` or
/// `gh api` (`/pulls/<n>/merge` or GraphQL `mergePullRequest`).
pub fn check(cmd: &shell::Cmd) -> Option<Denial> {
    if !is_gh(cmd) {
        return check_http(cmd);
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

    // An alias that merges (`gh alias set mm 'pr merge'`) is a merge the
    // next call runs by another name. A shell alias (`--shell`, or an
    // expansion starting with `!`) runs text gh hands to a shell, and an
    // imported file or stream is not read here: both are refused.
    if first == "alias" && alias_can_merge(cmd, &positionals) {
        return Some(Denial::new(
            RULE,
            "a gh alias that can merge a PR bypasses the locked merge gate",
            FIX,
        ));
    }

    // An alias or extension asked to merge: `gh p merge 12`.
    if !GH_BUILTINS.contains(&first) && positionals[1..].iter().any(|w| is_merge_word(w)) {
        return Some(Denial::new(
            RULE,
            format!("`gh {first}` is an alias or extension asked to merge"),
            FIX,
        ));
    }

    if first == "api" {
        let mut is_graphql = false;
        for &endpoint in positionals.iter().skip(1) {
            if is_merge_endpoint(endpoint) {
                return Some(Denial::new(
                    RULE,
                    format!("direct `gh api {endpoint}` bypasses the locked merge gate"),
                    FIX,
                ));
            }
            is_graphql |= normalize_endpoint(endpoint).rsplit('/').next() == Some("graphql");
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

fn is_merge_word(word: &str) -> bool {
    word.eq_ignore_ascii_case("merge")
}

/// `gh alias set|import …` defines an alias that can merge a PR.
fn alias_can_merge(cmd: &shell::Cmd, positionals: &[&str]) -> bool {
    match positionals.get(1).copied() {
        Some("import") => true,
        Some("set") => {
            let shell_alias = cmd
                .args()
                .iter()
                .take_while(|arg| *arg != "--")
                .any(|arg| arg == "--shell" || (is_short_cluster(arg) && arg.contains('s')));
            let expansions: Vec<String> = match positionals.get(3).copied() {
                // The expansion is read from standard input.
                Some("-") | None => stdin_texts(cmd),
                Some(text) => vec![text.to_string()],
            };
            shell_alias
                || expansions
                    .iter()
                    .any(|text| text.trim_start().starts_with('!') || expansion_merges(text))
        }
        _ => false,
    }
}

fn is_short_cluster(arg: &str) -> bool {
    arg.len() > 1 && arg.starts_with('-') && !arg.starts_with("--")
}

/// The words gh splits a plain alias expansion into (quotes removed, as
/// gh does) include `merge` or the merge endpoint, or the text names the
/// GraphQL mutation. `pr list --search is:merged` does not merge.
fn expansion_merges(text: &str) -> bool {
    if text.to_ascii_lowercase().contains("mergepullrequest") {
        return true;
    }
    let script = shell::parse(text);
    !script.opaque.is_empty()
        || script.cmds.iter().any(|cmd| {
            cmd.words
                .iter()
                .chain(&cmd.wrapped)
                .any(|word| is_merge_word(word) || is_merge_endpoint(word))
        })
}

/// The here-string, piped text and heredocs `cmd` reads.
pub(crate) fn stdin_texts(cmd: &shell::Cmd) -> Vec<String> {
    let mut texts = shell::stdin_scripts(&cmd.redirects, cmd.feeder.as_deref());
    texts.extend(cmd.heredocs.iter().cloned());
    texts
}

/// The URL-like pieces of `text`: split at characters a URL cannot hold
/// unquoted, so one inside a script (`requests.put('…')`) is found too.
pub(crate) fn urls(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| c.is_whitespace() || "'\"`()[]{}<>,;\\".contains(c))
        .filter(|piece| piece.contains("://") || piece.contains("repos/"))
}

/// An HTTP client (`curl`, `wget`, `http`) or an interpreter one-liner sent
/// to the GitHub merge endpoint, REST (`…/pulls/<n>/merge`) or GraphQL
/// (`mergePullRequest`).
fn check_http(cmd: &shell::Cmd) -> Option<Denial> {
    let name = cmd.name();
    let interp = crate::interp_guard::is_interpreter(name);
    if !interp && !HTTP_CLIENTS.contains(&name) {
        return None;
    }
    let mut texts: Vec<String> = cmd.args().to_vec();
    if interp {
        texts.extend(stdin_texts(cmd));
    }
    let found: Vec<&str> = texts.iter().flat_map(|text| urls(text)).collect();
    if let Some(url) = found.iter().find(|url| is_merge_endpoint(url)) {
        return Some(Denial::new(
            RULE,
            format!(
                "a direct request to {} bypasses the locked merge gate",
                normalize_endpoint(url)
            ),
            FIX,
        ));
    }
    let graphql = found
        .iter()
        .any(|url| normalize_endpoint(url).ends_with("/graphql"));
    let has_merge = |text: &str| text.to_ascii_lowercase().contains("mergepullrequest");
    let merges = texts.iter().any(|text| has_merge(text))
        || (!interp && stdin_texts(cmd).iter().any(|text| has_merge(text)));
    (graphql && merges).then(|| {
        Denial::new(
            RULE,
            "a direct GraphQL `mergePullRequest` request bypasses the locked merge gate",
            FIX,
        )
    })
}
