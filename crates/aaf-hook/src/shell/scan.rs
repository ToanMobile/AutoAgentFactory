//! Quote-aware scanning used to find where a `$(…)` ends.

use std::mem;

use super::{Cmd, MAX_QUOTE_NESTING};

/// Index of the `)` closing a `$(` whose body starts at `start`.
pub(super) fn find_close(chars: &[char], start: usize, nesting: usize) -> Option<usize> {
    if nesting > MAX_QUOTE_NESTING {
        return None;
    }
    let mut depth = 1usize;
    let mut at = start;
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    while at < chars.len() {
        match chars[at] {
            '\\' => at += 2,
            '\'' => at = find_char(chars, at + 1, '\'')? + 1,
            '"' => at = skip_double(chars, at + 1, nesting)? + 1,
            '`' => at = skip_backtick(chars, at + 1)? + 1,
            '(' => {
                depth += 1;
                at += 1;
            }
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(at);
                }
                at += 1;
            }
            '<' if chars.get(at + 1) == Some(&'<') && chars.get(at + 2) == Some(&'<') => at += 3,
            '<' if chars.get(at + 1) == Some(&'<') => {
                // A heredoc body is not shell syntax: an apostrophe in it
                // must not be read as a quote.
                let (delim, strip_tabs, next) = heredoc_delim(chars, at + 2);
                if !delim.is_empty() {
                    heredocs.push((delim, strip_tabs));
                }
                at = next;
            }
            '\n' if !heredocs.is_empty() => {
                at += 1;
                for (delim, strip_tabs) in mem::take(&mut heredocs) {
                    at = skip_heredoc(chars, at, &delim, strip_tabs)?;
                }
            }
            _ => at += 1,
        }
    }
    None
}

fn find_char(chars: &[char], from: usize, wanted: char) -> Option<usize> {
    chars
        .get(from..)?
        .iter()
        .position(|c| *c == wanted)
        .map(|offset| from + offset)
}

fn skip_double(chars: &[char], from: usize, nesting: usize) -> Option<usize> {
    let mut at = from;
    while at < chars.len() {
        match chars[at] {
            '\\' => at += 2,
            '"' => return Some(at),
            '$' if chars.get(at + 1) == Some(&'(') => {
                at = find_close(chars, at + 2, nesting + 1)? + 1;
            }
            '`' => at = skip_backtick(chars, at + 1)? + 1,
            _ => at += 1,
        }
    }
    None
}

fn skip_backtick(chars: &[char], from: usize) -> Option<usize> {
    let mut at = from;
    while at < chars.len() {
        match chars[at] {
            '\\' => at += 2,
            '`' => return Some(at),
            _ => at += 1,
        }
    }
    None
}

fn heredoc_delim(chars: &[char], from: usize) -> (String, bool, usize) {
    let mut at = from;
    let strip_tabs = chars.get(at) == Some(&'-');
    if strip_tabs {
        at += 1;
    }
    while matches!(chars.get(at), Some(' ' | '\t')) {
        at += 1;
    }
    let mut delim = String::new();
    while let Some(c) = chars.get(at).copied() {
        if c.is_whitespace() || ";&|()<>".contains(c) {
            break;
        }
        if !matches!(c, '\'' | '"' | '\\') {
            delim.push(c);
        }
        at += 1;
    }
    (delim, strip_tabs, at)
}

fn skip_heredoc(chars: &[char], from: usize, delim: &str, strip_tabs: bool) -> Option<usize> {
    let mut at = from;
    while at < chars.len() {
        let end = chars[at..]
            .iter()
            .position(|c| *c == '\n')
            .map_or(chars.len(), |offset| at + offset);
        let line: String = chars[at..end].iter().collect();
        at = (end + 1).min(chars.len());
        let probe = if strip_tabs {
            line.trim_start_matches('\t')
        } else {
            line.as_str()
        };
        if probe == delim {
            return Some(at);
        }
    }
    None
}

/// Allowlisted data-sink commands whose single-command quoted heredoc body is
/// literal data (never code). The owning command's words, assigns, and
/// redirects still go through every guard (`data_guard`, `git_files`, …).
pub(super) fn heredoc_data_sink(cmd: &Cmd) -> bool {
    if cmd.dynamic_name
        || !cmd.assigns.is_empty()
        || !cmd.wrapped.is_empty()
        || cmd.xargs
        || cmd.feeder.is_some()
        || cmd.env_cleared
        || !cmd.redirects.iter().all(|r| r.op == ">" || r.op == ">>")
    {
        return false;
    }
    match cmd.words.first().map(String::as_str) {
        Some("cat") => cmd.args().is_empty() || cmd.args() == ["-"],
        Some("git") => {
            let call = crate::git_guard::split(cmd.args());
            matches!(call.sub, "commit" | "tag")
                && has_stdin_flag(
                    cmd,
                    2 + call.globals.len(),
                    &["-F", "--file"],
                    &["-F-", "--file=-"],
                )
        }
        Some("gh") => {
            matches!(cmd.words.get(1).map(String::as_str), Some("pr" | "issue"))
                && cmd.words.get(2).map(String::as_str) == Some("create")
                && has_stdin_flag(cmd, 3, &["--body-file", "-F"], &["--body-file=-", "-F-"])
        }
        _ => false,
    }
}

fn has_stdin_flag(cmd: &Cmd, start: usize, separate: &[&str], glued: &[&str]) -> bool {
    let is_dyn = |i: usize| cmd.arg_dynamic.get(i).copied().unwrap_or(true);
    let mut i = start;
    while let Some(word) = cmd.words.get(i).map(String::as_str) {
        if word == "--" {
            break;
        }
        if !is_dyn(i) && glued.contains(&word) {
            return true;
        }
        if !is_dyn(i) && separate.contains(&word) {
            return cmd.words.get(i + 1).map(String::as_str) == Some("-") && !is_dyn(i + 1);
        }
        if matches!(
            word,
            "-m" | "--message" | "-t" | "--title" | "-b" | "--body" | "-C" | "-c"
        ) {
            i += 2;
            continue;
        }
        i += 1;
    }
    false
}
