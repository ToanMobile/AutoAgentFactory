//! Scoped call-argument extraction, JS regex-literal `.exec(` disambiguation,
//! and Python `tempfile.mkdtemp()` / `TemporaryDirectory()` variable tracking
//! for `interp_guard`.

use std::collections::HashSet;

use super::parse::normalize_api_haystack;

const SHELL_EXEC_NON_DOT_EXEC: &[&str] = &[
    "os.system",
    "os.popen",
    "subprocess.",
    "execSync",
    "execFileSync",
    "spawnSync",
    "child_process",
    "system(",
    "passthru(",
    "qx/",
    "qx{",
    "qx(",
];

pub(super) fn has_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn step_quote(c: char, quote: &mut Option<char>, escaped: &mut bool, backtick: bool) -> bool {
    if let Some(q) = *quote {
        if *escaped {
            *escaped = false;
        } else if c == '\\' {
            *escaped = true;
        } else if c == q {
            *quote = None;
        }
        return true;
    }
    if c == '\'' || c == '"' || (backtick && c == '`') {
        *quote = Some(c);
        return true;
    }
    false
}

/// True when `exec_pos` (start of `"exec("` in `hay`) is directly preceded by a
/// JS/Node regex literal `/pattern/[flags].` on the same line. Never exempts
/// arbitrary `obj.exec(` (which could be `require('child_process').exec`).
fn is_regex_literal_exec(hay: &str, exec_pos: usize) -> bool {
    let Some(before_dot) = hay[..exec_pos]
        .trim_end_matches([' ', '\t'])
        .strip_suffix('.')
    else {
        return false;
    };
    let before_flags = before_dot.trim_end_matches(['g', 'i', 'm', 's', 'u', 'y', 'd', 'v']);
    if !before_flags.ends_with('/') {
        return false;
    }
    let close_idx = before_flags.len() - 1;
    let line_start = before_flags[..close_idx].rfind('\n').map_or(0, |i| i + 1);
    let (line, target) = (&hay[line_start..=close_idx], close_idx - line_start);
    let (bytes, mut i, mut can_regex) = (line.as_bytes(), 0, true);
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\r' => i += 1,
            q @ (b'\'' | b'"' | b'`') => {
                i += 1;
                while i < bytes.len() && bytes[i] != q {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i = (i + 1).min(bytes.len());
                can_regex = false;
            }
            b'/' if can_regex => {
                let (open, mut in_class) = (i, false);
                i += 1;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i = (i + 2).min(bytes.len()),
                        b'[' => {
                            in_class = true;
                            i += 1;
                        }
                        b']' => {
                            in_class = false;
                            i += 1;
                        }
                        b'/' if !in_class => {
                            if i == target {
                                return i > open + 1;
                            }
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
                can_regex = false;
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'$' => {
                let start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'_' | b'$'))
                {
                    i += 1;
                }
                can_regex = matches!(
                    &line[start..i],
                    "return"
                        | "case"
                        | "throw"
                        | "typeof"
                        | "void"
                        | "delete"
                        | "in"
                        | "of"
                        | "yield"
                        | "await"
                        | "new"
                );
            }
            b'0'..=b'9' | b')' | b']' => {
                i += 1;
                can_regex = false;
            }
            _ => {
                i += 1;
                can_regex = true;
            }
        }
    }
    false
}

/// True when `hay` contains a shell-execution API, exempting ONLY JS regex
/// literal `.exec(` calls (`/pattern/[flags].exec(`).
pub(super) fn has_shell_exec_api(hay: &str) -> bool {
    if has_any(hay, SHELL_EXEC_NON_DOT_EXEC) {
        return true;
    }
    let mut from = 0;
    while let Some(rel) = hay[from..].find("exec(") {
        let pos = from + rel;
        let boundary = hay[..pos]
            .chars()
            .next_back()
            .is_none_or(|c| !is_ident_char(c));
        if boundary && !is_regex_literal_exec(hay, pos) {
            return true;
        }
        from = pos + 5;
    }
    false
}

fn balanced_paren_slice(text: &str, open_end: usize) -> &str {
    let (mut depth, mut quote, mut escaped) = (1usize, None, false);
    for (off, c) in text[open_end..].char_indices() {
        if step_quote(c, &mut quote, &mut escaped, true) {
            continue;
        }
        if c == '(' {
            depth += 1;
        } else if c == ')' {
            depth -= 1;
            if depth == 0 {
                return &text[open_end..open_end + off];
            }
        }
    }
    &text[open_end..]
}

fn stmt_tail_slice(text: &str, start: usize) -> &str {
    let (mut quote, mut escaped) = (None, false);
    for (off, c) in text[start..].char_indices() {
        if step_quote(c, &mut quote, &mut escaped, true) {
            continue;
        }
        if matches!(c, ';' | '\n' | ')' | '}') {
            return &text[start..start + off];
        }
    }
    &text[start..]
}

