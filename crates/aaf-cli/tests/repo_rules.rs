#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Workspace-wide rules from docs/PLAN.md section B, enforced as a test so
//! they hold on every `cargo test`, not only in review.
//!
//! The source checks are line based on purpose: no parser dependency, and a
//! false positive (the word inside a string literal) is cheap to reword.

use std::fs;
use std::path::{Path, PathBuf};

const MAX_RS_LINES: usize = 600;
/// Never descended into, at any depth.
const SKIP_DIRS: [&str; 3] = ["target", ".claude", ".git"];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// All files under `dir` (sorted, so reports are stable), skipping `SKIP_DIRS`.
fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if path.is_dir() {
            if !SKIP_DIRS.contains(&name.as_str()) {
                walk(&path, out);
            }
        } else {
            out.push(path);
        }
    }
}

fn files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/")
}

fn is_rs(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "rs")
}

/// Test code: anything in a `tests`/`benches` directory or a `tests.rs` file.
fn is_test_source(rel_path: &str) -> bool {
    rel_path
        .split('/')
        .any(|part| matches!(part, "tests" | "benches" | "tests.rs"))
}

fn too_long(root: &Path, limit: usize) -> Vec<String> {
    files(&root.join("crates"))
        .iter()
        .filter(|path| is_rs(path))
        .filter_map(|path| {
            let lines = fs::read_to_string(path).unwrap().lines().count();
            (lines > limit).then(|| format!("{}: {lines} lines (limit {limit})", rel(root, path)))
        })
        .collect()
}

/// `[lints]` followed by `workspace = true` before the next table header.
fn inherits_workspace_lints(manifest: &str) -> bool {
    let mut in_lints = false;
    for line in manifest.lines() {
        let line: String = line
            .split('#')
            .next()
            .unwrap_or("")
            .split_whitespace()
            .collect();
        if line.starts_with('[') {
            in_lints = line == "[lints]";
        } else if (in_lints && line == "workspace=true") || line == "lints.workspace=true" {
            return true;
        }
    }
    false
}

/// Manifests of our own crates: `crates/<name>/Cargo.toml`. A manifest
/// deeper down belongs to a fixture project used as test data.
fn crate_manifests(root: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(root.join("crates"))
        .unwrap_or_else(|e| panic!("read_dir {}/crates: {e}", root.display()))
        .map(|entry| entry.unwrap().path())
        .collect();
    dirs.sort();
    dirs.into_iter()
        .map(|dir| dir.join("Cargo.toml"))
        .filter(|manifest| manifest.is_file())
        .collect()
}

/// Owner decision 2026-11-22 (SECURITY.md §10): ONE module may use `unsafe`,
/// the reaper's `sandbox_check` binding. Its crate turns `forbid` into
/// `deny` (a module can only override `deny`) and must keep every other
/// workspace lint; no other file or manifest is exempt.
const UNSAFE_EXEMPT_SOURCE: &str = "crates/aaf-engine/src/executors/sandbox_check.rs";
const UNSAFE_EXEMPT_MANIFEST: &str = "crates/aaf-engine/Cargo.toml";

/// The exempt manifest: `unsafe_code = "deny"` and every clippy lint of the
/// workspace, word for word.
fn keeps_workspace_lints_but_deny_unsafe(root: &Path, manifest: &str) -> bool {
    let workspace = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let clippy: Vec<&str> = workspace
        .split("[workspace.lints.clippy]")
        .nth(1)
        .unwrap_or("")
        .lines()
        .take_while(|line| !line.trim_start().starts_with('['))
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    !clippy.is_empty()
        && manifest.contains("[lints.rust]\nunsafe_code = \"deny\"\n")
        && manifest.contains("[lints.clippy]")
        && clippy.iter().all(|line| manifest.contains(line))
}

fn missing_lints(root: &Path) -> Vec<String> {
    crate_manifests(root)
        .iter()
        .filter(|path| {
            let text = fs::read_to_string(path).unwrap();
            if rel(root, path) == UNSAFE_EXEMPT_MANIFEST {
                return !keeps_workspace_lints_but_deny_unsafe(root, &text);
            }
            !inherits_workspace_lints(&text)
        })
        .map(|path| format!("{}: no `[lints] workspace = true`", rel(root, path)))
        .collect()
}

/// Whether `word` occurs in `line` as a whole identifier.
fn word_positions(line: &str, word: &str) -> Vec<usize> {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    line.match_indices(word)
        .filter(|(at, _)| {
            let before = line[..*at].chars().next_back();
            let after = line[at + word.len()..].chars().next();
            !before.is_some_and(is_ident) && !after.is_some_and(is_ident)
        })
        .map(|(at, _)| at)
        .collect()
}

/// `unsafe` used as a keyword: a block, fn, impl, trait or extern.
fn has_unsafe_keyword(code: &str) -> bool {
    word_positions(code, "unsafe").into_iter().any(|at| {
        let after = code[at + "unsafe".len()..].trim_start();
        after.is_empty()
            || ["{", "fn", "impl", "trait", "extern"]
                .iter()
                .any(|next| after.starts_with(next))
    })
}

