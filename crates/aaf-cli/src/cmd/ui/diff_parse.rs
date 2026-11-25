//! Structured unified-diff parser and inline human review comment formatter
//! for `aaf ui`.

use std::iter::Peekable;
use std::path::{Component, Path};
use std::str::Chars;

use aaf_engine::executors::secret_mask::{self, NamedSecret};
use aaf_engine::executors::stream_log::scrub_secrets;
use serde::{Deserialize, Deserializer, Serialize};

/// Maximum number of inline diff review comments accepted per request.
pub(crate) const MAX_COMMENTS: usize = 20;
/// Maximum UTF-8 byte length of the formatted review comment note (for `rework`).
pub(crate) const MAX_COMMENTS_TOTAL_BYTES: usize = 4096;
/// Maximum UTF-8 byte length accepted for `steer` notes (matches `snapshots::MAX_STEER_NOTE_BYTES`).
pub(crate) const MAX_STEER_NOTE_BYTES: usize = 2000;

const MAX_COMMENT_PATH_CHARS: usize = 260;
const COMMENTS_HEADER: &str = "[Human Diff Review Comments]";
const REDACTED: &str = "[REDACTED]";

type LineKind = &'static str;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct DiffLine {
    #[serde(deserialize_with = "deserialize_line_kind")]
    pub kind: LineKind,
    pub old_ln: Option<u32>,
    pub new_ln: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct DiffHunk {
    pub header: String,
    pub old_start: u32,
    pub new_start: u32,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct DiffFile {
    pub path: String,
    pub status: String,
    pub head_locked: bool,
    pub binary: bool,
    pub additions: u32,
    pub deletions: u32,
    pub hunks: Vec<DiffHunk>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct DiffSummary {
    pub files_changed: usize,
    pub insertions: u32,
    pub deletions: u32,
    pub truncated: bool,
    pub head_locked_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct StructuredDiff {
    pub summary: DiffSummary,
    pub files: Vec<DiffFile>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DiffComment {
    pub path: String,
    #[serde(alias = "line")]
    pub line_start: u32,
    pub line_end: Option<u32>,
    pub body: String,
}

fn deserialize_line_kind<'de, D: Deserializer<'de>>(d: D) -> Result<&'static str, D::Error> {
    const KINDS: &[&str] = &["ctx", "add", "del", "meta"];
    let s = String::deserialize(d)?;
    KINDS
        .iter()
        .copied()
        .find(|&k| k == s)
        .ok_or_else(|| serde::de::Error::unknown_variant(&s, KINDS))
}

/// Scrubs env secrets, PEM blocks (including truncated multi-line blocks without `-----END`),
/// Bearer tokens, URL creds, and API keys from raw diff text.
pub(crate) fn scrub_diff_text(raw: &str) -> String {
    let (mut out, mut in_pem) = (String::with_capacity(raw.len()), false);
    for seg in raw.split_inclusive('\n') {
        let (line, nl) = seg
            .strip_suffix("\r\n")
            .map(|s| (s, "\r\n"))
            .or_else(|| seg.strip_suffix('\n').map(|s| (s, "\n")))
            .unwrap_or((seg, ""));
        if line.starts_with("diff --git ") {
            in_pem = false;
            out.push_str(seg);
            continue;
        }
        let is_hdr = line.starts_with("+++ ") || line.starts_with("--- ");
        let (prefix, body) = if !is_hdr && line.starts_with(['+', '-', ' ']) {
            (&line[..1], &line[1..])
        } else {
            ("", line)
        };
        if in_pem || is_pem_marker(body, "-----BEGIN ") {
            in_pem = !is_pem_marker(body, "-----END ");
            for part in [prefix, REDACTED, nl] {
                out.push_str(part);
            }
        } else {
            out.push_str(seg);
        }
    }
    scrub_secrets(&out, &[])
}

struct ActiveFile {
    file: DiffFile,
    old_path: Option<String>,
    cur_hunk: Option<(DiffHunk, u32, u32)>,
    in_pem: bool,
}

impl ActiveFile {
    fn from_header(rest: &str) -> Self {
        let (old_p, new_p) = split_diff_git_paths(rest);
        let path = if new_p.is_empty() {
            old_p.clone()
        } else {
            new_p
        };
        let file = DiffFile {
            path,
            status: "modified".into(),
            ..Default::default()
        };
        Self {
            file,
            old_path: (!old_p.is_empty()).then_some(old_p),
            cur_hunk: None,
            in_pem: false,
        }
    }

    fn flush_hunk(&mut self) {
        if let Some((hunk, _, _)) = self.cur_hunk.take() {
            self.file.hunks.push(hunk);
        }
        self.in_pem = false;
    }

    fn finish<F: Fn(&str) -> bool>(mut self, is_head_locked: &F) -> DiffFile {
        self.flush_hunk();
        self.file.head_locked = is_head_locked(&self.file.path)
            || (self.file.status == "renamed"
                && self.old_path.as_deref().is_some_and(is_head_locked));
        self.file
    }

    fn push_header_line(&mut self, line: &str) {
        let f = &mut self.file;
        if line.starts_with("new file mode ") {
            f.status = "added".into();
        } else if line.starts_with("deleted file mode ") {
            f.status = "deleted".into();
        } else if let Some(rest) = line.strip_prefix("rename from ") {
            self.old_path = Some(unquote_git_path(rest.trim()));
        } else if let Some(rest) = line.strip_prefix("rename to ") {
            f.status = "renamed".into();
            f.path = unquote_git_path(rest.trim());
        } else if let Some(rest) = line.strip_prefix("copy to ") {
            f.path = unquote_git_path(rest.trim());
        } else if (line.starts_with("Binary files ") && line.ends_with(" differ"))
            || line == "GIT binary patch"
        {
            f.binary = true;
        } else if let Some(rest) = line.strip_prefix("--- ") {
            let p = strip_git_prefix(&unquote_git_path(rest.trim()), "a/");
            if p == "/dev/null" && f.status == "modified" {
                f.status = "added".into();
            } else if p != "/dev/null" && !p.is_empty() {
                self.old_path = Some(p);
            }
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            let p = strip_git_prefix(&unquote_git_path(rest.trim()), "b/");
            if p == "/dev/null" {
                if f.status == "modified" {
                    f.status = "deleted".into();
                }
                if let Some(old) = &self.old_path {
                    f.path = old.clone();
                }
            } else if !p.is_empty() {
                f.path = p;
            }
        }
    }

    fn push_hunk_line(&mut self, line: &str, named: &[NamedSecret]) {
        let Some((hunk, old_ln, new_ln)) = self.cur_hunk.as_mut() else {
            return;
        };
        let (kind, o, n, raw) = if let Some(rest) = line.strip_prefix('+') {
            let ln = *new_ln;
            *new_ln = new_ln.saturating_add(1);
            self.file.additions = self.file.additions.saturating_add(1);
            ("add", None, Some(ln), rest)
        } else if let Some(rest) = line.strip_prefix('-') {
            let ln = *old_ln;
            *old_ln = old_ln.saturating_add(1);
            self.file.deletions = self.file.deletions.saturating_add(1);
            ("del", Some(ln), None, rest)
        } else if let Some(rest) = line
            .strip_prefix(' ')
            .or_else(|| line.is_empty().then_some(""))
        {
            let (cur_o, cur_n) = (*old_ln, *new_ln);
            *old_ln = old_ln.saturating_add(1);
            *new_ln = new_ln.saturating_add(1);
            ("ctx", Some(cur_o), Some(cur_n), rest)
        } else {
            ("meta", None, None, line)
        };
        hunk.lines.push(DiffLine {
            kind,
            old_ln: o,
            new_ln: n,
            text: scrub_diff_line(raw, named, &mut self.in_pem),
        });
    }
}

/// Parses `git diff --no-ext-diff --no-textconv` output into a structured diff,
/// scrubbing secrets on every parsed line and flagging head-locked files.
pub(crate) fn parse_unified_diff<F: Fn(&str) -> bool>(
    raw: &str,
    truncated: bool,
    is_head_locked: F,
) -> StructuredDiff {
    let named = secret_mask::collect_env_secrets();
    let (mut files, mut cur): (Vec<DiffFile>, Option<ActiveFile>) = (Vec::new(), None);
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(prev) = cur.take() {
                files.push(prev.finish(&is_head_locked));
            }
            cur = Some(ActiveFile::from_header(rest));
        } else if let Some(active) = cur.as_mut() {
            if let Some((old_start, new_start)) = parse_hunk_header(line) {
                active.flush_hunk();
                let header = secret_mask::mask_named_and_patterns(line, &named, &[]);
                let hunk = DiffHunk {
                    header,
                    old_start,
                    new_start,
                    lines: Vec::new(),
                };
                active.cur_hunk = Some((hunk, old_start, new_start));
            } else if active.cur_hunk.is_some() {
                active.push_hunk_line(line, &named);
            } else {
                active.push_header_line(line);
            }
        }
    }
    if let Some(prev) = cur.take() {
        files.push(prev.finish(&is_head_locked));
    }
    let (mut ins, mut del, mut locked) = (0u32, 0u32, 0usize);
    for f in &files {
        ins = ins.saturating_add(f.additions);
        del = del.saturating_add(f.deletions);
        locked += usize::from(f.head_locked);
    }
    StructuredDiff {
        summary: DiffSummary {
            files_changed: files.len(),
            insertions: ins,
            deletions: del,
            truncated,
            head_locked_count: locked,
        },
        files,
    }
}

fn is_pem_marker(text: &str, tag: &str) -> bool {
    text.contains(tag) && text.contains("PRIVATE") && text.contains("KEY-----")
}

fn scrub_diff_line(text: &str, named: &[NamedSecret], in_pem: &mut bool) -> String {
    let has_end = is_pem_marker(text, "-----END ");
    if *in_pem {
        *in_pem = !has_end;
        return REDACTED.to_string();
    }
    if is_pem_marker(text, "-----BEGIN ") && !has_end {
        *in_pem = true;
    }
    secret_mask::mask_named_and_patterns(text, named, &[])
}

fn parse_hunk_header(line: &str) -> Option<(u32, u32)> {
    let after = line.strip_prefix("@@ ")?;
    let mut parts = after[..after.find(" @@")?].split_whitespace();
    let start = |tok: &str, sig: char| tok.strip_prefix(sig)?.split(',').next()?.parse().ok();
    Some((start(parts.next()?, '-')?, start(parts.next()?, '+')?))
}

fn split_diff_git_paths(rest: &str) -> (String, String) {
    let s = rest.trim();
    if !s.starts_with('"') && s.len() % 2 == 1 {
        let mid = s.len() / 2;
        if s.as_bytes().get(mid) == Some(&b' ')
            && let (Some(lp), Some(rp)) =
                (s[..mid].strip_prefix("a/"), s[mid + 1..].strip_prefix("b/"))
            && !lp.is_empty()
            && lp == rp
        {
            return (lp.to_string(), rp.to_string());
        }
    }
    let (raw_a, raw_b) = if let Some((first, tail)) = take_quoted_token(s) {
        (first, tail.trim_start())
    } else if let Some(pos) = s.find(" \"b/").or_else(|| s.find(" b/")) {
        (&s[..pos], s[pos + 1..].trim())
    } else {
        s.split_once(' ').unwrap_or((s, ""))
    };
    (
        strip_git_prefix(&unquote_git_path(raw_a), "a/"),
        strip_git_prefix(&unquote_git_path(raw_b.trim()), "b/"),
    )
}

fn take_quoted_token(s: &str) -> Option<(&str, &str)> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'"') {
        return None;
    }
    let (mut i, mut esc) = (1, false);
    while i < bytes.len() {
        match (esc, bytes[i]) {
            (true, _) => esc = false,
            (false, b'\\') => esc = true,
            (false, b'"') => return Some((&s[..=i], &s[i + 1..])),
            _ => {}
        }
        i += 1;
    }
    None
}