/// Extracts argument slices of unquoted calls matching `apis`. Also returns
/// `true` if a non-paren API (e.g. `shutil.rmtree`) appears without a call site.
pub(super) fn extract_api_arg_slices<'a>(
    hay: &'a str,
    apis: &[&str],
    allow_space_or_quote: bool,
) -> (Vec<&'a str>, bool) {
    let (mut slices, mut seen, mut uncalled) = (Vec::new(), Vec::new(), false);
    let (mut quote, mut escaped, mut prev) = (None, false, None);
    let mut push = |start: usize, slice: &'a str| {
        if !seen.contains(&start) {
            seen.push(start);
            slices.push(slice);
        }
    };
    for (idx, c) in hay.char_indices() {
        if step_quote(c, &mut quote, &mut escaped, true) {
            prev = Some(c);
            continue;
        }
        let tail = &hay[idx..];
        for api in apis {
            if (api.starts_with(is_ident_char) && prev.is_some_and(is_ident_char))
                || !tail.starts_with(api)
            {
                continue;
            }
            let after = idx + api.len();
            let rest = &hay[after..];
            if api.ends_with('(') {
                push(after, balanced_paren_slice(hay, after));
            } else if rest.starts_with('(') {
                push(after + 1, balanced_paren_slice(hay, after + 1));
            } else if allow_space_or_quote && rest.starts_with([' ', '\t', '\'', '"']) {
                push(after, stmt_tail_slice(hay, after));
            } else if !rest.starts_with(is_ident_char) {
                uncalled = true;
            }
        }
        prev = Some(c);
    }
    (slices, uncalled)
}

fn is_py_ident(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(is_ident_char)
}

/// True when `expr` is a direct `tempfile.mkdtemp(...)` or `TemporaryDirectory(...)`
/// call with no `dir` override and no trailing expression.
fn is_safe_py_mktemp_call(expr: &str) -> bool {
    let norm = normalize_api_haystack(expr.trim(), false);
    let Some(rest) = [
        "tempfile.mkdtemp(",
        "mkdtemp(",
        "tempfile.TemporaryDirectory(",
        "TemporaryDirectory(",
    ]
    .iter()
    .find_map(|p| norm.strip_prefix(p)) else {
        return false;
    };
    let args = balanced_paren_slice(rest, 0);
    if args.len() + 1 != rest.len() || !rest.ends_with(')') {
        return false;
    }
    let trimmed = args.trim();
    trimmed.is_empty()
        || trimmed.split(',').all(|part| {
            let p = part.trim();
            p.is_empty() || p.starts_with("prefix=") || p.starts_with("suffix=")
        })
}

fn scan_top_level(text: &str, backtick: bool, mut on_top: impl FnMut(usize, char) -> bool) {
    let (mut depth, mut quote, mut escaped) = (0usize, None, false);
    for (idx, c) in text.char_indices() {
        if step_quote(c, &mut quote, &mut escaped, backtick) {
            continue;
        }
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ if depth == 0 && on_top(idx, c) => break,
            _ => {}
        }
    }
}

fn split_py_statements(code: &str) -> Vec<&str> {
    let (mut out, mut start) = (Vec::new(), 0);
    scan_top_level(code, false, |idx, c| {
        if matches!(c, '\n' | ';') {
            out.push(&code[start..idx]);
            start = idx + 1;
        }
        false
    });
    out.push(&code[start..]);
    out
}

fn find_top_level_assign(stmt: &str) -> Option<usize> {
    let bytes = stmt.as_bytes();
    let mut found = None;
    scan_top_level(stmt, false, |i, c| {
        if c == '=' {
            let prev = i.checked_sub(1).and_then(|p| bytes.get(p)).copied();
            if bytes.get(i + 1) != Some(&b'=') && !matches!(prev, Some(b'=' | b'!' | b'<' | b'>')) {
                found = Some(i);
                return true;
            }
        }
        false
    });
    found
}

/// Returns Python variable names assigned directly from `tempfile.mkdtemp()` /
/// `TemporaryDirectory()` in `active` and NEVER reassigned anywhere in `active`.
pub(super) fn safe_py_mktemp_vars(active: &str) -> HashSet<String> {
    let (mut assigned, mut tainted) = (HashSet::new(), HashSet::new());
    let taint_idents = |text: &str, tainted: &mut HashSet<String>| {
        tainted.extend(
            text.split(|c: char| !is_ident_char(c))
                .filter(|t| is_py_ident(t))
                .map(str::to_string),
        );
    };
    for stmt in split_py_statements(active)
        .into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if let Some(rest) = stmt.strip_prefix("with ")
            && let Some((ctx_expr, tail)) = rest.rsplit_once(" as ")
        {
            let var = tail.trim().trim_end_matches(':').trim();
            if is_py_ident(var) {
                if is_safe_py_mktemp_call(ctx_expr) && assigned.insert(var.to_string()) {
                    continue;
                }
                tainted.insert(var.to_string());
            }
        } else if let Some((lhs, _)) = stmt.strip_prefix("for ").and_then(|r| r.split_once(" in "))
        {
            taint_idents(lhs, &mut tainted);
        } else if let Some(rest) = stmt.strip_prefix("del ") {
            taint_idents(rest, &mut tainted);
        } else if let Some(eq) = find_top_level_assign(stmt) {
            let (lhs, rhs) = (stmt[..eq].trim(), stmt[eq + 1..].trim());
            let aug = lhs.ends_with(['+', '-', '*', '/', '%', '&', '|', '^', ':']);
            if aug
                || !is_py_ident(lhs)
                || !is_safe_py_mktemp_call(rhs)
                || !assigned.insert(lhs.to_string())
            {
                taint_idents(lhs, &mut tainted);
            }
        }
    }
    assigned.retain(|v| !tainted.contains(v));
    assigned
}

pub(super) fn is_safe_py_mktemp_arg(arg_slice: &str, safe_vars: &HashSet<String>) -> bool {
    let mut end = arg_slice.len();
    scan_top_level(arg_slice, true, |idx, c| {
        if c == ',' {
            end = idx;
            return true;
        }
        false
    });
    let first = arg_slice[..end].trim();
    safe_vars.contains(first)
        || first
            .strip_suffix(".name")
            .is_some_and(|base| safe_vars.contains(base))
}
