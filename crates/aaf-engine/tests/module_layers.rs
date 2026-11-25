#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Ratchet on the module graph of `aaf-engine` (docs/ARCHITECTURE.md).
//!
//! Every `crate::<module>` reference in production code between two
//! top-level modules must be listed in `ALLOWED`. A new edge fails the test
//! (decide whether the dependency belongs there, then list it on purpose);
//! an edge that disappeared fails too, so the list only ever shrinks and the
//! cycles named in the architecture doc can be removed one at a time.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// `(module, space-separated modules it may use)`: the graph as of the last review.
const ALLOWED: &[(&str, &str)] = &[
    ("cross_audit", "gate store_compliance"),
    ("detector", "process"),
    ("executors", "confine_core git process throttle"),
    (
        "factory",
        "accounts clock config cross_audit db executors gate gc git handover \
         hook_settings plan_debate pr_ops process proof store_compliance throttle \
         unblocker webhook",
    ),
    ("gate", "confine_core git proof store_compliance throttle"),
    ("gc", "config db gate gc_audits git process telemetry"),
    ("gc_audits", "db git"),
    ("git", "confine_core process"),
    ("github_board", "detector factory process"),
    ("github_poll", "factory github_board process"),
    ("handover", "cross_audit db gate git proof"),
    ("hook_settings", "gate"),
    ("plan_debate", "gate"),
    ("pr_ops", "config db gate git proof throttle"),
    ("proof", "config confine_core executors"),
    ("roles", "executors"),
    ("skills", "config"),
    ("split", "db factory git throttle"),
    ("telegram", "git watch webhook"),
    ("telemetry", "accounts clock db throttle"),
    ("throttle", "confine_core process"),
    ("unblocker", "gate store_compliance"),
    ("watch", "accounts db factory telemetry throttle"),
    ("web_ui", "executors git"),
];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "tests") {
                continue;
            }
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let name = path.file_name().unwrap().to_string_lossy();
            if name != "tests.rs" && !name.ends_with("_tests.rs") {
                out.push(path);
            }
        }
    }
}

fn raw_string_end(bytes: &[u8], i: usize) -> Option<usize> {
    if i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_') {
        return None;
    }
    let mut j = i + usize::from(matches!(bytes.get(i), Some(b'b' | b'c')));
    if bytes.get(j) != Some(&b'r') {
        return None;
    }
    j += 1;
    let hashes = bytes[j..].iter().take_while(|&&b| b == b'#').count();
    j += hashes;
    if bytes.get(j) != Some(&b'"') {
        return None;
    }
    let end = (j + 1..bytes.len())
        .find(|&p| bytes[p] == b'"' && (0..hashes).all(|k| bytes.get(p + 1 + k) == Some(&b'#')))
        .map_or(bytes.len(), |p| p + 1 + hashes);
    Some(end)
}

fn char_literal_end(text: &str, i: usize) -> Option<usize> {
    let rest = &text[i + 1..];
    if let Some(after) = rest.strip_prefix('\\') {
        let mut chars = after.char_indices();
        chars.next()?;
        for (off, ch) in chars {
            if ch == '\'' {
                return Some(i + 2 + off + 1);
            }
            if ch == '\n' {
                return None;
            }
        }
        return None;
    }
    let mut chars = rest.char_indices();
    let (_, ch) = chars.next()?;
    if matches!(ch, '\n' | '\r' | '\'') {
        return None;
    }
    let (end_off, closing) = chars.next()?;
    (closing == '\'').then_some(i + 1 + end_off + 1)
}

/// If `text[i..]` starts a comment, string, raw string, or char literal,
/// returns `(end_index, placeholder)`.
fn skip_non_code(text: &str, i: usize) -> Option<(usize, &'static str)> {
    let bytes = text.as_bytes();
    if bytes.get(i..i + 2) == Some(b"//") {
        let end = bytes[i + 2..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(bytes.len(), |p| i + 2 + p);
        return Some((end, ""));
    }
    if bytes.get(i..i + 2) == Some(b"/*") {
        let (mut j, mut depth) = (i + 2, 1usize);
        while j + 1 < bytes.len() && depth > 0 {
            match &bytes[j..j + 2] {
                b"/*" => (depth, j) = (depth + 1, j + 2),
                b"*/" => (depth, j) = (depth - 1, j + 2),
                _ => j += 1,
            }
        }
        return Some((if depth > 0 { bytes.len() } else { j }, " "));
    }
    if let Some(end) = raw_string_end(bytes, i) {
        return Some((end, "\"\""));
    }
    if bytes[i] == b'"' {
        let mut j = i + 1;
        while j < bytes.len() {
            match bytes[j] {
                b'\\' => j += 2,
                b'"' => return Some((j + 1, "\"\"")),
                _ => j += 1,
            }
        }
        return Some((bytes.len(), "\"\""));
    }
    if bytes[i] == b'\''
        && let Some(end) = char_literal_end(text, i)
    {
        return Some((end, "''"));
    }
    None
}

fn skip_ws_and_comments(text: &str, mut i: usize) -> usize {
    let bytes = text.as_bytes();
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
        } else if bytes[i] == b'/'
            && let Some((end, _)) = skip_non_code(text, i)
        {
            i = end;
        } else {
            break;
        }
    }
    i
}

