//! Deterministic, fail-closed mobile multi-locale translation diff summarizer.
//!
//! When at least [`MIN_LOCALE_FILES_TO_SUMMARIZE`] secondary locale files change
//! in one diff (Android `res/values-<locale>/strings.xml`, iOS
//! `<locale>.lproj/Localizable.strings`, Flutter `app_<locale>.arb`), this
//! module replaces each clean, strictly parseable secondary locale section with
//! a compact synthetic unified-diff summary while failing closed (keeping
//! verbatim) on base/English locales, small PRs, renames, binary/symlink
//! patches, multi-entry/comment-smuggled lines, encoded/suspicious values
//! (URLs, scripts, deep links, secrets), or unrecognized syntax.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

mod parsers;

use super::lockfile_summary::{
    parse_hunk_counts, sanitize_inline, split_diff_sections, strip_prefix_path,
};
use parsers::{
    extract_placeholders, has_suspicious_content, has_suspicious_value, parse_locale_line,
};

/// Minimum secondary locale files in one diff before locale summarization activates.
pub const MIN_LOCALE_FILES_TO_SUMMARIZE: usize = 4;

const ALREADY_SUMMARIZED_MARKER: &str = "-[locale-summary] ";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum LocaleFormat {
    AndroidXml,
    IosStrings,
    FlutterArb,
}

impl LocaleFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::AndroidXml => "android-strings",
            Self::IosStrings => "ios-strings",
            Self::FlutterArb => "flutter-arb",
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
enum LocaleRole {
    Base(LocaleFormat, String),
    Secondary(LocaleFormat, String, String),
}

fn is_english_tag(tag: &str) -> bool {
    let low = tag.to_ascii_lowercase();
    low == "en"
        || low == "base"
        || low.starts_with("en-")
        || low.starts_with("en_")
        || low == "b+en"
        || low.starts_with("b+en+")
}

/// Android accepts 2-letter ISO-639-1 codes in `values-xx` / `values-xx-rYY`
/// (3-letter qualifiers like `values-car` are UI-mode config qualifiers, not
/// locales; 3-letter languages require BCP-47 `values-b+...`).
fn is_android_locale_qualifier(q: &str) -> bool {
    if let Some(rest) = q.strip_prefix("b+") {
        let mut parts = rest.split('+');
        let Some(lang) = parts.next() else {
            return false;
        };
        return (2..=3).contains(&lang.len())
            && lang.bytes().all(|b| b.is_ascii_lowercase())
            && parts.all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric()));
    }
    if let Some((lang, region)) = q.split_once("-r") {
        return lang.len() == 2
            && lang.bytes().all(|b| b.is_ascii_lowercase())
            && region.len() == 2
            && region.bytes().all(|b| b.is_ascii_uppercase());
    }
    q.len() == 2 && q.bytes().all(|b| b.is_ascii_lowercase())
}

fn is_generic_locale_code(code: &str) -> bool {
    let mut parts = code.split(['-', '_']);
    let Some(lang) = parts.next() else {
        return false;
    };
    (2..=3).contains(&lang.len())
        && lang.bytes().all(|b| b.is_ascii_alphabetic())
        && parts.all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric()))
}

fn classify_locale_path(path: &str) -> Option<LocaleRole> {
    let norm = path.replace('\\', "/");
    let comps: Vec<&str> = norm.split('/').filter(|s| !s.is_empty()).collect();
    let len = comps.len();
    if len >= 3 && comps[len - 1] == "strings.xml" && comps[len - 3] == "res" {
        let scope = comps[..len - 2].join("/");
        let dir = comps[len - 2];
        if dir == "values" {
            return Some(LocaleRole::Base(LocaleFormat::AndroidXml, scope));
        }
        if let Some(q) = dir.strip_prefix("values-")
            && is_android_locale_qualifier(q)
        {
            return Some(if is_english_tag(q) {
                LocaleRole::Base(LocaleFormat::AndroidXml, scope)
            } else {
                LocaleRole::Secondary(LocaleFormat::AndroidXml, scope, q.to_string())
            });
        }
        return None;
    }
    if len >= 2
        && comps[len - 1] == "Localizable.strings"
        && let Some(loc) = comps[len - 2].strip_suffix(".lproj")
    {
        let scope = comps[..len - 2].join("/");
        if loc.eq_ignore_ascii_case("base") {
            return Some(LocaleRole::Base(LocaleFormat::IosStrings, scope));
        }
        if is_generic_locale_code(loc) {
            return Some(if is_english_tag(loc) {
                LocaleRole::Base(LocaleFormat::IosStrings, scope)
            } else {
                LocaleRole::Secondary(LocaleFormat::IosStrings, scope, loc.to_string())
            });
        }
        return None;
    }
    if len >= 2
        && matches!(comps[len - 2], "l10n" | "arb" | "i18n")
        && let Some(loc) = comps[len - 1]
            .strip_prefix("app_")
            .and_then(|s| s.strip_suffix(".arb"))
        && is_generic_locale_code(loc)
    {
        let scope = comps[..len - 1].join("/");
        return Some(if is_english_tag(loc) {
            LocaleRole::Base(LocaleFormat::FlutterArb, scope)
        } else {
            LocaleRole::Secondary(LocaleFormat::FlutterArb, scope, loc.to_string())
        });
    }
    None
}

/// True when `path` is an allowlisted secondary (non-base, non-English) mobile
/// translation file eligible for deterministic multi-locale summarization.
pub fn is_secondary_locale_file(path: &str) -> bool {
    matches!(classify_locale_path(path), Some(LocaleRole::Secondary(..)))
}

