//! Deterministic, fail-closed lockfile diff summarizer for cross-audit (`V2-SEC-13`).
//!
//! Only exact allowlisted lockfile basenames are summarized; arbitrary `*.lock`
//! files are never matched. Every emitted line is neutralised against delimiter
//! injection and wrapped in valid synthetic unified-diff headers so `diff_hunks`
//! and `validate_verdict_against_diff` preserve auditor `Fail` citations.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use super::neutralise;
mod parsers;

/// Exact allowlisted lockfile basenames eligible for deterministic summarization.
pub const ALLOWLISTED_LOCKFILES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "Podfile.lock",
    "poetry.lock",
    "uv.lock",
    "go.sum",
    "gradle.lockfile",
    "Package.resolved",
];

/// Minimum raw diff section size (bytes) before an allowlisted lockfile is summarized.
pub const LOCKFILE_SUMMARY_THRESHOLD_BYTES: usize = 4 * 1024;

const DEFAULT_REGISTRIES: &[&str] = &[
    "https://registry.npmjs.org/",
    "https://registry.yarnpkg.com/",
    "https://pypi.org/",
    "https://files.pythonhosted.org/",
    "registry+https://github.com/rust-lang/crates.io-index",
    "sparse+https://index.crates.io/",
    "https://cdn.cocoapods.org/",
];

const ALREADY_SUMMARIZED_MARKER: &str = "-[lockfile-summary] ";

#[derive(Clone, Copy, PartialEq, Eq)]
enum LockFormat {
    Toml,
    Json,
    YamlOrYarn,
    GoSum,
    GradleLock,
}

impl LockFormat {
    fn from_basename(name: &str) -> Option<Self> {
        match name {
            "Cargo.lock" | "poetry.lock" | "uv.lock" => Some(Self::Toml),
            "package-lock.json" | "Package.resolved" => Some(Self::Json),
            "pnpm-lock.yaml" | "Podfile.lock" | "yarn.lock" => Some(Self::YamlOrYarn),
            "go.sum" => Some(Self::GoSum),
            "gradle.lockfile" => Some(Self::GradleLock),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Toml => "toml",
            Self::Json => "json",
            Self::YamlOrYarn => "yaml/yarn",
            Self::GoSum => "go.sum",
            Self::GradleLock => "gradle.lockfile",
        }
    }
}

/// True ONLY when `path`'s basename exactly matches [`ALLOWLISTED_LOCKFILES`].
pub fn is_allowlisted_lockfile(path: &str) -> bool {
    let norm = path.replace('\\', "/");
    let trimmed = norm.trim_end_matches('/');
    ALLOWLISTED_LOCKFILES.contains(&trimmed.rsplit('/').next().unwrap_or(trimmed))
}

fn basename_of(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed)
}

/// Summarizes large allowlisted lockfile sections in `diff` using
/// [`LOCKFILE_SUMMARY_THRESHOLD_BYTES`]. Non-lockfile and malformed sections
/// are kept verbatim (fail closed).
pub fn summarize_diff_lockfiles(diff: &str) -> String {
    summarize_diff_lockfiles_with_threshold(diff, LOCKFILE_SUMMARY_THRESHOLD_BYTES)
}

/// Like [`summarize_diff_lockfiles`], with an explicit per-section byte threshold.
pub fn summarize_diff_lockfiles_with_threshold(diff: &str, threshold_bytes: usize) -> String {
    let sections = split_diff_sections(diff);
    let mut out = String::with_capacity(diff.len().min(super::MAX_DIFF_BYTES));
    for section in sections {
        if section.len() > threshold_bytes
            && let Some(summary) = try_summarize_section(section)
            && summary.len() < section.len()
        {
            out.push_str(&summary);
            if section.ends_with('\n') && !summary.ends_with('\n') {
                out.push('\n');
            }
            continue;
        }
        out.push_str(section);
    }
    out
}

