#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;

use aaf_domain::account::ProviderKind;
use aaf_domain::config::AccountsConfig;
use aaf_engine::accounts::{DiscoveryEnv, binary_on_path_in, discover};

/// Lays out the marker entries of a real Claude Code config dir.
fn claude_dir(path: &Path) {
    fs::create_dir_all(path.join("projects")).unwrap();
    fs::write(path.join(".claude.json"), "{}").unwrap();
}

fn env<'a>(home: &Path, on_path: &'a dyn Fn(&str) -> bool) -> DiscoveryEnv<'a> {
    DiscoveryEnv {
        home: home.to_path_buf(),
        binary_on_path: on_path,
    }
}

fn dirs(found: &[aaf_domain::account::AccountProfile]) -> Vec<(ProviderKind, String)> {
    found
        .iter()
        .map(|p| {
            let name = Path::new(&p.config_dir).file_name().unwrap();
            (p.provider, name.to_str().unwrap().to_string())
        })
        .collect()
}

#[test]
fn finds_claude_dirs_with_markers_and_skips_lookalikes_and_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    claude_dir(&home.join(".claude"));
    claude_dir(&home.join(".claude-work"));
    // Settings-only layout (default dir without an inner .claude.json).
    fs::create_dir_all(home.join(".claude-max/projects")).unwrap();
    fs::write(home.join(".claude-max/settings.json"), "{}").unwrap();
    // Look-alikes: tool state dir, markers of the wrong kind, a plain file.
    fs::create_dir_all(home.join(".claude-verify/records")).unwrap();
    fs::create_dir_all(home.join(".claude-half")).unwrap();
    fs::write(home.join(".claude-half/.claude.json"), "{}").unwrap();
    fs::create_dir_all(home.join(".claude-swapped/.claude.json")).unwrap();
    fs::write(home.join(".claude-swapped/projects"), "").unwrap();
    fs::write(home.join(".claude.json"), "{}").unwrap();
    fs::create_dir_all(home.join(".claude-fileproj")).unwrap();
    fs::write(home.join(".claude-fileproj/projects"), "").unwrap();
    fs::write(home.join(".claude-fileproj/.claude.json"), "{}").unwrap();
    // A link to a real config dir must not become a second account.
    std::os::unix::fs::symlink(home.join(".claude-work"), home.join(".claude-link")).unwrap();
    // Real profile-switcher layout: own .claude.json, shared projects and
    // settings linked to the default dir. That is an account of its own.
    let shared = home.join(".claude-shared");
    fs::create_dir_all(&shared).unwrap();
    fs::write(shared.join(".claude.json"), "{}").unwrap();
    std::os::unix::fs::symlink(home.join(".claude/projects"), shared.join("projects")).unwrap();
    std::os::unix::fs::symlink(
        home.join(".claude/.claude.json"),
        shared.join("settings.json"),
    )
    .unwrap();
    // A dir made only of links is somebody else's account, not a new one.
    let linked = home.join(".claude-all-links");
    fs::create_dir_all(&linked).unwrap();
    for name in ["projects", ".claude.json", "settings.json"] {
        std::os::unix::fs::symlink(home.join(".claude-work").join(name), linked.join(name))
            .unwrap();
    }
    fs::write(home.join(".claude-work/settings.json"), "{}").unwrap();

    let none = |_: &str| false;
    let found = discover(&AccountsConfig::default(), &env(home, &none));

    assert_eq!(
        dirs(&found),
        vec![
            (ProviderKind::ClaudeCode, ".claude".to_string()),
            (ProviderKind::ClaudeCode, ".claude-max".to_string()),
            (ProviderKind::ClaudeCode, ".claude-shared".to_string()),
            (ProviderKind::ClaudeCode, ".claude-work".to_string()),
        ]
    );
}

#[test]
fn ids_are_stable_file_safe_and_unique_and_overlapping_globs_dedupe() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    claude_dir(&home.join(".claude"));
    claude_dir(&home.join(".claude-work"));
    let cfg = AccountsConfig {
        discovery_globs: vec![
            "~/.claude-*".to_string(),
            "~/.claude".to_string(),
            "~/.claude*".to_string(),
            format!("{}/.claude-work", home.display()),
        ],
        ..AccountsConfig::default()
    };
    let none = |_: &str| false;

    let first = discover(&cfg, &env(home, &none));
    let second = discover(&cfg, &env(home, &none));

    assert_eq!(first, second);
    assert_eq!(first.len(), 2);
    assert_ne!(first[0].id, first[1].id);
    for profile in &first {
        assert!(profile.id.starts_with("claude_code-"), "{}", profile.id);
        assert_eq!(profile.id.len(), "claude_code-".len() + 12);
        assert!(
            profile
                .id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        );
        assert!(Path::new(&profile.config_dir).is_absolute());
    }
}

