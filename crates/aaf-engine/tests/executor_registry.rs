#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::ffi::OsString;
use std::fs;
use std::path::Path;

use aaf_engine::executors::registry::{
    HookSupport, build_headless_argv, builtin_executors, discover_inventory, find_descriptor,
    resolve_with_fallback,
};

fn create_fake_bin(dir: &Path, name: &str) {
    fs::write(dir.join(name), "#!/bin/sh\nexit 0\n").unwrap();
}

fn path_for(dirs: &[&Path]) -> OsString {
    std::env::join_paths(dirs).unwrap()
}

#[test]
fn all_ten_builtin_executors_exist_with_unique_ids_and_hook_support() {
    let all = builtin_executors();
    assert_eq!(all.len(), 10);

    let ids: Vec<&str> = all.iter().map(|d| d.id).collect();
    assert_eq!(
        ids,
        vec![
            "claude",
            "codex",
            "gemini",
            "opencode",
            "cursor",
            "antigravity",
            "codebuddy",
            "kimi",
            "pi",
            "deepseek",
        ]
    );

    assert_eq!(
        find_descriptor("claude").unwrap().hook_support,
        HookSupport::ClaudePreToolUse
    );
    assert_eq!(
        find_descriptor("codex").unwrap().hook_support,
        HookSupport::CodexPreToolUse
    );
    assert_eq!(
        find_descriptor("gemini").unwrap().hook_support,
        HookSupport::GeminiBeforeTool
    );
    assert_eq!(
        find_descriptor("gemini").unwrap().headless_env,
        &[("GEMINI_CLI_TRUST_WORKSPACE", "true")]
    );
    for id in [
        "opencode",
        "cursor",
        "antigravity",
        "codebuddy",
        "kimi",
        "pi",
        "deepseek",
    ] {
        assert_eq!(
            find_descriptor(id).unwrap().hook_support,
            HookSupport::SandboxOnly,
            "{id}"
        );
    }
}

#[test]
fn find_descriptor_resolves_canonical_ids_and_case_insensitive_aliases() {
    let cases = [
        ("claude", "claude"),
        ("CLAUDE-CODE", "claude"),
        ("claudecode", "claude"),
        ("codex-cli", "codex"),
        ("OpenAI-Codex", "codex"),
        ("gemini-cli", "gemini"),
        ("open-code", "opencode"),
        ("cursor-cli", "cursor"),
        ("cursor-agent", "cursor"),
        ("agent", "cursor"),
        ("agy", "antigravity"),
        ("AGY", "antigravity"),
        ("antigravity-cli", "antigravity"),
        ("code-buddy", "codebuddy"),
        ("kimi-code", "kimi"),
        ("pi-agent", "pi"),
        ("dsh", "deepseek"),
        ("DEEPSEEK-HARNESS", "deepseek"),
    ];
    for (input, expected) in cases {
        let desc = find_descriptor(input).unwrap_or_else(|| panic!("missing alias {input:?}"));
        assert_eq!(desc.id, expected, "alias {input:?}");
    }

    for invalid in ["", "   ", "unknown", "custom", "../claude"] {
        assert!(find_descriptor(invalid).is_none(), "{invalid:?}");
    }
}

