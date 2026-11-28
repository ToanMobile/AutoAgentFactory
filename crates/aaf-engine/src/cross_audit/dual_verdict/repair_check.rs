//! Lenient JSON scanning for the anti-downgrade latch and compact format-repair gating.

use std::collections::BTreeSet;

use aaf_domain::gate::GoldenScore;

use super::{ChecklistItem, parse_scores};
use crate::cross_audit::verdict::strip_ansi_escapes;

/// Scans top-level and inner complete JSON objects from `text`, also recording
/// whether any `{` candidate failed to parse (e.g. truncated JSON reply).
fn scan_json_values(text: &str) -> (Vec<serde_json::Value>, bool) {
    if text.len() > 512 * 1024 {
        return (Vec::new(), true);
    }
    let clean = strip_ansi_escapes(text);
    let (s, b) = (clean.as_ref(), clean.as_bytes());
    let (mut out, mut count, mut i, mut had_unparsed) = (Vec::new(), 0usize, 0usize, false);
    while let Some(rel) = b.get(i..).and_then(|x| x.iter().position(|&c| c == b'{')) {
        let start = i + rel;
        if b[start + 1..].iter().find(|c| !c.is_ascii_whitespace()) != Some(&b'"') {
            i = start + 1;
            continue;
        }
        count += 1;
        if count > 4096 {
            had_unparsed = true;
            break;
        }
        let mut de =
            serde_json::Deserializer::from_str(&s[start..]).into_iter::<serde_json::Value>();
        if let Some(Ok(val)) = de.next() {
            if val.is_object() {
                out.push(val);
            }
            i = start + de.byte_offset().max(1);
        } else {
            had_unparsed = true;
            i = start + 1;
        }
    }
    (out, had_unparsed)
}

fn is_blocking_id(id: &str, checklist: &[ChecklistItem]) -> bool {
    let trimmed = id.trim();
    checklist
        .iter()
        .any(|it| it.blocking && it.id.eq_ignore_ascii_case(trimmed))
}

fn value_names_blocker_recursive(val: &serde_json::Value, checklist: &[ChecklistItem]) -> bool {
    match val {
        serde_json::Value::Object(map) => {
            if let (Some(id), Some(ans)) = (
                map.get("id").and_then(serde_json::Value::as_str),
                map.get("answer").and_then(serde_json::Value::as_str),
            ) && ans.trim().eq_ignore_ascii_case("yes")
                && is_blocking_id(id, checklist)
            {
                return true;
            }
            if map
                .get("severity")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|s| {
                    s.trim().eq_ignore_ascii_case("P0") || s.trim().eq_ignore_ascii_case("P1")
                })
            {
                return true;
            }
            map.values()
                .any(|v| value_names_blocker_recursive(v, checklist))
        }
        serde_json::Value::Array(arr) => arr
            .iter()
            .any(|v| value_names_blocker_recursive(v, checklist)),
        _ => false,
    }
}

fn enclosing_object_fragment(text: &str, pos: usize, needle_len: usize) -> &str {
    let bytes = text.as_bytes();
    let mut start = pos;
    while start > 0 {
        let b = bytes[start - 1];
        if b == b'{' {
            start -= 1;
            break;
        }
        if b == b'}' {
            break;
        }
        start -= 1;
    }
    let mut end = (pos + needle_len).min(bytes.len());
    while end < bytes.len() {
        let b = bytes[end];
        if b == b'}' {
            end += 1;
            break;
        }
        if b == b'{' {
            break;
        }
        end += 1;
    }
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    &text[start..end]
}