fn strip_git_prefix(path: &str, prefix: &str) -> String {
    path.strip_prefix(prefix).unwrap_or(path).to_string()
}

fn unquote_git_path(raw: &str) -> String {
    let Some(inner) = raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return raw.to_string();
    };
    let b = inner.as_bytes();
    let (mut out, mut i) = (Vec::with_capacity(b.len()), 0);
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            if matches!(b[i + 1], b'0'..=b'7')
                && i + 3 < b.len()
                && let Ok(s) = std::str::from_utf8(&b[i + 1..i + 4])
                && let Ok(v) = u8::from_str_radix(s, 8)
            {
                out.push(v);
                i += 4;
                continue;
            }
            out.push(match b[i + 1] {
                b't' => b'\t',
                b'n' => b'\n',
                b'r' => b'\r',
                other => other,
            });
            i += 2;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Validates inline diff comments (up to 20 comments, 4096 B for `rework`).
#[cfg(test)]
pub(crate) fn format_inline_comments(
    base_note: &str,
    comments: &[DiffComment],
) -> Result<String, String> {
    format_inline_comments_bounded(base_note, comments, MAX_COMMENTS_TOTAL_BYTES)
}

/// Validates and formats inline diff comments enforcing a caller-specified byte cap.
pub(crate) fn format_inline_comments_bounded(
    base_note: &str,
    comments: &[DiffComment],
    max_bytes: usize,
) -> Result<String, String> {
    if comments.len() > MAX_COMMENTS {
        return Err(format!(
            "invalid comments: too many ({}, max {MAX_COMMENTS})",
            comments.len()
        ));
    }
    let clean_base = sanitize_text(base_note, false);
    let mut bullets = Vec::with_capacity(comments.len());
    for (idx, c) in comments.iter().enumerate() {
        let n = idx + 1;
        let path = validate_comment_path(&c.path, n)?;
        let bad_end = (c.line_start == 0 && c.line_end.is_some_and(|e| e != 0))
            || (c.line_start > 0 && c.line_end.is_some_and(|e| e < c.line_start));
        if bad_end {
            return Err(format!(
                "invalid comment #{n}: invalid line_end for line_start {}",
                c.line_start
            ));
        }
        let body = sanitize_text(&c.body, true);
        if body.is_empty() {
            return Err(format!("invalid comment #{n}: body cannot be empty"));
        }
        bullets.push(match (c.line_start, c.line_end) {
            (0, _) => format!("- {path}: {body}"),
            (s, Some(end)) if end > s => format!("- {path}:{s}-{end}: {body}"),
            (s, _) => format!("- {path}:{s}: {body}"),
        });
    }
    let out = match (bullets.is_empty(), clean_base.is_empty()) {
        (true, _) => clean_base,
        (false, true) => format!("{COMMENTS_HEADER}\n{}", bullets.join("\n")),
        (false, false) => format!("{clean_base}\n\n{COMMENTS_HEADER}\n{}", bullets.join("\n")),
    };
    if out.len() > max_bytes {
        return Err(format!(
            "invalid comments: exceeds {max_bytes}-byte limit ({}B)",
            out.len()
        ));
    }
    Ok(out)
}

