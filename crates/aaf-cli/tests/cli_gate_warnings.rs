//! The gate's warnings about the work tree (files on disk that the
//! fingerprint does not cover) are in the receipt's `reasons`. `aaf gate`
//! also shows them where a person or a script looks for warnings.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "cli_support.rs"]
mod support;

use std::fs;

use serde_json::json;
use support::{Sandbox, write};

/// A repository whose gate passes, with one file hidden from the
/// fingerprint through the repository's own exclude file.
fn repo_with_a_hidden_file() -> Sandbox {
    let sb = Sandbox::new();
    write(
        &sb.repo.join(".autofactory/config.toml"),
        "[commands]\ntest_scoped = \"true\"\n",
    );
    support::calm_machine(&sb, 1);
    sb.commit_all("configure");
    fs::write(sb.repo.join(".git/info/exclude"), "notes.local\n").unwrap();
    write(&sb.repo.join("notes.local"), "mine\n");
    sb
}

// Each test starts the real gate, which waits with no time limit while the
// machine is under memory pressure: the guard returns early instead (or
// fails, with AAF_REQUIRE_REAL=1), as in cli_gate.rs.

#[test]
fn work_tree_warnings_are_a_field_of_their_own_and_reasons_stay_as_they_are() {
    if !support::memory_is_calm("work_tree_warnings_are_a_field_of_their_own") {
        return;
    }
    let sb = repo_with_a_hidden_file();

    let (_, out) = sb.json(&["gate"]);

    let warnings = out["warnings"].as_array().expect("a warnings array");
    assert_eq!(warnings.len(), 1, "{out}");
    let warning = warnings[0].as_str().unwrap();
    assert!(warning.starts_with("warning: work tree:"), "{warning}");
    assert!(warning.contains("notes.local"), "{warning}");
    // Additive: the same line is still among the reasons.
    assert!(
        out["reasons"].as_array().unwrap().contains(&json!(warning)),
        "{out}"
    );
}

#[test]
fn a_gate_without_warnings_has_an_empty_list() {
    if !support::memory_is_calm("a_gate_without_warnings_has_an_empty_list") {
        return;
    }
    let sb = Sandbox::new();
    write(
        &sb.repo.join(".autofactory/config.toml"),
        "[commands]\ntest_scoped = \"true\"\n",
    );
    support::calm_machine(&sb, 1);
    sb.commit_all("configure");

    let (_, out) = sb.json(&["gate"]);

    assert_eq!(out["warnings"], json!([]), "{out}");
}

#[test]
fn work_tree_warnings_are_printed_on_stderr_in_text_mode() {
    if !support::memory_is_calm("work_tree_warnings_are_printed_on_stderr") {
        return;
    }
    let sb = repo_with_a_hidden_file();

    let out = sb.aaf(&["gate"]);

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("warning: work tree:"), "{stderr}");
    assert!(stderr.contains("notes.local"), "{stderr}");
}