fn raw_text_names_blocker(raw: &str, checklist: &[ChecklistItem]) -> bool {
    let clean = strip_ansi_escapes(raw);
    let compact: String = clean.chars().filter(|c| !c.is_whitespace()).collect();
    let lower = compact.to_ascii_lowercase();
    if lower.contains("\"severity\":\"p0\"") || lower.contains("\"severity\":\"p1\"") {
        return true;
    }
    if !lower.contains("\"answer\":\"yes\"") {
        return false;
    }
    // Bound each match strictly to its enclosing `{ ... }` object fragment so
    // long `evidence` fields still match while adjacent `STORE-*` objects never bleed over.
    checklist.iter().filter(|it| it.blocking).any(|it| {
        let needle = format!("\"id\":\"{}\"", it.id.to_ascii_lowercase());
        lower.match_indices(&needle).any(|(pos, _)| {
            enclosing_object_fragment(&lower, pos, needle.len()).contains("\"answer\":\"yes\"")
        })
    })
}

fn value_has_uncited_blocker(val: &serde_json::Value, checklist: &[ChecklistItem]) -> bool {
    match val {
        serde_json::Value::Object(map) => {
            if let (Some(id), Some(ans)) = (
                map.get("id").and_then(serde_json::Value::as_str),
                map.get("answer").and_then(serde_json::Value::as_str),
            ) && ans.trim().eq_ignore_ascii_case("yes")
                && is_blocking_id(id, checklist)
                && map
                    .get("evidence")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .is_empty()
            {
                return true;
            }
            if map
                .get("severity")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|s| {
                    s.trim().eq_ignore_ascii_case("P0") || s.trim().eq_ignore_ascii_case("P1")
                })
                && map
                    .get("location")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .is_empty()
            {
                return true;
            }
            map.values()
                .any(|v| value_has_uncited_blocker(v, checklist))
        }
        serde_json::Value::Array(arr) => {
            arr.iter().any(|v| value_has_uncited_blocker(v, checklist))
        }
        _ => false,
    }
}

/// Returns `true` if `raw` contains any blocking checklist `"yes"` answer or any
/// `P0`/`P1` issue — walking parsed JSON recursively (including wrapped objects
/// and standalone inner objects from truncated replies) and scanning unparsed
/// JSON fragments so strict deserialization failures never bypass the
/// anti-downgrade latch.
pub fn names_dual_blocker(raw: &str, checklist: &[ChecklistItem]) -> bool {
    let (vals, had_unparsed) = scan_json_values(raw);
    if vals
        .iter()
        .any(|v| value_names_blocker_recursive(v, checklist))
    {
        return true;
    }
    if vals.is_empty() || had_unparsed {
        return raw_text_names_blocker(raw, checklist);
    }
    false
}

/// Returns `true` ONLY when `raw` has no unparsed/truncated JSON objects, contains
/// no uncited blocking findings across any parsed object, and contains at least one
/// lenient JSON object that answered every required checklist ID with `"yes"`/`"no"`
/// plus all 7 valid `G1..G7` scores when `score_gate` is enabled.
pub fn can_use_compact_format_repair(
    raw: &str,
    checklist: &[ChecklistItem],
    score_gate: bool,
) -> bool {
    if checklist.is_empty() {
        return false;
    }
    let (vals, had_unparsed) = scan_json_values(raw);
    if had_unparsed || vals.iter().any(|v| value_has_uncited_blocker(v, checklist)) {
        return false;
    }
    vals.into_iter().any(|val| {
        let Some(answers) = val.get("answers").and_then(serde_json::Value::as_array) else {
            return false;
        };
        let mut seen = BTreeSet::new();
        for entry in answers {
            let Some(id) = entry.get("id").and_then(serde_json::Value::as_str) else {
                return false;
            };
            let Some(ans) = entry.get("answer").and_then(serde_json::Value::as_str) else {
                return false;
            };
            if !matches!(ans, "yes" | "no")
                || !checklist.iter().any(|it| it.id == id)
                || !seen.insert(id)
            {
                return false;
            }
        }
        if seen.len() != checklist.len() {
            return false;
        }
        if !score_gate {
            return true;
        }
        let Some(scores_val) = val.get("scores").cloned() else {
            return false;
        };
        serde_json::from_value::<Vec<GoldenScore>>(scores_val)
            .ok()
            .and_then(|s| parse_scores(s).ok())
            .is_some()
    })
}
