//! Per-format lockfile line parsers for deterministic lockfile diff summarization.

use super::{LockFormat, SummaryAcc};

pub(super) fn parse_lock_line(
    fmt: LockFormat,
    acc: &mut SummaryAcc,
    pkg: &mut String,
    sign: char,
    content: &str,
) -> bool {
    match fmt {
        LockFormat::Toml => parse_toml_line(acc, pkg, sign, content),
        LockFormat::Json => parse_json_line(acc, pkg, sign, content),
        LockFormat::YamlOrYarn => parse_yaml_or_yarn_line(acc, pkg, sign, content),
        LockFormat::GoSum => parse_go_sum_line(acc, sign, content),
        LockFormat::GradleLock => parse_gradle_lock_line(acc, sign, content),
    }
}

fn quoted(line: &str) -> Option<String> {
    let start = line.find('"')? + 1;
    Some(line[start..start + line[start..].find('"')?].to_string())
}

fn rhs_val(line: &str) -> Option<String> {
    let rhs = line
        .split_once('=')
        .or_else(|| line.split_once(':'))?
        .1
        .trim();
    let cleaned = rhs.trim_end_matches(',').trim().trim_matches(['"', '\'']);
    (!cleaned.is_empty()).then(|| cleaned.to_string())
}

fn parse_toml_line(acc: &mut SummaryAcc, pkg: &mut String, sign: char, content: &str) -> bool {
    let t = content.trim();
    if t.is_empty() || t.starts_with('#') {
        return true;
    }
    if t.starts_with("fn ") || t.starts_with("function ") || t.starts_with("import ") {
        return false;
    }
    if t == "[[package]]" {
        pkg.clear();
        acc.recognized_fields += 1;
        return true;
    }
    if let Some(rest) = t.strip_prefix("name")
        && rest.trim_start().starts_with('=')
        && let Some(val) = quoted(rest)
    {
        *pkg = val;
        if sign != ' ' {
            acc.recognized_fields += 1;
            acc.entry(pkg);
        }
        return true;
    }
    if sign == ' ' {
        return true;
    }
    match t.split('=').next().unwrap_or("").trim() {
        "version" => {
            if let Some(v) = rhs_val(t) {
                acc.recognized_fields += 1;
                acc.entry(pkg).add_ver(sign, v);
            }
        }
        "checksum" | "hash" => {
            if let Some(h) = rhs_val(t) {
                acc.recognized_fields += 1;
                acc.entry(pkg).add_hash(sign, h);
            }
        }
        k @ ("source" | "url" | "git" | "path" | "rev" | "dependencies" | "sdist" | "wheels") => {
            acc.recognized_fields += 1;
            if matches!(k, "source" | "url" | "git" | "path")
                && let Some(u) = rhs_val(t)
            {
                acc.entry(pkg).urls.insert(u);
            }
        }
        _ if t.contains('=') || t.starts_with('"') || matches!(t, "]" | "[" | "{" | "}") => {
            acc.recognized_fields += 1
        }
        _ => {}
    }
    true
}

