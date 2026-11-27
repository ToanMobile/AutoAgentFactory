//! Strict fail-closed line parsers, security content scanner, and
//! order-sensitive placeholder extraction for [`super`].

use super::LocaleFormat;

pub(super) fn has_suspicious_content(content: &str) -> bool {
    if content.bytes().any(|b| b == 0 || (b < 0x20 && b != b'\t')) {
        return true;
    }
    let low = content.to_ascii_lowercase();
    const CI_NEEDLES: &[&str] = &[
        "<script",
        "javascript:",
        "onerror=",
        "onload=",
        "http://",
        "https://",
        "ws://",
        "wss://",
        "file://",
        "content://",
        "intent://",
        "market://",
        "data:",
        "tel:",
        "mailto:",
        "sms:",
        "&#",
        "\\u",
        "\\x",
        "<!",
        "<<<aaf_untrusted_",
        "-----begin",
    ];
    const CS_SECRETS: &[&str] = &[
        "AIza",
        "AKIA",
        "ASIA",
        "ghp_",
        "gho_",
        "github_pat_",
        "sk_live_",
        "pk_live_",
        "rk_live_",
        "xoxb-",
        "xoxp-",
        "xoxa-",
        "eyJ",
        "sk-",
    ];
    CI_NEEDLES.iter().any(|n| low.contains(n)) || CS_SECRETS.iter().any(|p| content.contains(p))
}

pub(super) fn has_suspicious_value(val: &str) -> bool {
    has_suspicious_content(val) || val.contains("//")
}

/// Extracts printf / NSString / ICU `{param}` placeholders. Sorts ONLY when
/// every placeholder is explicitly named or numbered (`%1$s`, `{count}`);
/// unnumbered printf specs (`%s %d`, `%lld`) keep exact sequence order so
/// argument swaps trigger a mismatch alert.
pub(super) fn extract_placeholders(value: &str) -> Vec<String> {
    let bytes = value.as_bytes();
    let (mut out, mut all_indexed_or_named) = (Vec::new(), true);
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'%' {
                i += 2;
                continue;
            }
            let start = i;
            i += 1;
            let mut has_pos = false;
            let dig_start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i > dig_start && i < bytes.len() && bytes[i] == b'$' {
                has_pos = true;
                i += 1;
            }
            while i < bytes.len()
                && matches!(bytes[i], b'-' | b'+' | b' ' | b'#' | b'0'..=b'9' | b'.')
            {
                i += 1;
            }
            while i < bytes.len() && matches!(bytes[i], b'l' | b'h' | b'z' | b't' | b'j') {
                i += 1;
            }
            if i < bytes.len()
                && matches!(
                    bytes[i],
                    b's' | b'd'
                        | b'i'
                        | b'u'
                        | b'f'
                        | b'e'
                        | b'E'
                        | b'g'
                        | b'G'
                        | b'c'
                        | b'x'
                        | b'X'
                        | b'@'
                        | b'p'
                )
            {
                if !has_pos {
                    all_indexed_or_named = false;
                }
                out.push(value[start..=i].to_string());
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'{' {
            let start = i;
            if let Some(rel) = value[i + 1..].find('}') {
                let inner = &value[i + 1..i + 1 + rel];
                if !inner.is_empty()
                    && inner
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_')
                {
                    out.push(value[start..=i + 1 + rel].to_string());
                    i += rel + 2;
                    continue;
                }
            }
        }
        i += 1;
    }
    if all_indexed_or_named {
        out.sort();
    }
    out
}

/// Parses one JSON/iOS double-quoted string literal respecting `\"` / `\\` escapes,
/// returning `(inner_slice, remaining_after_closing_quote)`.
fn parse_quoted_str(s: &str) -> Option<(&str, &str)> {
    let rest = s.strip_prefix('"')?;
    let mut escaped = false;
    for (idx, ch) in rest.char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return Some((&rest[..idx], &rest[idx + 1..]));
        }
    }
    None
}

fn parse_android_line(line: &str) -> Option<Option<(String, String)>> {
    let t = line.trim();
    if t.is_empty() || t == "</resources>" || t == "<resources>" {
        return Some(None);
    }
    if t.starts_with("<!--")
        && t.ends_with("-->")
        && t.len() >= 7
        && !t[4..t.len() - 3].contains("-->")
    {
        return Some(None);
    }
    if t.starts_with("<resources ") && t.ends_with('>') && !t[1..t.len() - 1].contains(['<', '>']) {
        return Some(None);
    }
    if t.starts_with("<?xml")
        && t.ends_with("?>")
        && t.len() >= 7
        && !t[5..t.len() - 2].contains("?>")
    {
        return Some(None);
    }
    let after = t.strip_prefix("<string ")?;
    let gt = after.find('>')?;
    let attrs = &after[..gt];
    if attrs.contains('<') {
        return None;
    }
    let name_pos = attrs.find("name=\"")?;
    let rest = &attrs[name_pos + 6..];
    let end_q = rest.find('"')?;
    let key = &rest[..end_q];
    if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return None;
    }
    let body = after[gt + 1..].strip_suffix("</string>")?;
    if body.contains(['<', '>']) {
        return None;
    }
    Some(Some((key.to_string(), body.to_string())))
}

fn parse_ios_line(line: &str) -> Option<Option<(String, String)>> {
    let t = line.trim();
    if t.is_empty() || t.starts_with("//") {
        return Some(None);
    }
    if t.starts_with("/*") && t.ends_with("*/") && t.len() >= 4 && !t[2..t.len() - 2].contains("*/")
    {
        return Some(None);
    }
    let (key, after_key) = parse_quoted_str(t)?;
    if key.is_empty() {
        return None;
    }
    let rhs = after_key.trim_start().strip_prefix('=')?.trim_start();
    let (val, tail) = parse_quoted_str(rhs)?;
    if tail.trim() != ";" {
        return None;
    }
    Some(Some((key.to_string(), val.to_string())))
}

fn parse_arb_line(line: &str) -> Option<Option<(String, String)>> {
    let t = line.trim();
    if t.is_empty() || t == "{" || t == "}" {
        return Some(None);
    }
    let (key, after_key) = parse_quoted_str(t)?;
    if key.is_empty() {
        return None;
    }
    let rhs = after_key.trim_start().strip_prefix(':')?.trim_start();
    let (val, tail) = parse_quoted_str(rhs)?;
    if !matches!(tail.trim(), "" | ",") {
        return None;
    }
    if key == "@@locale" {
        return Some(None);
    }
    if key.starts_with('@') {
        return None;
    }
    Some(Some((key.to_string(), val.to_string())))
}

pub(super) fn parse_locale_line(fmt: LocaleFormat, line: &str) -> Option<Option<(String, String)>> {
    match fmt {
        LocaleFormat::AndroidXml => parse_android_line(line),
        LocaleFormat::IosStrings => parse_ios_line(line),
        LocaleFormat::FlutterArb => parse_arb_line(line),
    }
}