fn validate_comment_path(raw: &str, n: usize) -> Result<String, String> {
    let err = |msg: String| Err(format!("invalid comment #{n}: {msg}"));
    if raw.chars().any(char::is_control) {
        return err("path contains control characters".into());
    }
    let path = raw.trim();
    if path.is_empty() || path.chars().count() > MAX_COMMENT_PATH_CHARS {
        return err(format!("path must be 1..={MAX_COMMENT_PATH_CHARS} chars"));
    }
    let p = Path::new(path);
    if path.starts_with(['/', '\\']) || path.as_bytes().get(1) == Some(&b':') || p.is_absolute() {
        return err(format!("path must be relative: {path}"));
    }
    let has_bad = path.split(['/', '\\']).any(|s| s == "..")
        || p.components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir));
    if has_bad {
        return err(format!("path cannot contain '..': {path}"));
    }
    Ok(path.to_string())
}

fn skip_ansi(chars: &mut Peekable<Chars<'_>>) {
    match chars.next_if(|c| (' '..='~').contains(c)) {
        Some('[') => {
            let _ = chars.find(|c| ('@'..='~').contains(c));
        }
        Some(']') => {
            while let Some(c) = chars.next() {
                if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                    break;
                }
            }
        }
        _ => {}
    }
}

fn sanitize_text(raw: &str, collapse_newlines: bool) -> String {
    let (mut out, mut chars) = (String::with_capacity(raw.len()), raw.chars().peekable());
    let nl = if collapse_newlines { ' ' } else { '\n' };
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => skip_ansi(&mut chars),
            '\r' => {
                let _ = chars.next_if_eq(&'\n');
                out.push(nl);
            }
            '\n' => out.push(nl),
            '\t' => out.push('\t'),
            _ if !c.is_control() => out.push(c),
            _ => {}
        }
    }
    out.trim().to_string()
}

