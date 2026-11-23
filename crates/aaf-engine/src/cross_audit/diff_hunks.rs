//! Unified diff hunk parser and `file:line` citation verifier for cross-audit.
//!
//! Indexes both old (`-`) and new (`+`) spans under both `old_path` and
//! `new_path` so deleted lines/files validate, unquotes git C-escaped paths,
//! and counts hunk lines so diff content cannot forge headers.

use std::collections::HashMap;

use super::prompt::TRUNCATION_MARKER;

/// 1-based inclusive line span touched by a diff hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    fn from_range(start: u32, count: u32) -> Option<Self> {
        (count > 0).then(|| Self {
            start,
            end: start.saturating_add(count - 1),
        })
    }

    fn contains_range(self, start: u32, end: u32) -> bool {
        self.start <= start && end <= self.end
    }
}

/// Touched line spans for one path in a diff.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathSpans {
    pub old_spans: Vec<Span>,
    pub new_spans: Vec<Span>,
    /// File appeared in an explicit hunkless diff (binary, mode-only, pure rename).
    pub hunkless: bool,
}

impl PathSpans {
    pub fn contains_range(&self, start: u32, end: u32) -> bool {
        start > 0
            && end >= start
            && (self.hunkless
                || self.new_spans.iter().any(|s| s.contains_range(start, end))
                || self.old_spans.iter().any(|s| s.contains_range(start, end)))
    }
}

/// Map of normalised diff paths to their touched spans.
pub type DiffHunks = HashMap<String, PathSpans>;

/// A parsed `file:line` (or `file:start-end`) citation extracted from evidence text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Citation {
    pub path: String,
    pub start: u32,
    pub end: u32,
}

