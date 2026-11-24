//! A13 F7-2: `aaf init --hook-host` decides every host settings merge before
//! the first write, so a refused Codex/Gemini file leaves the repo untouched.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "cli_support.rs"]
mod support;

use std::fs;

use serde_json::json;
use support::{Sandbox, write};

#[test]
fn init_refusing_a_host_hook_file_writes_nothing() {
    for (host, rel) in [
        ("codex", ".codex/hooks.json"),
        ("gemini", ".gemini/settings.json"),
    ] {
        let sb = Sandbox::new();
        write(&sb.repo.join(rel), "[]");

        let (code, out) = sb.json(&["init", "--hook-host", host]);

        assert_eq!(code, 1, "{host}: {out}");
        assert_eq!(out["ok"], json!(false), "{host}: {out}");
        assert_eq!(fs::read_to_string(sb.repo.join(rel)).unwrap(), "[]");
        assert!(
            !sb.repo.join(".autofactory/config.toml").exists(),
            "{host}: config written before the host hook refused"
        );
        assert!(!sb.repo.join(".claude/settings.json").exists(), "{host}");
        assert!(!sb.repo.join(".gitignore").exists(), "{host}");
    }
}

#[test]
fn init_all_hosts_registers_codex_and_gemini_hooks() {
    let sb = Sandbox::new();
    let (code, out) = sb.json(&["init", "--hook-host", "all"]);
    assert_eq!(code, 0, "{out}");
    for rel in [".codex/hooks.json", ".gemini/settings.json"] {
        let text = fs::read_to_string(sb.repo.join(rel)).unwrap();
        assert!(text.contains("aaf-hook"), "{rel}: {text}");
    }
    assert!(sb.repo.join(".autofactory/config.toml").exists());
    // Idempotent: a second run leaves both host files unchanged.
    let (code, out) = sb.json(&["init", "--hook-host", "all"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.to_string().contains("unchanged"), "{out}");
}
