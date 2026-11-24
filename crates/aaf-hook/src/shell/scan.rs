//! Quote-aware scanning used to find where a `$(…)` ends.

use std::mem;

use super::{Cmd, HeredocBody, Lexer, MAX_QUOTE_NESTING, lex};

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

/// Reads one heredoc body from `chars` starting at `*pos` up to `delim`.
fn read_heredoc_body(chars: &[char], pos: &mut usize, delim: &str, strip_tabs: bool) -> String {
    let mut body = String::new();
    while *pos < chars.len() {
        let end = chars[*pos..]
            .iter()
            .position(|c| *c == '\n')
            .map_or(chars.len(), |offset| *pos + offset);
        let line: String = chars[*pos..end].iter().collect();
        *pos = (end + 1).min(chars.len());
        let probe = if strip_tabs {
            line.trim_start_matches('\t')
        } else {
            line.as_str()
        };
        if probe == delim {
            break;
        }
        body.push_str(&line);
        body.push('\n');
    }
    body
}

/// Decodes `$'…'` starting right after `$'`, so `$'\x67it'` is seen as `git`.
/// Returns `None` if unterminated, else `Some((decoded, has_invalid_escape))`.
pub(super) fn decode_ansi_c(chars: &[char], pos: &mut usize) -> Option<(String, bool)> {
    let mut text = String::new();
    let mut dynamic = false;
    loop {
        let ch = *chars.get(*pos)?;
        *pos += 1;
        let decoded = match ch {
            '\'' => return Some((text, dynamic)),
            '\\' => {
                let esc = *chars.get(*pos)?;
                *pos += 1;
                match esc {
                    'n' => Some('\n'),
                    't' => Some('\t'),
                    'r' => Some('\r'),
                    'a' => Some('\u{7}'),
                    'b' => Some('\u{8}'),
                    'e' | 'E' => Some('\u{1b}'),
                    'f' => Some('\u{c}'),
                    'v' => Some('\u{b}'),
                    'x' => code_point(chars, pos, 2, 16),
                    'u' => code_point(chars, pos, 4, 16),
                    'U' => code_point(chars, pos, 8, 16),
                    '0'..='7' => {
                        *pos -= 1;
                        code_point(chars, pos, 3, 8)
                    }
                    c => Some(c),
                }
            }
            c => Some(c),
        };
        match decoded {
            Some(c) => text.push(c),
            None => dynamic = true,
        }
    }
}

fn code_point(chars: &[char], pos: &mut usize, max_digits: usize, radix: u32) -> Option<char> {
    let mut value: u32 = 0;
    let mut digits = 0;
    while digits < max_digits {
        let Some(digit) = chars.get(*pos).copied().and_then(|c| c.to_digit(radix)) else {
            break;
        };
        value = value.saturating_mul(radix).saturating_add(digit);
        *pos += 1;
        digits += 1;
    }
    if digits == 0 {
        return None;
    }
    char::from_u32(value)
}

/// Allowlisted data-sink commands whose single-command quoted heredoc body is
/// literal data (never code). The owning command's words, assigns, and
/// redirects still go through every guard (`data_guard`, `git_files`, …).
fn heredoc_data_sink(cmd: &Cmd) -> bool {
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

impl Lexer<'_, '_> {
    fn is_top_level_data_sink(&self) -> bool {
        !self.failed
            && !self.compound
            && self.depth == 0
            && self.out.cmds.len() == self.first_cmd + 1
            && self.out.cmds.last().is_some_and(heredoc_data_sink)
    }

    pub(super) fn read_heredocs_or_defer(&mut self) {
        if self.is_top_level_data_sink()
            && !self.heredocs.is_empty()
            && self
                .heredocs
                .iter()
                .all(|doc| doc.quoted && doc.pipeline.is_empty())
        {
            for doc in mem::take(&mut self.heredocs) {
                let body =
                    read_heredoc_body(&self.chars, &mut self.pos, &doc.delim, doc.strip_tabs);
                let mut wrapped = Vec::new();
                if let Some(cmd) = doc.cmd_idx.and_then(|idx| self.out.cmds.get_mut(idx)) {
                    wrapped = cmd.wrapped.clone();
                    cmd.heredocs.push(body.clone());
                }
                self.data_sink_bodies.push(HeredocBody {
                    body,
                    quoted: doc.quoted,
                    cleared: doc.cleared.unwrap_or(false),
                    is_interp: false,
                    wrapped,
                });
            }
            return;
        }
        self.read_heredocs();
    }

    pub(super) fn finish_all_bodies(&mut self) {
        if self.compound {
            let items = mem::take(&mut self.bodies);
            self.lex_deferred_bodies(items);
        }
        self.finish_bodies();
        let sinks = mem::take(&mut self.data_sink_bodies);
        if !self.is_top_level_data_sink() {
            self.lex_deferred_bodies(sinks);
        }
    }

    fn lex_deferred_bodies(&mut self, items: Vec<HeredocBody>) {
        for item in items {
            if item.quoted && item.is_interp {
                continue;
            }
            let outer_cleared = self.out.env_cleared;
            self.out.env_cleared |= item.cleared;
            self.out.enter_scope_wrapped(item.wrapped);
            lex(&item.body, self.depth + 1, self.out, None);
            self.out.exit_scope();
            self.out.env_cleared = outer_cleared;
        }
    }
}