#[test]
fn build_headless_argv_builds_valid_args_for_all_ten_clis() {
    for desc in builtin_executors() {
        let bin = format!("/usr/local/bin/{}", desc.binaries[0]);
        let argv = build_headless_argv(
            desc,
            &bin,
            "implement feature",
            Some("model-v1"),
            Some("high"),
            true,
        )
        .unwrap_or_else(|e| panic!("failed for {}: {e}", desc.id));
        assert_eq!(argv[0], bin, "{}", desc.id);
        assert_eq!(argv.last().map(String::as_str), Some("implement feature"));
        assert!(argv.contains(&"--model".to_string()), "{}", desc.id);
        assert!(argv.contains(&"model-v1".to_string()), "{}", desc.id);
    }

    let claude = find_descriptor("claude").unwrap();
    let claude_ro = build_headless_argv(
        claude,
        "/bin/claude",
        "-leading-dash prompt",
        Some("claude-sonnet-4-6"),
        Some("medium"),
        true,
    )
    .unwrap();
    assert_eq!(
        claude_ro,
        vec![
            "/bin/claude",
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--permission-mode",
            "plan",
            "--model",
            "claude-sonnet-4-6",
            "--effort",
            "medium",
            "--",
            "-leading-dash prompt",
        ]
    );

    let codex = find_descriptor("codex").unwrap();
    let codex_ro =
        build_headless_argv(codex, "/bin/codex", "audit", Some("o4-mini"), None, true).unwrap();
    assert_eq!(
        codex_ro,
        vec![
            "/bin/codex",
            "exec",
            "--sandbox",
            "read-only",
            "--model",
            "o4-mini",
            "--",
            "audit",
        ]
    );

    let gemini = find_descriptor("gemini").unwrap();
    let gemini_ro = build_headless_argv(gemini, "/bin/gemini", "review", None, None, true).unwrap();
    assert_eq!(
        gemini_ro,
        vec!["/bin/gemini", "--approval-mode", "plan", "-p", "review"]
    );

    let pi = find_descriptor("pi").unwrap();
    let pi_args =
        build_headless_argv(pi, "/bin/pi", "fix", Some("sonnet"), Some("high"), false).unwrap();
    assert_eq!(
        pi_args,
        vec![
            "/bin/pi",
            "--model",
            "sonnet",
            "--thinking",
            "high",
            "-p",
            "fix"
        ]
    );
}

#[test]
fn build_headless_argv_rejects_flag_injection_and_whitespace() {
    let claude = find_descriptor("claude").unwrap();
    let codex = find_descriptor("codex").unwrap();

    for bad_model in [
        "--model=-evil",
        "-evil",
        "sonnet --danger",
        "   ",
        "sonnet\n--exec",
        "sonnet\tflag",
    ] {
        let err = build_headless_argv(claude, "/bin/claude", "ok", Some(bad_model), None, false)
            .unwrap_err();
        assert!(err.contains("model"), "bad_model={bad_model:?}, err={err}");
    }

    for bad_effort in ["-high", "--effort=max", "high low", "  ", "low\r"] {
        // Also rejected on CLIs that do not emit an effort flag.
        let err = build_headless_argv(codex, "/bin/codex", "ok", None, Some(bad_effort), false)
            .unwrap_err();
        assert!(
            err.contains("effort"),
            "bad_effort={bad_effort:?}, err={err}"
        );
    }

    assert!(build_headless_argv(claude, "", "ok", None, None, false).is_err());
    assert!(build_headless_argv(claude, "/bin/claude\n", "ok", None, None, false).is_err());
    assert!(build_headless_argv(claude, "/bin/claude", "   ", None, None, false).is_err());
}

#[test]
fn discover_inventory_finds_fake_binaries_on_synthetic_path() {
    let tmp = tempfile::tempdir().unwrap();
    create_fake_bin(tmp.path(), "claude");
    create_fake_bin(tmp.path(), "agent");
    create_fake_bin(tmp.path(), "agy");
    create_fake_bin(tmp.path(), "dsh");

    let synthetic_path = path_for(&[tmp.path()]);
    let inventory = discover_inventory(&synthetic_path);
    assert_eq!(inventory.len(), 10);

    let by_id = |id: &str| inventory.iter().find(|item| item.id == id).unwrap();

    assert!(by_id("claude").installed);
    assert_eq!(
        by_id("claude").resolved_binary.as_deref(),
        Some(tmp.path().join("claude").as_path())
    );
    assert!(by_id("cursor").installed);
    assert_eq!(
        by_id("cursor").resolved_binary.as_deref(),
        Some(tmp.path().join("agent").as_path())
    );
    assert!(by_id("antigravity").installed);
    assert_eq!(
        by_id("antigravity").resolved_binary.as_deref(),
        Some(tmp.path().join("agy").as_path())
    );
    assert!(by_id("deepseek").installed);
    assert_eq!(
        by_id("deepseek").resolved_binary.as_deref(),
        Some(tmp.path().join("dsh").as_path())
    );

    assert!(!by_id("codex").installed);
    assert!(by_id("codex").resolved_binary.is_none());
    assert_eq!(by_id("gemini").headless_env_count, 1);
    assert_eq!(by_id("claude").headless_env_count, 0);

    let empty = discover_inventory(&OsString::new());
    assert_eq!(empty.len(), 10);
    assert!(empty.iter().all(|item| !item.installed));
}