pub(super) fn parse_hunk_counts(line: &str) -> Option<(u32, u32)> {
    let mut parts = line
        .strip_prefix("@@")?
        .split("@@")
        .next()?
        .split_whitespace();
    let count = |s: &str| s.split_once(',').map_or(Some(1), |(_, c)| c.parse().ok());
    Some((
        count(parts.next()?.strip_prefix('-')?)?,
        count(parts.next()?.strip_prefix('+')?)?,
    ))
}

/// Splits a unified git diff on top-level `diff --git ` headers while tracking
/// hunk body line counts so diff content cannot forge section boundaries.
pub(super) fn split_diff_sections(diff: &str) -> Vec<&str> {
    let mut starts: Vec<usize> = Vec::new();
    let (mut old_rem, mut new_rem, mut offset) = (0u32, 0u32, 0usize);
    for raw in diff.split_inclusive('\n') {
        let line = raw.trim_end_matches(['\r', '\n']);
        if old_rem > 0 || new_rem > 0 {
            if line.starts_with(' ') && old_rem > 0 && new_rem > 0 {
                (old_rem, new_rem, offset) = (old_rem - 1, new_rem - 1, offset + raw.len());
                continue;
            }
            if line.starts_with('+') && new_rem > 0 {
                (new_rem, offset) = (new_rem - 1, offset + raw.len());
                continue;
            }
            if line.starts_with('-') && old_rem > 0 {
                (old_rem, offset) = (old_rem - 1, offset + raw.len());
                continue;
            }
            if line.starts_with('\\') {
                offset += raw.len();
                continue;
            }
            (old_rem, new_rem) = (0, 0);
        }
        if line.starts_with("diff --git ") {
            starts.push(offset);
        } else if line.starts_with("@@")
            && let Some((oc, nc)) = parse_hunk_counts(line)
        {
            (old_rem, new_rem) = (oc, nc);
        }
        offset += raw.len();
    }
    if starts.is_empty() {
        return vec![diff];
    }
    let mut sections = Vec::with_capacity(starts.len() + 1);
    if starts[0] > 0 {
        sections.push(&diff[..starts[0]]);
    }
    for win in starts.windows(2) {
        sections.push(&diff[win[0]..win[1]]);
    }
    if let Some(&last) = starts.last() {
        sections.push(&diff[last..]);
    }
    sections
}

pub(super) fn strip_prefix_path(raw: &str, prefix: &str) -> Option<String> {
    let field = raw.split('\t').next().unwrap_or(raw).trim();
    if field.is_empty() || field == "/dev/null" {
        return None;
    }
    let unquoted = field.trim_matches('"').replace('\\', "/");
    Some(
        unquoted
            .strip_prefix(prefix)
            .unwrap_or(&unquoted)
            .to_string(),
    )
}

fn parse_section_paths(section: &str) -> Option<(String, LockFormat)> {
    let rest = section
        .lines()
        .next()?
        .trim_end_matches('\r')
        .strip_prefix("diff --git ")?;
    let pos = rest.rfind(" b/")?;
    let mut candidates = Vec::new();
    candidates.extend(strip_prefix_path(&rest[..pos], "a/"));
    candidates.extend(strip_prefix_path(&rest[pos + 1..], "b/"));
    let (mut target_path, mut saw_hunk) = (None, false);
    for raw in section.lines().skip(1) {
        let line = raw.trim_end_matches('\r');
        if line.starts_with("@@") {
            saw_hunk = true;
            break;
        }
        if line == "GIT binary patch"
            || line.starts_with("Binary files ")
            || line.contains("120000")
        {
            return None;
        }
        if let Some(r) = line.strip_prefix("--- ") {
            candidates.extend(strip_prefix_path(r, "a/"));
        } else if let Some(r) = line.strip_prefix("+++ ")
            && let Some(p) = strip_prefix_path(r, "b/")
        {
            target_path = Some(p.clone());
            candidates.push(p);
        }
    }
    if !saw_hunk || candidates.is_empty() {
        return None;
    }
    let base_ref = basename_of(&candidates[0]);
    let fmt = LockFormat::from_basename(base_ref)?;
    if !candidates
        .iter()
        .all(|p| is_allowlisted_lockfile(p) && basename_of(p) == base_ref)
    {
        return None;
    }
    Some((target_path.or_else(|| candidates.last().cloned())?, fmt))
}