fn parse_outer_attr(text: &str, i: usize) -> Option<(&str, usize)> {
    let bytes = text.as_bytes();
    if bytes.get(i) != Some(&b'#') {
        return None;
    }
    let j = skip_ws_and_comments(text, i + 1);
    if bytes.get(j) != Some(&b'[') {
        return None;
    }
    let (body_start, mut k, mut depth) = (j + 1, j + 1, 1usize);
    while k < bytes.len() && depth > 0 {
        if let Some((end, _)) = skip_non_code(text, k) {
            k = end;
        } else {
            depth = match bytes[k] {
                b'[' => depth + 1,
                b']' => depth - 1,
                _ => depth,
            };
            k += 1;
        }
    }
    (depth == 0).then(|| (&text[body_start..k - 1], k))
}

fn is_cfg_test_attr(body: &str) -> bool {
    let compact: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    compact == "cfg(test)"
        || compact
            .strip_prefix("cfg(all(")
            .and_then(|s| s.strip_suffix("))"))
            .is_some_and(|inner| inner.split(',').any(|part| part == "test"))
}

fn skip_attached_item(text: &str, mut i: usize) -> usize {
    let bytes = text.as_bytes();
    while let Some((_, end)) = parse_outer_attr(text, skip_ws_and_comments(text, i)) {
        i = end;
    }
    i = skip_ws_and_comments(text, i);
    let (mut braces, mut parens, mut brackets, mut saw_use) = (0usize, 0usize, 0usize, false);
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    while i < bytes.len() {
        if let Some((end, _)) = skip_non_code(text, i) {
            i = end;
            continue;
        }
        let at_top = braces == 0 && parens == 0 && brackets == 0;
        if at_top
            && text[i..].starts_with("use")
            && !bytes.get(i + 3).copied().is_some_and(is_ident)
            && (i == 0 || !is_ident(bytes[i - 1]))
        {
            saw_use = true;
            i += 3;
            continue;
        }
        match bytes[i] {
            b'(' => parens += 1,
            b')' => parens = parens.saturating_sub(1),
            b'[' => brackets += 1,
            b']' => brackets = brackets.saturating_sub(1),
            b'{' => braces += 1,
            b'}' if braces > 0 => {
                braces -= 1;
                if braces == 0 && parens == 0 && brackets == 0 && !saw_use {
                    return i + 1;
                }
            }
            b';' if at_top => return i + 1,
            _ => {}
        }
        i += 1;
    }
    bytes.len()
}