/// Decodes a git C-quoted path (`"..."` with `\ooo` octal & control escapes).
fn unquote_git_path(quoted: &str) -> String {
    let inner = quoted.trim_matches('"');
    let mut bytes = Vec::with_capacity(inner.len());
    let mut iter = inner.bytes().peekable();
    while let Some(b) = iter.next() {
        if b != b'\\' {
            bytes.push(b);
            continue;
        }
        match iter.next() {
            Some(b'n') => bytes.push(b'\n'),
            Some(b't') => bytes.push(b'\t'),
            Some(b'r') => bytes.push(b'\r'),
            Some(b'a') => bytes.push(0x07),
            Some(b'b') => bytes.push(0x08),
            Some(b'f') => bytes.push(0x0c),
            Some(b'v') => bytes.push(0x0b),
            Some(digit @ b'0'..=b'7') => {
                let mut val = u32::from(digit - b'0');
                for _ in 0..2 {
                    if let Some(&next @ b'0'..=b'7') = iter.peek() {
                        val = val * 8 + u32::from(next - b'0');
                        iter.next();
                    } else {
                        break;
                    }
                }
                bytes.push(u8::try_from(val).unwrap_or(b'?'));
            }
            Some(other) => bytes.push(other),
            None => {}
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn clean_citation_path(raw: &str) -> String {
    let trimmed = raw.trim().trim_matches(['`', '"', '\'']);
    trimmed.replace('\\', "/")
}

fn strip_header_path(raw: &str, git_prefix: &str) -> Option<String> {
    let field = raw.split('\t').next().unwrap_or(raw).trim();
    if field.is_empty() || field == "/dev/null" {
        return None;
    }
    let decoded = if field.starts_with('"') && field.ends_with('"') && field.len() >= 2 {
        unquote_git_path(field)
    } else {
        field.replace('\\', "/")
    };
    Some(
        decoded
            .strip_prefix(git_prefix)
            .unwrap_or(&decoded)
            .to_string(),
    )
}

fn split_git_diff_pair(rest: &str) -> Option<(&str, &str)> {
    if rest.starts_with('"') {
        let mut escaped = false;
        for (i, b) in rest.bytes().enumerate().skip(1) {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                return Some((&rest[..=i], rest[i + 1..].trim_start()));
            }
        }
        return None;
    }
    if rest.ends_with('"')
        && let Some(pos) = rest.find(" \"b/")
    {
        return Some((&rest[..pos], &rest[pos + 1..]));
    }
    // Non-rename fast path (handles paths containing `" b/"`): `a/X b/X`.
    let mid = rest.len() / 2;
    if rest.as_bytes().get(mid) == Some(&b' ')
        && rest[..mid].strip_prefix("a/") == rest[mid + 1..].strip_prefix("b/")
    {
        return Some((&rest[..mid], &rest[mid + 1..]));
    }
    let pos = rest.rfind(" b/")?;
    Some((&rest[..pos], &rest[pos + 1..]))
}

fn parse_git_diff_header(rest: &str) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some((a, b)) = split_git_diff_pair(rest) {
        if let Some(pa) = strip_header_path(a, "a/") {
            paths.push(pa);
        }
        if let Some(pb) = strip_header_path(b, "b/")
            && !paths.contains(&pb)
        {
            paths.push(pb);
        }
    }
    paths
}

fn is_explicit_hunkless_meta(line: &str) -> bool {
    line == "GIT binary patch"
        || line.starts_with("Binary files ")
        || line.starts_with("new file mode ")
        || line.starts_with("deleted file mode ")
        || line.starts_with("old mode ")
        || line.starts_with("new mode ")
        || line.starts_with("rename from ")
        || line.starts_with("rename to ")
        || line.starts_with("copy from ")
        || line.starts_with("copy to ")
}

fn parse_hunk_header(line: &str) -> Option<(u32, u32, u32, u32)> {
    let mut parts = line
        .strip_prefix("@@")?
        .split("@@")
        .next()?
        .split_whitespace();
    let parse_pair = |s: &str| -> Option<(u32, u32)> {
        match s.split_once(',') {
            Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
            None => Some((s.parse().ok()?, 1)),
        }
    };
    let (os, oc) = parse_pair(parts.next()?.strip_prefix('-')?)?;
    let (ns, nc) = parse_pair(parts.next()?.strip_prefix('+')?)?;
    Some((os, oc, ns, nc))
}

fn flush_hunkless(map: &mut DiffHunks, git_paths: &[String], had_hunk: bool, explicit: bool) {
    if !had_hunk && explicit {
        for path in git_paths {
            map.entry(path.clone()).or_insert_with(|| PathSpans {
                hunkless: true,
                ..Default::default()
            });
        }
    }
}

/// Parses unified diff hunks into a path -> touched-spans index.
pub fn parse_diff_hunks(diff: &str) -> DiffHunks {
    parse_diff_hunks_with_truncation(diff, false)
}

/// Like [`parse_diff_hunks`], but when `truncated` is true (or the truncation
/// marker is present), never marks the final section as hunkless.
pub fn parse_diff_hunks_with_truncation(diff: &str, truncated: bool) -> DiffHunks {
    let mut map: DiffHunks = HashMap::new();
    let (mut git_paths, mut old_path, mut new_path) = (Vec::new(), None, None);
    let (mut had_hunk, mut explicit_hunkless) = (false, false);
    let (mut old_rem, mut new_rem) = (0u32, 0u32);
    let cut = truncated || diff.contains(TRUNCATION_MARKER);

    for raw_line in diff.lines() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        // Anti-forgery hunk body accounting so diff content cannot forge headers.
        if old_rem > 0 || new_rem > 0 {
            if line.starts_with(' ') && old_rem > 0 && new_rem > 0 {
                old_rem -= 1;
                new_rem -= 1;
                continue;
            }
            if line.starts_with('+') && new_rem > 0 {
                new_rem -= 1;
                continue;
            }
            if line.starts_with('-') && old_rem > 0 {
                old_rem -= 1;
                continue;
            }
            if line.starts_with('\\') {
                continue;
            }
            old_rem = 0;
            new_rem = 0;
        }

        if let Some(rest) = line.strip_prefix("diff --git ") {
            flush_hunkless(&mut map, &git_paths, had_hunk, explicit_hunkless);
            git_paths = parse_git_diff_header(rest);
            old_path = None;
            new_path = None;
            had_hunk = false;
            explicit_hunkless = false;
        } else if is_explicit_hunkless_meta(line) {
            explicit_hunkless = true;
        } else if let Some(rest) = line.strip_prefix("--- ") {
            old_path = strip_header_path(rest, "a/");
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            new_path = strip_header_path(rest, "b/");
        } else if line.starts_with("@@")
            && let Some((os, oc, ns, nc)) = parse_hunk_header(line)
        {
            (old_rem, new_rem, had_hunk) = (oc, nc, true);
            let (old_span, new_span) = (Span::from_range(os, oc), Span::from_range(ns, nc));
            for target in [old_path.as_deref(), new_path.as_deref()]
                .into_iter()
                .flatten()
            {
                let entry = map.entry(target.to_string()).or_default();
                entry.hunkless = false;
                if let Some(s) = old_span
                    && !entry.old_spans.contains(&s)
                {
                    entry.old_spans.push(s);
                }
                if let Some(s) = new_span
                    && !entry.new_spans.contains(&s)
                {
                    entry.new_spans.push(s);
                }
            }
        }
    }
    if !cut {
        flush_hunkless(&mut map, &git_paths, had_hunk, explicit_hunkless);
    }
    map
}