#[derive(Default)]
struct PkgDelta {
    old_versions: BTreeSet<String>,
    new_versions: BTreeSet<String>,
    old_integrity: BTreeSet<String>,
    new_integrity: BTreeSet<String>,
    urls: BTreeSet<String>,
}

impl PkgDelta {
    fn add_ver(&mut self, sign: char, v: String) {
        let set = if sign == '+' {
            &mut self.new_versions
        } else {
            &mut self.old_versions
        };
        set.insert(v);
    }

    fn add_hash(&mut self, sign: char, h: String) {
        let set = if sign == '+' {
            &mut self.new_integrity
        } else {
            &mut self.old_integrity
        };
        set.insert(h);
    }
}

#[derive(Default)]
struct SummaryAcc {
    packages: BTreeMap<String, PkgDelta>,
    alerts: BTreeSet<String>,
    recognized_fields: usize,
}

impl SummaryAcc {
    fn entry(&mut self, pkg: &str) -> &mut PkgDelta {
        let key = if pkg.is_empty() { "<root>" } else { pkg };
        self.packages.entry(key.to_string()).or_default()
    }
}

pub(super) fn sanitize_inline(text: &str) -> String {
    neutralise(text.replace(['\r', '\n'], " ").trim())
}

fn extract_urls(line: &str) -> Vec<String> {
    const SCHEMES: &[&str] = &[
        "http://", "https://", "git+", "github:", "file:", "ssh://", "git://",
    ];
    line.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '"' | '\'' | ',' | ';' | '(' | ')' | '<' | '>' | '[' | ']'
            )
    })
    .map(|raw| raw.trim_matches(|c: char| matches!(c, '{' | '}')))
    .filter(|tok| {
        let low = tok.to_ascii_lowercase();
        SCHEMES.iter().any(|p| low.starts_with(p))
    })
    .map(str::to_string)
    .collect()
}

fn scan_security_signals(acc: &mut SummaryAcc, pkg: &str, sign: char, content: &str) {
    let (lower, trimmed) = (content.to_ascii_lowercase(), content.trim());
    if (lower.contains("hasinstallscript") && lower.contains("true"))
        || lower.contains("postinstall")
        || lower.contains("preinstall")
        || (lower.contains("requiresbuild") && lower.contains("true"))
    {
        acc.alerts
            .insert(format!("[alert:install-script] {sign}{pkg}: {trimmed}"));
    }
    if lower.contains("\"bin\"") || lower.contains("hasbin") {
        acc.alerts
            .insert(format!("[alert:bin] {sign}{pkg}: {trimmed}"));
    }
    if (lower.contains("\"link\"") && lower.contains("true")) || lower.contains("link:") {
        acc.alerts
            .insert(format!("[alert:link] {sign}{pkg}: {trimmed}"));
    }
    for url in extract_urls(content) {
        acc.entry(pkg).urls.insert(url.clone());
        let low = url.to_ascii_lowercase();
        if !DEFAULT_REGISTRIES.iter().any(|p| low.starts_with(p)) {
            acc.alerts
                .insert(format!("[alert:non-default-url] {sign}{pkg}: {url}"));
        }
    }
}