/// Returns `true` only for safe git ref / branch / SHA tokens (rejects option injection,
/// whitespace, control chars, NUL, `..`, and `@{`).
pub(crate) fn is_safe_git_ref(r: &str) -> bool {
    let bad = r.is_empty()
        || r.len() > 256
        || r == "@"
        || r.starts_with(['-', '/', '.'])
        || r.ends_with(['/', '.'])
        || ["..", "@{", "//"].iter().any(|s| r.contains(s));
    !bad && r
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+' | '@'))
}

pub(crate) fn is_hex_sha(s: &str) -> bool {
    (40..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn resolve_commit_sha(repo: &Path, ref_name: &str) -> Option<String> {
    if !is_safe_git_ref(ref_name) {
        return None;
    }
    let target = format!("{ref_name}^{{commit}}");
    let (out, trunc, ok) =
        super::extras::run_git_bounded(repo, &["rev-parse", "--verify", "--quiet", &target], 256);
    let sha = out.trim();
    (ok && !trunc && is_hex_sha(sha)).then(|| sha.to_string())
}

pub(super) fn resolve_diff_base(
    repo: &Path,
    configured_base: Option<&str>,
    head_ref: &str,
) -> String {
    let Some(head_sha) = resolve_commit_sha(repo, head_ref) else {
        return "HEAD".to_string();
    };
    let mut cands: Vec<&str> = configured_base
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("auto") && is_safe_git_ref(s))
        .into_iter()
        .collect();
    cands.extend(["origin/HEAD", "origin/main", "main", "master"]);
    cands
        .into_iter()
        .find_map(|r| {
            let cand_sha = resolve_commit_sha(repo, r)?;
            let (out, trunc, ok) =
                super::extras::run_git_bounded(repo, &["merge-base", &cand_sha, &head_sha], 256);
            let sha = out.trim();
            (ok && !trunc && is_hex_sha(sha)).then(|| sha.to_string())
        })
        .unwrap_or(head_sha)
}

pub(super) fn fallback_branch_diff(
    repo: &Path,
    runs: &[aaf_domain::run::RunRecord],
    base_cfg: Option<&str>,
) -> Option<(String, String, bool, String)> {
    let branch = runs.iter().rev().find_map(|r| {
        let b = r.branch.trim();
        (!b.is_empty() && is_safe_git_ref(b)).then(|| b.to_string())
    })?;
    let head_sha = resolve_commit_sha(repo, &branch)?;
    let base_sha = resolve_diff_base(repo, base_cfg, &head_sha);
    if !is_hex_sha(&base_sha) {
        return None;
    }
    let range = format!("{base_sha}..{head_sha}");
    let (diff, truncated, _) = super::extras::run_git_bounded(
        repo,
        &["diff", "--no-ext-diff", "--no-textconv", &range, "--"],
        super::extras::MAX_DIFF_BYTES,
    );
    let (stat, _, _) = super::extras::run_git_bounded(
        repo,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--stat",
            &range,
            "--",
        ],
        32 * 1024,
    );
    (!diff.is_empty() || !stat.is_empty()).then_some((diff, stat, truncated, branch))
}

#[cfg(test)]
mod tests;
