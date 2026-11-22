#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]
//! `aaf accounts next` answers for the repository's executor. Seen in the
//! real run: with a Gemini account discovered next to the Claude ones it
//! named the Gemini account, and `aaf run` then used a Claude one.

#[path = "factory_support.rs"]
mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;

use serde_json::json;
use support::Fixture;

/// A fixture whose machine has one Claude Code profile and a Gemini login.
fn two_providers() -> (Fixture, String) {
    let fx = Fixture::new();
    let home = fx.root().join("home");
    // What discovery takes for a Claude Code config dir.
    fs::create_dir_all(home.join(".claude-work/projects")).unwrap();
    fs::write(home.join(".claude-work/.claude.json"), "{}").unwrap();
    fs::create_dir_all(home.join(".gemini")).unwrap();
    fs::write(
        home.join(".config/autofactory/config.toml"),
        "[machine]\nmax_load_percent = 0\n\n[accounts]\ndiscovery_globs = [\"~/.claude-work\"]\n",
    )
    .unwrap();
    // The Gemini login only counts when its binary is on PATH.
    let bin = fx.root().join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(bin.join("gemini"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(bin.join("gemini"), fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    (fx, path)
}

#[test]
fn next_names_an_account_of_the_executors_provider() {
    let (fx, path) = two_providers();
    let env = [("PATH", path.as_str())];

    let listed = fx.aaf_in(&fx.repo, &["accounts", "list"], &env);
    let providers: Vec<&str> = listed.json["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["provider"].as_str().unwrap())
        .collect();
    assert_eq!(
        providers,
        ["antigravity", "claude_code"],
        "{}",
        listed.stderr
    );

    // The repository runs `claude` (the default executor).
    let next = fx.aaf_in(&fx.repo, &["accounts", "next"], &env);
    assert_eq!(next.code, 0, "{}", next.stderr);
    assert_eq!(next.json["account"]["provider"], json!("claude_code"));
    assert_eq!(next.json["account"]["name"], json!("work"));
    assert_eq!(next.json["executor"], json!("claude"));
}

#[test]
fn next_says_so_when_only_other_providers_are_usable() {
    let (fx, path) = two_providers();
    let env = [("PATH", path.as_str())];
    let cooled = fx.aaf_in(&fx.repo, &["accounts", "cooldown", "work"], &env);
    assert_eq!(cooled.code, 0, "{}", cooled.stderr);

    let next = fx.aaf_in(&fx.repo, &["accounts", "next"], &env);

    // Never the Gemini account: `aaf run` would not start on it either.
    assert_eq!(next.code, 2, "{}", next.stderr);
    assert_eq!(next.json["account"], json!(null));
    assert_eq!(next.json["status"], json!("none"));
}