fn format_pkg_lines(acc: &SummaryAcc) -> (Vec<String>, usize, usize, usize) {
    let (mut lines, mut added, mut removed, mut modified) = (Vec::new(), 0, 0, 0);
    let join = |s: &BTreeSet<String>| s.iter().cloned().collect::<Vec<_>>().join(",");
    for (name, d) in &acc.packages {
        let has_new = !d.new_versions.is_empty() || !d.new_integrity.is_empty();
        let has_old = !d.old_versions.is_empty() || !d.old_integrity.is_empty();
        let tag = match (has_old, has_new) {
            (false, true) => {
                added += 1;
                "added"
            }
            (true, false) => {
                removed += 1;
                "removed"
            }
            (true, true) => {
                modified += 1;
                "modified"
            }
            (false, false) if !d.urls.is_empty() => {
                modified += 1;
                "modified"
            }
            (false, false) => continue,
        };
        let mut line = format!("[{tag}] {name}");
        if !d.old_versions.is_empty() || !d.new_versions.is_empty() {
            let _ = write!(
                line,
                " version={}->{}",
                join(&d.old_versions),
                join(&d.new_versions)
            );
        }
        if !d.old_integrity.is_empty() || !d.new_integrity.is_empty() {
            let _ = write!(
                line,
                " integrity={}->{}",
                join(&d.old_integrity),
                join(&d.new_integrity)
            );
        }
        if !d.urls.is_empty() {
            let _ = write!(line, " urls={}", join(&d.urls));
        }
        lines.push(sanitize_inline(&line));
    }
    (lines, added, removed, modified)
}

fn try_summarize_section(section: &str) -> Option<String> {
    if section.contains(ALREADY_SUMMARIZED_MARKER) {
        return None;
    }
    let (path, fmt) = parse_section_paths(section)?;
    let (mut acc, mut pkg) = (SummaryAcc::default(), String::new());
    let (mut old_rem, mut new_rem, mut saw_changed) = (0u32, 0u32, false);
    for raw in section.lines() {
        let line = raw.trim_end_matches('\r');
        if old_rem == 0 && new_rem == 0 {
            if line.starts_with("@@")
                && let Some((oc, nc)) = parse_hunk_counts(line)
            {
                (old_rem, new_rem) = (oc, nc);
            }
            continue;
        }
        let (sign, content) = if let Some(c) = line.strip_prefix(' ') {
            if old_rem == 0 || new_rem == 0 {
                return None;
            }
            (old_rem, new_rem) = (old_rem - 1, new_rem - 1);
            (' ', c)
        } else if let Some(c) = line.strip_prefix('+') {
            if new_rem == 0 {
                return None;
            }
            new_rem -= 1;
            saw_changed = true;
            ('+', c)
        } else if let Some(c) = line.strip_prefix('-') {
            if old_rem == 0 {
                return None;
            }
            old_rem -= 1;
            saw_changed = true;
            ('-', c)
        } else if line.starts_with('\\') {
            continue;
        } else {
            return None;
        };
        if content.bytes().any(|b| b == 0 || (b < 0x20 && b != b'\t')) {
            return None;
        }
        if !parsers::parse_lock_line(fmt, &mut acc, &mut pkg, sign, content) {
            return None;
        }
        if sign != ' ' {
            scan_security_signals(&mut acc, &pkg, sign, content);
        }
    }
    if old_rem != 0 || new_rem != 0 || !saw_changed || acc.recognized_fields == 0 {
        return None;
    }

    let (pkg_lines, added, removed, modified) = format_pkg_lines(&acc);
    let mut summary_lines = Vec::with_capacity(1 + acc.alerts.len() + pkg_lines.len());
    summary_lines.push(sanitize_inline(&format!(
        "[lockfile-summary] {path} (format={}, raw_bytes={}, added={added}, removed={removed}, modified={modified}, alerts={})",
        fmt.as_str(), section.len(), acc.alerts.len(),
    )));
    summary_lines.extend(acc.alerts.iter().map(|a| sanitize_inline(a)));
    summary_lines.extend(pkg_lines);

    let safe_path = sanitize_inline(&path);
    let mut out = String::with_capacity(summary_lines.len() * 80 + 128);
    let _ = writeln!(out, "diff --git a/{safe_path} b/{safe_path}");
    let _ = writeln!(out, "--- a/{safe_path}");
    let _ = writeln!(out, "+++ b/{safe_path}");
    let _ = writeln!(out, "@@ -1,1 +1,{} @@", summary_lines.len());
    let _ = writeln!(
        out,
        "{ALREADY_SUMMARIZED_MARKER}raw diff ({} bytes) replaced by deterministic summary",
        section.len()
    );
    for line in summary_lines {
        let _ = writeln!(out, "+{line}");
    }
    Some(out)
}