fn parse_line_span(spec: &str) -> Option<(u32, u32)> {
    let rest = spec.strip_prefix('L').unwrap_or(spec);
    let n = rest.bytes().take_while(u8::is_ascii_digit).count();
    let start: u32 = rest.get(..n)?.parse().ok()?;
    if start == 0 {
        return None;
    }
    let tail = &rest[n..];
    if tail.is_empty() {
        return Some((start, start));
    }
    let after = tail.strip_prefix("..").or_else(|| tail.strip_prefix('-'))?;
    let end_str = after.strip_prefix('L').unwrap_or(after);
    if end_str.is_empty() || !end_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let end: u32 = end_str.parse().ok()?;
    (end >= start).then_some((start, end))
}

fn has_valid_colons(path: &str) -> bool {
    let Some(pos) = path.find(':') else {
        return true;
    };
    let b = path.as_bytes();
    pos == 1
        && b.len() >= 3
        && b[0].is_ascii_alphabetic()
        && b[2] == b'/'
        && !path[2..].contains(':')
}

fn looks_like_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains("://")
        && has_valid_colons(path)
        && path.chars().next().is_some_and(|c| !c.is_ascii_digit())
        && (path.contains('/')
            || path.contains('.')
            || matches!(
                path,
                "Makefile" | "Dockerfile" | "Jenkinsfile" | "Justfile" | "Procfile"
            ))
}

const WRAP_START: [char; 9] = ['`', '"', '\'', '(', ')', '[', ']', '<', '>'];
const WRAP_END: [char; 13] = [
    '`', '"', '\'', '(', ')', '[', ']', '<', '>', '.', ':', ',', ';',
];

fn parse_citation_token(raw: &str) -> Option<Citation> {
    let tok = raw
        .trim_start_matches(WRAP_START)
        .trim_end_matches(WRAP_END);
    if tok.is_empty() || tok.contains("://") {
        return None;
    }
    if let Some((raw_path, rest)) = tok.split_once("#L") {
        let path = clean_citation_path(raw_path);
        if looks_like_path(&path)
            && let Some((start, end)) = parse_line_span(rest)
        {
            return Some(Citation { path, start, end });
        }
    }
    let colon = tok.rfind(':')?;
    let (mut raw_path, mut spec) = (&tok[..colon], &tok[colon + 1..]);
    if !spec.is_empty()
        && spec.bytes().all(|b| b.is_ascii_digit())
        && let Some(prev) = raw_path.rfind(':')
        && parse_line_span(&raw_path[prev + 1..]).is_some()
    {
        spec = &raw_path[prev + 1..];
        raw_path = &raw_path[..prev];
    }
    let path = clean_citation_path(raw_path);
    looks_like_path(&path)
        .then(|| parse_line_span(spec).map(|(start, end)| Citation { path, start, end }))?
}

fn push_unique(out: &mut Vec<Citation>, item: Citation) {
    if !out.contains(&item) {
        out.push(item);
    }
}

/// Extracts all recognisable `file:line` citations from an evidence string,
/// scanning quoted/backticked segments first so paths with spaces are preserved.
pub fn extract_citations(evidence: &str) -> Vec<Citation> {
    let mut out = Vec::new();
    let bytes = evidence.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let delim = bytes[i];
        if matches!(delim, b'`' | b'"' | b'\'')
            && let Some(rel_end) = bytes[i + 1..].iter().position(|&b| b == delim)
        {
            let seg_end = i + 1 + rel_end;
            let seg = &evidence[i + 1..seg_end];
            if let Some(c) = parse_citation_token(seg) {
                push_unique(&mut out, c);
            } else if let Some(after) = evidence.get(seg_end + 1..)
                && (after.starts_with(':') || after.starts_with("#L"))
            {
                let suffix_len = after
                    .find(|c: char| c.is_whitespace() || matches!(c, ',' | ';'))
                    .unwrap_or(after.len());
                let combined = format!("{seg}{}", &after[..suffix_len]);
                if let Some(c) = parse_citation_token(&combined) {
                    push_unique(&mut out, c);
                }
            }
            i = seg_end + 1;
            continue;
        }
        i += 1;
    }
    for tok in evidence.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';')) {
        if let Some(c) = parse_citation_token(tok) {
            push_unique(&mut out, c);
        }
    }
    out
}

/// Returns `true` when `evidence` contains at least one syntactic `file:line` reference.
pub fn has_file_line_reference(evidence: &str) -> bool {
    !extract_citations(evidence).is_empty()
}

/// Returns `true` when `evidence` cites at least one `file:line` anchored inside `hunks`.
pub fn evidence_in_diff(evidence: &str, hunks: &DiffHunks) -> bool {
    extract_citations(evidence).iter().any(|c| {
        [
            Some(c.path.as_str()),
            c.path.strip_prefix("./"),
            c.path.strip_prefix("a/"),
            c.path.strip_prefix("b/"),
        ]
        .into_iter()
        .flatten()
        .any(|key| {
            hunks
                .get(key)
                .is_some_and(|s| s.contains_range(c.start, c.end))
        })
    })
}
