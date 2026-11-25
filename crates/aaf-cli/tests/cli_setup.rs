#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `aaf setup`: one command registers the hook, then reports the doctor checks.

#[path = "factory_support.rs"]
mod support;

use std::fs;

use support::Fixture;

#[test]
fn setup_registers_the_hook_runs_doctor_and_is_safe_to_rerun() {
    let fx = Fixture::new();
    let first = fx.aaf(&["setup", "--hook-host", "claude", "--no-skills"]);
    assert!(
        matches!(first.code, 0 | 2),
        "setup must not error: {}",
        first.stderr
    );
    assert_eq!(first.json["command"], "setup");
    assert_eq!(first.json["hook_hosts"][0], "claude");
    let settings = fs::read_to_string(fx.repo.join(".claude/settings.json")).unwrap();
    assert!(settings.contains("aaf-hook"), "{settings}");
    assert!(fx.repo.join(".autofactory/config.toml").is_file());
    let checks = first.json["checks"].as_array().unwrap();
    let hook = checks
        .iter()
        .find(|c| c["name"] == "hook_registered")
        .expect("doctor reports the hook");
    assert_eq!(hook["ok"], true, "{hook}");
    let failed = first.json["failed_required"].as_array().unwrap();
    assert_eq!(first.code == 0, failed.is_empty());

    // Edited config survives, and the second run changes nothing.
    let config = fx.repo.join(".autofactory/config.toml");
    let edited = format!(
        "{}\n# kept by setup\n",
        fs::read_to_string(&config).unwrap()
    );
    fs::write(&config, &edited).unwrap();
    let second = fx.aaf(&["setup", "--hook-host", "claude", "--no-skills"]);
    assert_eq!(second.code, first.code, "{}", second.stderr);
    assert_eq!(fs::read_to_string(&config).unwrap(), edited);
    assert_eq!(
        fs::read_to_string(fx.repo.join(".claude/settings.json")).unwrap(),
        settings
    );
}

#[test]
fn setup_outside_a_repository_is_an_error() {
    let fx = Fixture::new();
    let elsewhere = fx.root().join("not-a-repo");
    fs::create_dir_all(&elsewhere).unwrap();
    let out = fx.aaf_in(&elsewhere, &["setup", "--no-skills"], &[]);
    assert_eq!(out.code, 1, "{}", out.stderr);
}

#[test]
fn setup_is_refused_inside_an_agent_run() {
    let fx = Fixture::new();
    let out = fx.aaf_in(&fx.repo, &["setup"], &[("AAF_RUN_ID", "r-1")]);
    assert_eq!(out.code, 2, "{}", out.stderr);
    assert_eq!(out.json["status"], "refused");
    assert!(!fx.repo.join(".claude/settings.json").exists());
}
