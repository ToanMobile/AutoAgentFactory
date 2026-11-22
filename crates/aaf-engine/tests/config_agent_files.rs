#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Two things an agent must not decide for itself: the files Claude Code
//! loads as instructions, hooks and servers (head-locked by default), and
//! which variables of the operator's environment it gets (`run.env_pass`,
//! which the operator can set in the machine config no task can edit).

use std::fs;
use std::path::Path;

use aaf_domain::config::{AGENT_CONFIG_GLOBS, GateConfig};
use aaf_engine::config::{ConfigInputs, ConfigOverlay, EffectiveConfig, REPO_CONFIG_REL, load};
use globset::{GlobBuilder, GlobSetBuilder};
use tempfile::TempDir;

fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn load_tiers(tier1: Option<&str>, tier2: &str) -> EffectiveConfig {
    let dir = TempDir::new().unwrap();
    let global = dir.path().join("machine/config.toml");
    if let Some(body) = tier1 {
        write(&global, body);
    }
    write(&dir.path().join("repo").join(REPO_CONFIG_REL), tier2);
    load(&ConfigInputs {
        global_config: Some(global),
        repo_root: dir.path().join("repo"),
        env: &|_| None,
        cli: ConfigOverlay::default(),
    })
    .unwrap()
}

/// Compiled the way the gate compiles them (`gate::head_lock`).
fn locked(globs: &[String], path: &str) -> bool {
    let mut set = GlobSetBuilder::new();
    for glob in globs {
        set.add(
            GlobBuilder::new(glob)
                .literal_separator(true)
                .build()
                .unwrap(),
        );
    }
    set.build().unwrap().is_match(path)
}

#[test]
fn the_default_head_lock_covers_what_claude_code_loads_from_the_repository() {
    let globs = GateConfig::default().head_lock_globs;
    for path in [
        ".claude/settings.json",
        ".claude/settings.local.json",
        ".claude/hooks/session-start.sh",
        ".claude/agents/reviewer.md",
        "packages/app/.claude/settings.json",
        "CLAUDE.md",
        "packages/app/CLAUDE.md",
        ".mcp.json",
    ] {
        assert!(locked(&globs, path), "{path} is not head-locked");
    }
    for path in ["src/claude.rs", "docs/CLAUDE.md.txt", "src/mcp.json"] {
        assert!(!locked(&globs, path), "{path} is head-locked");
    }
}

#[test]
fn a_repo_list_written_before_the_agent_globs_is_kept_as_it_is() {
    // Written by an earlier `aaf init`: the key is set, so the list is the
    // repository's own and the new defaults do not reach it (`aaf doctor`
    // says so: `head_lock_agent_files`).
    let cfg = load_tiers(None, "[gate]\nhead_lock_globs = [\"**/tests/**\"]\n");
    assert_eq!(cfg.repo.gate.head_lock_globs, ["**/tests/**"]);

    // A config that leaves the key alone gets them.
    let cfg = load_tiers(None, "[gate]\ncommand_timeout_secs = 60\n");
    for glob in AGENT_CONFIG_GLOBS {
        assert!(cfg.repo.gate.head_lock_globs.iter().any(|g| g == glob));
    }
}

#[test]
fn the_machine_config_can_name_variables_for_the_agent() {
    let cfg = load_tiers(Some("[run]\nenv_pass = [\"SENTRY_DSN\"]\n"), "");
    assert_eq!(cfg.global.run.env_pass, ["SENTRY_DSN"]);
    // Kept apart from the repository's list: who allowed what stays visible.
    assert!(cfg.repo.run.env_pass.is_empty());

    let both = load_tiers(
        Some("[run]\nenv_pass = [\"SENTRY_DSN\"]\n"),
        "[run]\nenv_pass = [\"MAPS_KEY\"]\n",
    );
    assert_eq!(both.global.run.env_pass, ["SENTRY_DSN"]);
    assert_eq!(both.repo.run.env_pass, ["MAPS_KEY"]);
}

#[test]
fn the_machine_run_section_takes_nothing_but_env_pass() {
    let dir = TempDir::new().unwrap();
    let global = dir.path().join("config.toml");
    write(&global, "[run]\nexecutor = \"custom\"\n");
    let loaded = load(&ConfigInputs {
        global_config: Some(global),
        repo_root: dir.path().to_path_buf(),
        env: &|_| None,
        cli: ConfigOverlay::default(),
    });
    assert!(loaded.is_err(), "the executor is the repository's to set");
}