#[test]
fn resolve_with_fallback_picks_primary_when_available() {
    let tmp = tempfile::tempdir().unwrap();
    create_fake_bin(tmp.path(), "claude");
    create_fake_bin(tmp.path(), "codex");

    let resolved = resolve_with_fallback(
        "claude-code",
        &["codex".to_string(), "gemini".to_string()],
        &path_for(&[tmp.path()]),
        &[],
    )
    .unwrap();

    assert_eq!(resolved.descriptor.id, "claude");
    assert_eq!(resolved.binary_path, tmp.path().join("claude"));
    assert!(!resolved.used_fallback);
    assert!(resolved.skipped.is_empty());
}

#[test]
fn resolve_with_fallback_skips_missing_and_unavailable_with_audit_trail() {
    let tmp = tempfile::tempdir().unwrap();
    create_fake_bin(tmp.path(), "claude");
    create_fake_bin(tmp.path(), "gemini");

    // Primary is installed but cooling down; first fallback `codex` is missing;
    // second fallback `gemini` succeeds.
    let resolved = resolve_with_fallback(
        "claude",
        &["codex".to_string(), "gemini".to_string()],
        &path_for(&[tmp.path()]),
        &["claude-code"],
    )
    .unwrap();

    assert_eq!(resolved.descriptor.id, "gemini");
    assert_eq!(resolved.binary_path, tmp.path().join("gemini"));
    assert!(resolved.used_fallback);
    assert_eq!(resolved.skipped.len(), 2);
    assert_eq!(resolved.skipped[0].0, "claude");
    assert!(resolved.skipped[0].1.contains("unavailable"));
    assert_eq!(resolved.skipped[1].0, "codex");
    assert!(resolved.skipped[1].1.contains("not found on PATH"));

    // Unknown primary falls back to alias `dsh` -> `deepseek`.
    create_fake_bin(tmp.path(), "dsh");
    let alias_fallback = resolve_with_fallback(
        "bogus-cli",
        &["dsh".to_string()],
        &path_for(&[tmp.path()]),
        &[],
    )
    .unwrap();
    assert_eq!(alias_fallback.descriptor.id, "deepseek");
    assert!(alias_fallback.used_fallback);
    assert_eq!(
        alias_fallback.skipped,
        vec![("bogus-cli".to_string(), "unknown executor".to_string())]
    );
}

#[test]
fn resolve_with_fallback_errors_when_all_candidates_unavailable() {
    let tmp = tempfile::tempdir().unwrap();
    create_fake_bin(tmp.path(), "claude");

    let err = resolve_with_fallback(
        "claude",
        &["codex".to_string(), "nonexistent".to_string()],
        &path_for(&[tmp.path()]),
        &["claude"],
    )
    .unwrap_err();

    assert!(err.contains("claude"), "{err}");
    assert!(err.contains("unavailable"), "{err}");
    assert!(err.contains("codex"), "{err}");
    assert!(err.contains("not found on PATH"), "{err}");
    assert!(err.contains("nonexistent"), "{err}");
    assert!(err.contains("unknown executor"), "{err}");
}