fn parse_json_line(acc: &mut SummaryAcc, pkg: &mut String, sign: char, content: &str) -> bool {
    let t = content.trim();
    if t.is_empty() || matches!(t, "{" | "}" | "}," | "[" | "]" | "],") {
        return true;
    }
    if !t.starts_with('"') && !t.starts_with('{') && !t.starts_with('}') {
        return false;
    }
    let key = quoted(t).unwrap_or_default();
    if t.ends_with('{')
        && !key.is_empty()
        && !matches!(
            key.as_str(),
            "packages"
                | "dependencies"
                | "devDependencies"
                | "optionalDependencies"
                | "bin"
                | "engines"
                | "funding"
                | "state"
        )
    {
        *pkg = key;
        if sign != ' ' {
            acc.recognized_fields += 1;
            acc.entry(pkg);
        }
        return true;
    }
    if matches!(key.as_str(), "identity" | "package" | "name")
        && let Some(val) = rhs_val(t)
    {
        *pkg = val;
        if sign != ' ' {
            acc.recognized_fields += 1;
            acc.entry(pkg);
        }
        return true;
    }
    if sign == ' ' {
        return true;
    }
    match key.as_str() {
        "version" => {
            if let Some(v) = rhs_val(t) {
                acc.recognized_fields += 1;
                acc.entry(pkg).add_ver(sign, v);
            }
        }
        "integrity" | "revision" => {
            if let Some(h) = rhs_val(t) {
                acc.recognized_fields += 1;
                acc.entry(pkg).add_hash(sign, h);
            }
        }
        "resolved" | "location" | "repositoryURL" => {
            acc.recognized_fields += 1;
            if let Some(u) = rhs_val(t) {
                acc.entry(pkg).urls.insert(u);
            }
        }
        _ if t.contains(':') => acc.recognized_fields += 1,
        _ => {}
    }
    true
}

fn parse_yaml_or_yarn_line(
    acc: &mut SummaryAcc,
    pkg: &mut String,
    sign: char,
    content: &str,
) -> bool {
    let t = content.trim();
    if t.is_empty() || t.starts_with('#') {
        return true;
    }
    if t.ends_with(':') && content.len() - content.trim_start().len() <= 2 {
        let header = t.trim_end_matches(':').trim_matches(['"', '\'']);
        if !matches!(
            header,
            "packages"
                | "snapshots"
                | "importers"
                | "dependencies"
                | "devDependencies"
                | "resolution"
                | "PODS"
                | "SPEC CHECKSUMS"
                | "EXTERNAL SOURCES"
        ) {
            *pkg = header.to_string();
            if sign != ' ' {
                acc.recognized_fields += 1;
                acc.entry(pkg);
            }
            return true;
        }
    }
    if sign == ' ' {
        return true;
    }
    let lower = t.to_ascii_lowercase();
    let val = || {
        rhs_val(t).or_else(|| {
            t.split_whitespace()
                .nth(1)
                .map(|s| s.trim_matches('"').to_string())
        })
    };
    if lower.starts_with("version") {
        if let Some(v) = val() {
            acc.recognized_fields += 1;
            acc.entry(pkg).add_ver(sign, v);
        }
    } else if lower.starts_with("integrity") || lower.starts_with("checksum") {
        if let Some(h) = val() {
            acc.recognized_fields += 1;
            acc.entry(pkg).add_hash(sign, h);
        }
    } else if t.contains(':') || t.starts_with("- ") {
        acc.recognized_fields += 1;
    }
    true
}

fn parse_go_sum_line(acc: &mut SummaryAcc, sign: char, content: &str) -> bool {
    let t = content.trim();
    if t.is_empty() || sign == ' ' {
        return true;
    }
    let parts: Vec<&str> = t.split_whitespace().collect();
    if parts.len() != 3 || !(parts[2].starts_with("h1:") || parts[2].starts_with("sha")) {
        return false;
    }
    acc.recognized_fields += 1;
    let delta = acc.entry(parts[0]);
    delta.add_ver(sign, parts[1].to_string());
    delta.add_hash(sign, parts[2].to_string());
    true
}

fn parse_gradle_lock_line(acc: &mut SummaryAcc, sign: char, content: &str) -> bool {
    let t = content.trim();
    if t.is_empty() || t.starts_with('#') || sign == ' ' {
        return true;
    }
    if t.starts_with("empty=") {
        acc.recognized_fields += 1;
        return true;
    }
    let Some((coord, _)) = t.split_once('=') else {
        return false;
    };
    let segs: Vec<&str> = coord.split(':').collect();
    if segs.len() < 3 {
        return false;
    }
    acc.recognized_fields += 1;
    acc.entry(&format!("{}:{}", segs[0], segs[1]))
        .add_ver(sign, segs[2..].join(":"));
    true
}