/// `tokio` named as a crate path: `use tokio`, `tokio::`, `extern crate tokio`.
fn imports_tokio(code: &str) -> bool {
    ["tokio", "tokio_util", "tokio_stream"].iter().any(|name| {
        word_positions(code, name).into_iter().any(|at| {
            let before = code[..at].trim_end();
            let after = code[at + name.len()..].trim_start();
            after.starts_with("::")
                || after.starts_with(';')
                || before.ends_with("use")
                || before.ends_with("crate")
                || before.ends_with("::")
                || before.ends_with('{')
        })
    })
}

fn forbidden_in_sources(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for path in files(&root.join("crates")) {
        let rel_path = rel(root, &path);
        if !is_rs(&path) || is_test_source(&rel_path) {
            continue;
        }
        let text = fs::read_to_string(&path).unwrap();
        for (index, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            if has_unsafe_keyword(code) && rel_path != UNSAFE_EXEMPT_SOURCE {
                out.push(format!("{rel_path}:{}: `unsafe` is forbidden", index + 1));
            }
            if imports_tokio(code) {
                out.push(format!("{rel_path}:{}: tokio is forbidden", index + 1));
            }
        }
    }
    out
}

/// tokio declared by the workspace manifest or one of our crates. The lock
/// file is not consulted: the rule is about what we depend on and import,
/// not about what a third-party crate pulls in.
fn tokio_dependencies(root: &Path) -> Vec<String> {
    let mut manifests = vec![root.join("Cargo.toml")];
    manifests.extend(crate_manifests(root));
    let declares = |line: &str| {
        line.starts_with("tokio")
            || line.contains("dependencies.tokio")
            || line.contains("package = \"tokio")
    };
    let mut out = Vec::new();
    for path in manifests {
        let text = fs::read_to_string(&path).unwrap();
        for (index, line) in text.lines().enumerate() {
            if declares(line.trim()) {
                out.push(format!(
                    "{}:{}: tokio dependency: {}",
                    rel(root, &path),
                    index + 1,
                    line.trim()
                ));
            }
        }
    }
    out
}

fn assert_clean(rule: &str, violations: &[String]) {
    assert!(
        violations.is_empty(),
        "{rule}: {} violation(s)\n  {}",
        violations.len(),
        violations.join("\n  ")
    );
}

// --- the real workspace ---------------------------------------------------

#[test]
fn workspace_is_found_and_has_crates() {
    let root = workspace_root();
    assert!(root.join("Cargo.toml").is_file(), "{}", root.display());
    let rs = files(&root.join("crates"))
        .iter()
        .filter(|p| is_rs(p))
        .count();
    // An empty walk would make every rule below pass vacuously.
    assert!(rs >= 5, "only {rs} .rs files found under crates/");
}

#[test]
fn no_rs_file_exceeds_600_lines() {
    assert_clean("file length", &too_long(&workspace_root(), MAX_RS_LINES));
}

#[test]
fn every_crate_inherits_workspace_lints() {
    assert_clean("workspace lints", &missing_lints(&workspace_root()));
}

#[test]
fn no_unsafe_and_no_tokio_in_non_test_sources() {
    assert_clean(
        "forbidden constructs",
        &forbidden_in_sources(&workspace_root()),
    );
}

#[test]
fn no_tokio_in_our_manifests() {
    assert_clean("tokio dependency", &tokio_dependencies(&workspace_root()));
}

// --- the checks themselves, against throwaway trees -------------------------

fn write(root: &Path, rel_path: &str, text: &str) {
    let path = root.join(rel_path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

const GOOD_MANIFEST: &str = "[package]\nname = \"x\"\n\n[lints]\nworkspace = true\n";

#[test]
fn length_check_reports_only_files_over_the_limit() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "crates/a/src/ok.rs", &"// line\n".repeat(600));
    write(tmp.path(), "crates/a/src/big.rs", &"// line\n".repeat(601));
    write(
        tmp.path(),
        "crates/a/tests/big_test.rs",
        &"// line\n".repeat(700),
    );
    write(tmp.path(), "crates/a/notes.md", &"line\n".repeat(900));
    // Skipped directories and files outside crates/ are not counted.
    write(
        tmp.path(),
        "crates/a/target/gen.rs",
        &"// line\n".repeat(900),
    );
    write(
        tmp.path(),
        "crates/a/.claude/copy.rs",
        &"// line\n".repeat(900),
    );
    write(tmp.path(), "tools/big.rs", &"// line\n".repeat(900));

    assert_eq!(
        too_long(tmp.path(), MAX_RS_LINES),
        vec![
            "crates/a/src/big.rs: 601 lines (limit 600)".to_string(),
            "crates/a/tests/big_test.rs: 700 lines (limit 600)".to_string(),
        ]
    );
}