#[test]
fn unsupported_glob_shapes_match_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    claude_dir(&home.join(".claude"));
    claude_dir(&home.join("nested/.claude"));
    let cfg = AccountsConfig {
        discovery_globs: vec![
            "~/*/.claude".to_string(),
            "~/.cl*ude".to_string(),
            ".claude".to_string(),
            "~/**".to_string(),
            "~/missing/*".to_string(),
        ],
        ..AccountsConfig::default()
    };
    let none = |_: &str| false;
    assert!(discover(&cfg, &env(home, &none)).is_empty());
}

#[test]
fn other_providers_need_both_directory_and_binary() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::create_dir_all(home.join(".gemini")).unwrap();
    // OpenCode dir is missing; a symlinked dir must not count.
    fs::create_dir_all(home.join(".config")).unwrap();
    std::os::unix::fs::symlink(home.join(".codex"), home.join(".config/opencode")).unwrap();
    let cfg = AccountsConfig::default();

    let none = |_: &str| false;
    assert!(discover(&cfg, &env(home, &none)).is_empty());

    let only_gemini = |name: &str| name == "gemini";
    let found = discover(&cfg, &env(home, &only_gemini));
    assert_eq!(
        dirs(&found),
        vec![(ProviderKind::Antigravity, ".gemini".to_string())]
    );
    assert!(found[0].id.starts_with("antigravity-"));

    let all = |_: &str| true;
    let found = discover(&cfg, &env(home, &all));
    assert_eq!(
        dirs(&found),
        vec![
            (ProviderKind::Antigravity, ".gemini".to_string()),
            (ProviderKind::Codex, ".codex".to_string()),
        ]
    );

    fs::remove_file(home.join(".config/opencode")).unwrap();
    fs::create_dir_all(home.join(".config/opencode")).unwrap();
    let found = discover(&cfg, &env(home, &all));
    assert_eq!(found.len(), 3);
    assert_eq!(found[2].provider, ProviderKind::OpenCode);
}

#[test]
fn binary_lookup_needs_an_executable_file_in_a_listed_dir() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let other = tmp.path().join("other");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&other).unwrap();
    fs::write(bin.join("codex"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(bin.join("codex"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(bin.join("plain"), "").unwrap();
    fs::set_permissions(bin.join("plain"), fs::Permissions::from_mode(0o644)).unwrap();
    fs::create_dir_all(bin.join("adir")).unwrap();
    let path_var = std::env::join_paths([&other, &bin]).unwrap();

    assert!(binary_on_path_in(&path_var, "codex"));
    assert!(!binary_on_path_in(&path_var, "plain"));
    assert!(!binary_on_path_in(&path_var, "adir"));
    assert!(!binary_on_path_in(&path_var, "missing"));
    assert!(!binary_on_path_in(&path_var, "../bin/codex"));
    assert!(!binary_on_path_in(other.as_os_str(), "codex"));
}

#[test]
fn names_default_to_the_dir_name_and_aliases_override_without_changing_ids() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    claude_dir(&home.join(".claude"));
    claude_dir(&home.join(".claude-work"));
    claude_dir(&home.join(".claude-max"));
    fs::create_dir_all(home.join(".codex")).unwrap();
    let all = |_: &str| true;

    let plain = discover(&AccountsConfig::default(), &env(home, &all));
    let names = |found: &[aaf_domain::account::AccountProfile]| -> Vec<String> {
        found.iter().map(|p| p.name.clone()).collect()
    };
    assert_eq!(names(&plain), ["claude", "max", "work", "codex"]);

    let mut cfg = AccountsConfig::default();
    // `~`, an absolute path with a trailing slash, another provider, and a
    // key that matches nothing.
    cfg.aliases
        .insert("~/.claude-work".to_string(), "Work".to_string());
    cfg.aliases.insert(
        format!("{}/.claude-max/", home.display()),
        "Personal Max".to_string(),
    );
    cfg.aliases
        .insert("~/.codex".to_string(), "Codex main".to_string());
    cfg.aliases
        .insert("~/.claude-gone".to_string(), "Gone".to_string());
    // An empty alias is not a name.
    cfg.aliases.insert("~/.claude".to_string(), " ".to_string());
    let aliased = discover(&cfg, &env(home, &all));
    assert_eq!(
        names(&aliased),
        ["claude", "Personal Max", "Work", "Codex main"]
    );

    let ids = |found: &[aaf_domain::account::AccountProfile]| -> Vec<String> {
        found.iter().map(|p| p.id.clone()).collect()
    };
    assert_eq!(ids(&plain), ids(&aliased));
    let dirs_of = |found: &[aaf_domain::account::AccountProfile]| -> Vec<String> {
        found.iter().map(|p| p.config_dir.clone()).collect()
    };
    assert_eq!(dirs_of(&plain), dirs_of(&aliased));
}
