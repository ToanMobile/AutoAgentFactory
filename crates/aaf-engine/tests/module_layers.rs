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

/// `(module, modules it may use)`: the graph as of the last review.
const ALLOWED: &[(&str, &[&str])] = &[
    ("accounts", &["db"]),
    ("config", &["git"]),
    ("cross_audit", &["gate", "plan_debate", "store_compliance"]),
    ("db", &["accounts", "cross_audit", "factory", "unblocker"]),
    ("detector", &["process"]),
    ("executors", &["db", "git", "throttle"]),
    (
        "factory",
        &[
            "accounts",
            "clock",
            "config",
            "cross_audit",
            "db",
            "executors",
            "gate",
            "gc",
            "git",
            "handover",
            "hook_settings",
            "plan_debate",
            "process",
            "proof",
            "store_compliance",
            "throttle",
            "unblocker",
            "webhook",
        ],
    ),
    ("gate", &["executors", "git", "proof", "throttle"]),
    (
        "gc",
        &[
            "config",
            "db",
            "gate",
            "gc_audits",
            "git",
            "process",
            "telemetry",
        ],
    ),
    ("gc_audits", &["db", "git"]),
    (
        "git",
        &["config", "db", "gate", "process", "proof", "throttle"],
    ),
    ("github_board", &["detector", "factory", "process"]),
    ("github_poll", &["factory", "github_board", "process"]),
    (
        "handover",
        &[
            "cross_audit",
            "db",
            "gate",
            "handover_dual",
            "handover_export",
            "handover_html",
            "proof",
        ],
    ),
    (
        "handover_devkit",
        &["handover", "handover_dual", "handover_html"],
    ),
    ("handover_dual", &["cross_audit", "handover_html"]),
    ("handover_export", &["db", "git", "handover"]),
    (
        "handover_html",
        &["cross_audit", "git", "handover", "handover_devkit", "proof"],
    ),
    ("hook_settings", &["gate"]),
    ("plan_debate", &["cross_audit", "factory", "gate"]),
    ("proof", &["config", "db", "executors", "gate"]),
    ("roles", &["executors"]),
    ("skills", &["config"]),
    ("split", &["db", "factory", "git", "throttle"]),
    ("telegram", &["git", "webhook"]),
    ("telemetry", &["accounts", "clock", "db"]),
    ("throttle", &["db", "gate", "process", "telemetry"]),
    ("unblocker", &["gate", "store_compliance"]),
    (
        "watch",
        &["accounts", "db", "factory", "telemetry", "throttle"],
    ),
    ("web_ui", &["executors", "git"]),
];

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            // Test-only modules live in `tests/` directories or `tests.rs`.
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

/// Production text of a file: everything before the first `#[cfg(test)]`,
/// without `//` comment lines.
fn production_code(text: &str) -> String {
    let before_tests = text.split("#[cfg(test)]").next().unwrap_or("");
    before_tests
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn crate_refs(code: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut rest = code;
    while let Some(pos) = rest.find("crate::") {
        let tail = &rest[pos + "crate::".len()..];
        let ident: String = tail
            .chars()
            .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_')
            .collect();
        // `$crate::` and `super::crate::` are not ours; a bare `crate::` is.
        let preceded_by_ident = rest[..pos]
            .chars()
            .last()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$');
        if !ident.is_empty() && !preceded_by_ident {
            out.insert(ident);
        }
        rest = tail;
    }
    out
}

fn actual_graph() -> BTreeMap<String, BTreeSet<String>> {
    let src = src_dir();
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    let mut graph: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for file in files {
        let rel = file.strip_prefix(&src).unwrap();
        let first = rel
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

fn allowed_graph() -> BTreeMap<String, BTreeSet<String>> {
    ALLOWED
        .iter()
        .map(|(m, deps)| {
            let deps = deps.iter().map(|d| (*d).to_string()).collect();
            ((*m).to_string(), deps)
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
    let allowed = edges(&allowed_graph());
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
    let allowed = edges(&allowed_graph());
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
    // Modules everything else may build on: they must not reach up into the
    // orchestration layer.
    for module in ["clock", "process", "detector", "webhook", "telemetry"] {
        let deps = graph.get(module).cloned().unwrap_or_default();
        for upper in [
            "factory",
            "plan_debate",
            "github_board",
            "github_poll",
            "web_ui",
        ] {
            assert!(
                !deps.contains(upper),
                "{module} must not depend on {upper} (found {deps:?})"
            );
        }
    }
}

#[test]
fn reference_scanner_ignores_macro_paths_and_comments() {
    let code = production_code(
        "use crate::db::Db;\n// crate::factory in a comment\n$crate::gate::x();\nfn f() { crate::git::run() }\n#[cfg(test)]\nuse crate::throttle;",
    );
    let refs: Vec<String> = crate_refs(&code).into_iter().collect();
    assert_eq!(refs, ["db", "git"]);
}