fn parse_section_role(section: &str) -> Option<(String, LocaleRole)> {
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
            || line.starts_with("rename from ")
            || line.starts_with("rename to ")
            || line.starts_with("copy from ")
            || line.starts_with("copy to ")
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
    let canonical = &candidates[0];
    if !candidates.iter().all(|p| p == canonical) {
        return None;
    }
    let role = classify_locale_path(canonical)?;
    Some((target_path.unwrap_or_else(|| canonical.clone()), role))
}

type BasePlaceholderKey = (LocaleFormat, String, String);
type BasePlaceholderMap = BTreeMap<BasePlaceholderKey, Vec<String>>;

fn collect_base_placeholders(sections: &[&str]) -> BasePlaceholderMap {
    let mut map = BasePlaceholderMap::new();
    for section in sections {
        let Some((_, LocaleRole::Base(fmt, scope))) = parse_section_role(section) else {
            continue;
        };
        let (mut old_rem, mut new_rem) = (0u32, 0u32);
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
            if line.starts_with(' ') && old_rem > 0 && new_rem > 0 {
                (old_rem, new_rem) = (old_rem - 1, new_rem - 1);
            } else if let Some(c) = line.strip_prefix('+')
                && new_rem > 0
            {
                new_rem -= 1;
                if let Some(Some((k, v))) = parse_locale_line(fmt, c) {
                    map.insert((fmt, scope.clone(), k), extract_placeholders(&v));
                }
            } else if line.starts_with('-') && old_rem > 0 {
                old_rem -= 1;
            }
        }
    }
    map
}

fn try_summarize_locale_section(
    section: &str,
    path: &str,
    fmt: LocaleFormat,
    scope: &str,
    locale: &str,
    base_ph: &BasePlaceholderMap,
) -> Option<String> {
    if section.contains(ALREADY_SUMMARIZED_MARKER) {
        return None;
    }
    let (mut old_keys, mut new_keys) = (BTreeSet::new(), BTreeSet::new());
    let mut alerts = BTreeSet::new();
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
        if sign == ' ' {
            continue;
        }
        let parsed = parse_locale_line(fmt, content)?;
        if has_suspicious_content(content) {
            return None;
        }
        if let Some((key, val)) = parsed {
            if has_suspicious_value(&val) {
                return None;
            }
            if sign == '+' {
                if val.trim().is_empty() {
                    alerts.insert(format!("[alert:empty-value] {key}"));
                }
                if let Some(expected) = base_ph.get(&(fmt, scope.to_string(), key.clone())) {
                    let got = extract_placeholders(&val);
                    if &got != expected {
                        alerts.insert(format!(
                            "[alert:placeholder-mismatch] {key}: expected={expected:?} got={got:?}"
                        ));
                    }
                }
                new_keys.insert(key);
            } else {
                old_keys.insert(key);
            }
        }
    }
    if old_rem != 0 || new_rem != 0 || !saw_changed || (old_keys.is_empty() && new_keys.is_empty())
    {
        return None;
    }

    let added = new_keys.difference(&old_keys).count();
    let removed = old_keys.difference(&new_keys).count();
    let modified = new_keys.intersection(&old_keys).count();
    let preview_keys: Vec<String> = new_keys.union(&old_keys).take(12).cloned().collect();
    let total_keys = new_keys.union(&old_keys).count();
    let keys_tail = if total_keys > preview_keys.len() {
        format!(",+{}more", total_keys - preview_keys.len())
    } else {
        String::new()
    };

    let mut summary_lines = Vec::with_capacity(1 + alerts.len());
    summary_lines.push(sanitize_inline(&format!(
        "[locale-summary] {path} (format={}, locale={locale}, raw_bytes={}, added={added}, removed={removed}, modified={modified}, alerts={}, keys={}{keys_tail})",
        fmt.as_str(),
        section.len(),
        alerts.len(),
        preview_keys.join(","),
    )));
    summary_lines.extend(alerts.iter().map(|a| sanitize_inline(a)));

    let safe_path = sanitize_inline(path);
    let mut out = String::with_capacity(summary_lines.len() * 100 + 128);
    let _ = writeln!(out, "diff --git a/{safe_path} b/{safe_path}");
    let _ = writeln!(out, "--- a/{safe_path}");
    let _ = writeln!(out, "+++ b/{safe_path}");
    let _ = writeln!(out, "@@ -1,1 +1,{} @@", summary_lines.len());
    let _ = writeln!(out, "{ALREADY_SUMMARIZED_MARKER}{}B", section.len());
    for line in summary_lines {
        let _ = writeln!(out, "+{line}");
    }
    Some(out)
}

/// Summarizes secondary mobile locale translation diffs when at least
/// [`MIN_LOCALE_FILES_TO_SUMMARIZE`] secondary locale files are present in
/// `diff`. Base locales, small PRs, renames, malformed lines, and suspicious
/// content are kept verbatim (fail closed).
pub fn summarize_diff_locales(diff: &str) -> String {
    summarize_diff_locales_with_min_files(diff, MIN_LOCALE_FILES_TO_SUMMARIZE)
}

/// Like [`summarize_diff_locales`] with an explicit minimum secondary-file count.
pub fn summarize_diff_locales_with_min_files(diff: &str, min_files: usize) -> String {
    let sections = split_diff_sections(diff);
    let secondary_count = sections
        .iter()
        .filter(|s| matches!(parse_section_role(s), Some((_, LocaleRole::Secondary(..)))))
        .count();
    if secondary_count < min_files.max(1) {
        return diff.to_string();
    }
    let base_ph = collect_base_placeholders(&sections);
    let mut out = String::with_capacity(diff.len().min(super::MAX_DIFF_BYTES));
    for section in sections {
        if let Some((path, LocaleRole::Secondary(fmt, scope, locale))) = parse_section_role(section)
            && let Some(summary) =
                try_summarize_locale_section(section, &path, fmt, &scope, &locale, &base_ph)
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