#[test]
fn lints_check_wants_workspace_true_inside_the_lints_table() {
    for good in [
        GOOD_MANIFEST,
        "[lints]\nworkspace=true # inherited\n",
        "[lints]\n\n  workspace   =   true\n[dependencies]\n",
        "lints.workspace = true\n[package]\n",
    ] {
        assert!(inherits_workspace_lints(good), "{good:?}");
    }
    for bad in [
        "[package]\nname = \"x\"\n",
        "[lints]\n[dependencies]\nworkspace = true\n",
        "[lints]\nworkspace = false\n",
        "[lints]\n# workspace = true\n",
        "[package]\nworkspace = true\n",
        "[lints.rust]\nworkspace = true\n",
    ] {
        assert!(!inherits_workspace_lints(bad), "{bad:?}");
    }

    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "crates/good/Cargo.toml", GOOD_MANIFEST);
    write(
        tmp.path(),
        "crates/bad/Cargo.toml",
        "[package]\nname = \"bad\"\n",
    );
    // A manifest deeper down is test data (a fixture project), not our crate.
    write(
        tmp.path(),
        "crates/good/tests/fixtures/app/Cargo.toml",
        "[package]\nname = \"fixture\"\n",
    );
    assert_eq!(
        missing_lints(tmp.path()),
        vec!["crates/bad/Cargo.toml: no `[lints] workspace = true`".to_string()]
    );
}

#[test]
fn unsafe_detection_matches_the_keyword_only() {
    for bad in [
        "let x = unsafe { *ptr };",
        "unsafe fn f() {}",
        "pub unsafe fn f() {}",
        "unsafe impl Send for X {}",
        "unsafe extern \"C\" {}",
        "let y = unsafe",
        "unsafe{ go() }",
    ] {
        assert!(has_unsafe_keyword(bad), "{bad:?}");
    }
    for good in [
        "#![forbid(unsafe_code)]",
        "let unsafe_count = 1;",
        "fn is_unsafe_path() {}",
        "let not_unsafe = true;",
        "",
    ] {
        assert!(!has_unsafe_keyword(good), "{good:?}");
    }
}

#[test]
fn tokio_detection_matches_crate_paths_only() {
    for bad in [
        "use tokio::sync::mpsc;",
        "use tokio;",
        "extern crate tokio;",
        "let rt = tokio::runtime::Runtime::new();",
        "pub use ::tokio::task;",
        "use {tokio::time, std::fmt};",
        "use tokio_util::codec;",
    ] {
        assert!(imports_tokio(bad), "{bad:?}");
    }
    for good in [
        "let executor_name = \"not tokio based\";",
        "fn tokio_free() {}",
        "let no_tokio = true;",
        "use std::thread;",
    ] {
        assert!(!imports_tokio(good), "{good:?}");
    }
}

#[test]
fn source_check_skips_comments_and_test_code() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "crates/a/src/lib.rs",
        "// unsafe { } and use tokio::x; are only named in a comment\nfn ok() {}\n",
    );
    write(
        tmp.path(),
        "crates/a/src/bad.rs",
        "fn f(p: *const u8) -> u8 {\n    unsafe { *p }\n}\nuse tokio::task;\n",
    );
    write(tmp.path(), "crates/a/src/x/tests.rs", "use tokio::task;\n");
    write(tmp.path(), "crates/a/tests/it.rs", "unsafe fn t() {}\n");
    write(
        tmp.path(),
        "crates/a/target/debug/gen.rs",
        "unsafe fn t() {}\n",
    );

    assert_eq!(
        forbidden_in_sources(tmp.path()),
        vec![
            "crates/a/src/bad.rs:2: `unsafe` is forbidden".to_string(),
            "crates/a/src/bad.rs:4: tokio is forbidden".to_string(),
        ]
    );
}

#[test]
fn dependency_check_reads_our_manifests_only() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "Cargo.toml", "[workspace]\nmembers = []\n");
    write(tmp.path(), "crates/good/Cargo.toml", GOOD_MANIFEST);
    // What a third-party crate pulls in is not ours to forbid, and a fixture
    // project's manifest is test data.
    write(
        tmp.path(),
        "Cargo.lock",
        "[[package]]\nname = \"tokio\"\nversion = \"1.0.0\"\n",
    );
    write(
        tmp.path(),
        "crates/good/tests/fixtures/app/Cargo.toml",
        "[dependencies]\ntokio = \"1\"\n",
    );
    assert_eq!(tokio_dependencies(tmp.path()), Vec::<String>::new());

    write(
        tmp.path(),
        "crates/bad/Cargo.toml",
        "[dependencies]\ntokio = { version = \"1\" }\nrt = { package = \"tokio\", version = \"1\" }\n\n[dev-dependencies]\ntokio-util = \"0.7\"\n",
    );
    write(
        tmp.path(),
        "Cargo.toml",
        "[workspace]\nmembers = []\n[workspace.dependencies.tokio]\nversion = \"1\"\n",
    );
    assert_eq!(
        tokio_dependencies(tmp.path()),
        vec![
            "Cargo.toml:3: tokio dependency: [workspace.dependencies.tokio]".to_string(),
            "crates/bad/Cargo.toml:2: tokio dependency: tokio = { version = \"1\" }".to_string(),
            "crates/bad/Cargo.toml:3: tokio dependency: rt = { package = \"tokio\", version = \"1\" }"
                .to_string(),
            "crates/bad/Cargo.toml:6: tokio dependency: tokio-util = \"0.7\"".to_string(),
        ]
    );
}