/// Strips comments, literal bodies, and only the item attached to each
/// `#[cfg(test)]` attribute, keeping all production code before and after.
fn production_code(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if let Some((end, rep)) = skip_non_code(text, i) {
            out.push_str(rep);
            i = end;
        } else if bytes[i] == b'#'
            && let Some((body, end)) = parse_outer_attr(text, i)
            && is_cfg_test_attr(body)
        {
            i = skip_attached_item(text, end);
            out.push(' ');
        } else {
            let ch = text[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn push_module_ident(ident: &str, out: &mut BTreeSet<String>) {
    if !ident.is_empty()
        && ident != "self"
        && ident
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        let canonical = match ident {
            "handover_dual" | "handover_export" => "handover",
            other => other,
        };
        out.insert(canonical.to_string());
    }
}

fn find_matching_brace(s: &str) -> Option<usize> {
    let mut depth = 1usize;
    for (idx, b) in s.bytes().enumerate() {
        depth = match b {
            b'{' => depth + 1,
            b'}' => depth - 1,
            _ => depth,
        };
        if depth == 0 {
            return Some(idx);
        }
    }
    None
}

fn collect_brace_group_modules(group: &str, out: &mut BTreeSet<String>) {
    let handle = |entry: &str, out: &mut BTreeSet<String>| {
        let trimmed = entry.trim_start();
        if let Some(inner) = trimmed.strip_prefix('{') {
            if let Some(close) = find_matching_brace(inner) {
                collect_brace_group_modules(&inner[..close], out);
            }
            return;
        }
        let ident: String = trimmed
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        push_module_ident(&ident, out);
    };
    let (mut depth, mut start) = (0usize, 0usize);
    for (i, b) in group.bytes().enumerate() {
        match b {
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                handle(&group[start..i], out);
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < group.len() {
        handle(&group[start..], out);
    }
}

fn crate_refs(code: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut rest = code;
    while let Some(pos) = rest.find("crate::") {
        let tail = &rest[pos + "crate::".len()..];
        let preceded = rest[..pos]
            .chars()
            .last()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$');
        if !preceded {
            let trimmed = tail.trim_start();
            if let Some(after) = trimmed.strip_prefix('{') {
                if let Some(close) = find_matching_brace(after) {
                    collect_brace_group_modules(&after[..close], &mut out);
                }
            } else {
                let ident: String = trimmed
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                push_module_ident(&ident, &mut out);
            }
        }
        rest = tail;
    }
    out
}

fn actual_graph() -> BTreeMap<String, BTreeSet<String>> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    let mut graph: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for file in files {
        let first = file
            .strip_prefix(&src)
            .unwrap()
            .components()
            .next()
            .unwrap()
            .as_os_str()
            .to_string_lossy();
        let top = first.trim_end_matches(".rs").to_string();
        if top == "lib" {
            continue;
        }
        let code = production_code(&fs::read_to_string(&file).unwrap());
        for used in crate_refs(&code) {
            if used != top {
                graph.entry(top.clone()).or_default().insert(used);
            }
        }
    }
    graph
}

fn allowed_edges() -> BTreeSet<(String, String)> {
    ALLOWED
        .iter()
        .flat_map(|(m, deps)| {
            deps.split_whitespace()
                .map(move |d| ((*m).to_string(), d.to_string()))
        })
        .collect()
}

fn edges(graph: &BTreeMap<String, BTreeSet<String>>) -> BTreeSet<(String, String)> {
    graph
        .iter()
        .flat_map(|(m, deps)| deps.iter().map(move |d| (m.clone(), d.clone())))
        .collect()
}

#[test]
fn no_new_dependency_between_engine_modules() {
    let actual = edges(&actual_graph());
    let allowed = allowed_edges();
    let added: Vec<String> = actual
        .difference(&allowed)
        .map(|(a, b)| format!("{a} -> {b}"))
        .collect();
    assert!(
        added.is_empty(),
        "new module dependencies in aaf-engine: {added:?}\n\
         If the dependency is right, add it to ALLOWED in this file and to \
         docs/ARCHITECTURE.md; otherwise move the shared code down a layer."
    );
}

#[test]
fn removed_dependencies_are_dropped_from_the_list() {
    let actual = edges(&actual_graph());
    let allowed = allowed_edges();
    let stale: Vec<String> = allowed
        .difference(&actual)
        .map(|(a, b)| format!("{a} -> {b}"))
        .collect();
    assert!(
        stale.is_empty(),
        "these dependencies are gone, remove them from ALLOWED so they cannot come back: {stale:?}"
    );
}

#[test]
fn foundation_modules_stay_at_the_bottom() {
    let graph = actual_graph();
    let foundations = "clock config confine_core process detector webhook telemetry";
    let uppers = "factory plan_debate pr_ops split github_board github_poll watch web_ui";
    for module in foundations.split_whitespace() {
        let deps = graph.get(module).cloned().unwrap_or_default();
        for upper in uppers.split_whitespace() {
            assert!(
                !deps.contains(upper),
                "{module} must not depend on {upper} (found {deps:?})"
            );
        }
    }
}

/// Computes all cycles (SCCs of size >= 2, plus self-loops) via Tarjan's algorithm.
fn tarjan_cycles(graph: &BTreeMap<String, BTreeSet<String>>) -> Vec<Vec<String>> {
    #[derive(Default)]
    struct State {
        next: usize,
        idx: BTreeMap<String, usize>,
        low: BTreeMap<String, usize>,
        stack: Vec<String>,
        on_stack: BTreeSet<String>,
        cycles: Vec<Vec<String>>,
    }
    fn visit(node: &str, graph: &BTreeMap<String, BTreeSet<String>>, s: &mut State) {
        let i = s.next;
        s.next += 1;
        s.idx.insert(node.to_string(), i);
        s.low.insert(node.to_string(), i);
        s.stack.push(node.to_string());
        s.on_stack.insert(node.to_string());
        if let Some(neighbors) = graph.get(node) {
            for next in neighbors {
                if !s.idx.contains_key(next) {
                    visit(next, graph, s);
                    let nl = s.low[next];
                    *s.low.get_mut(node).unwrap() = s.low[node].min(nl);
                } else if s.on_stack.contains(next) {
                    let ni = s.idx[next];
                    *s.low.get_mut(node).unwrap() = s.low[node].min(ni);
                }
            }
        }
        if s.low[node] == s.idx[node] {
            let mut scc = Vec::new();
            while let Some(member) = s.stack.pop() {
                s.on_stack.remove(&member);
                let done = member == node;
                scc.push(member);
                if done {
                    break;
                }
            }
            scc.sort();
            let self_loop =
                scc.len() == 1 && graph.get(&scc[0]).is_some_and(|d| d.contains(&scc[0]));
            if scc.len() >= 2 || self_loop {
                s.cycles.push(scc);
            }
        }
    }
    let nodes: BTreeSet<String> = graph
        .keys()
        .chain(graph.values().flat_map(|d| d.iter()))
        .cloned()
        .collect();
    let mut state = State::default();
    for node in nodes {
        if !state.idx.contains_key(&node) {
            visit(&node, graph, &mut state);
        }
    }
    state.cycles.sort();
    state.cycles
}

#[test]
fn no_cycles_in_engine_module_graph() {
    let cycles = tarjan_cycles(&actual_graph());
    assert!(
        cycles.is_empty(),
        "dependency cycle(s) detected in aaf-engine module graph: {cycles:?}\n\
         Extract shared primitives down to a foundation module (e.g. `confine_core` or `aaf-domain`) \
         or move orchestration up instead of introducing a cycle."
    );
}

#[test]
fn tarjan_scc_detects_cycles_and_accepts_dags() {
    let mk = |items: &[(&str, &str)]| -> BTreeMap<String, BTreeSet<String>> {
        items
            .iter()
            .map(|(k, v)| {
                (
                    (*k).to_string(),
                    v.split_whitespace().map(str::to_string).collect(),
                )
            })
            .collect()
    };
    let dag = mk(&[("a", "b c"), ("b", "d"), ("c", "d"), ("d", "")]);
    assert!(tarjan_cycles(&dag).is_empty());
    let cyclic = mk(&[("a", "b"), ("b", "c"), ("c", "a"), ("x", "y"), ("y", "x")]);
    assert_eq!(
        tarjan_cycles(&cyclic),
        vec![vec!["a", "b", "c"], vec!["x", "y"]]
    );
}

#[test]
fn reference_scanner_ignores_macro_paths_and_comments() {
    let code = production_code(
        "use crate::db::Db;\n// crate::factory in a comment\n$crate::gate::x();\n\
         fn f() { crate::git::run() }\n#[cfg(test)]\nuse crate::throttle;",
    );
    let refs: Vec<String> = crate_refs(&code).into_iter().collect();
    assert_eq!(refs, ["db", "git"]);
}

#[test]
fn scanner_preserves_production_code_after_cfg_test_items() {
    let src = r##"
#[cfg(test)]
mod tests;
#[cfg(test)]
#[allow(unused)]
mod inline_tests {
    use crate::secret::hidden;
    fn tricky() { let _ = "}"; let _ = '}'; /* } */ let _ = r#"} crate::raw"#; }
}
#[cfg(test)]
use crate::{test_only_a, test_only_b::Thing};
use crate::after_mod::Visible;
pub fn work() { let _ = (crate::handover_dual::x(), crate::handover_export::y()); }
"##;
    let refs: Vec<String> = crate_refs(&production_code(src)).into_iter().collect();
    assert_eq!(refs, ["after_mod", "handover"]);
}

#[test]
fn scanner_extracts_brace_grouped_and_multiline_crate_imports() {
    let src = "use crate::{\n    foo,\n    bar::baz,\n    qux::{nested_a, nested_b},\n    handover_dual as hd,\n    self,\n};";
    let refs: Vec<String> = crate_refs(&production_code(src)).into_iter().collect();
    assert_eq!(refs, ["bar", "foo", "handover", "qux"]);
}
