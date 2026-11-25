#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Step 5 & Round-1 (Findings #12, #13a/b) tests: cross-OS hydration wiring with
//! shared `MockBackend`, real env-var subprocess check (`AAF_SEED_CACHE=0` & unset
//! `AAF_DATA_DIR`), broken mid-clone + `setup_cmd`, `setup_cmd` pre-creating dst,
//! idempotency, and `clone_dirs` write_denied/fail-closed invariants.

#[path = "git_support.rs"]
mod git_support;
#[path = "seed_support.rs"]
mod seed_support;

use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::process::Command;

use aaf_domain::engine_contracts::REPO_CONFIG_REL;
use aaf_domain::worktree_config::WorktreeConfig;
use aaf_engine::git::hydration::seed::{
    SEED_CACHE_ENV, SEED_TIMEOUT, seed_cache_enabled_from, seed_worktree_with,
};
use aaf_engine::git::hydration::{
    hydrate_for_agent, hydrate_fresh_worktree, hydrate_fresh_worktree_with_seed_options,
    worktree_write_denied_for_cwd,
};
use aaf_engine::git::worktree;
use git_support::{Fixture, commit_file};
use seed_support::{MockBackend, write_nested};

fn commit_nested(repo: &Path, rel: &str, content: &str, msg: &str) -> String {
    let full = repo.join(rel);
    if let Some(p) = full.parent() {
        fs::create_dir_all(p).unwrap();
    }
    commit_file(repo, rel, content, msg)
}

fn setup_pods_repo() -> (Fixture, String) {
    let fx = Fixture::new();
    commit_file(
        &fx.repo,
        ".gitignore",
        "Pods/\nsetup.stamp\ncache_dir/\n",
        "ignore",
    );
    commit_file(&fx.repo, "Podfile", "platform :ios, '15.0'\n", "podfile");
    let sha = commit_file(
        &fx.repo,
        "Podfile.lock",
        "PODS:\n  - Demo (1.0)\n",
        "podlock",
    );
    write_nested(&fx.repo, "Pods/Manifest.lock", "PODS:\n  - Demo (1.0)\n");
    write_nested(&fx.repo, "Pods/Demo/Demo.swift", "public let v = 1\n");
    (fx, sha)
}

#[test]
fn seed_hydrate_empty_worktree_config_seeds_both_branches_and_idempotent_cross_os() {
    let (fx, sha) = setup_pods_repo();
    let mock = MockBackend::default();

    // 1. Public `hydrate_for_agent` has no base_sha -> must NEVER seed.
    let wt_pub = worktree::create(&fx.repo, &fx.worktrees, "t-pub", "main").unwrap();
    let h_pub = hydrate_for_agent(&fx.repo, &wt_pub.path, &WorktreeConfig::default()).unwrap();
    assert!(h_pub.write_denied.is_empty());
    assert!(!wt_pub.path.join("Pods").exists());

    // 2. Branch WITH data_dir/snaps (`Some((data_dir, common_id))`), empty `[worktree]`.
    let data_dir = fx.repo.parent().unwrap().join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let wt_snaps = worktree::create(&fx.repo, &fx.worktrees, "t-snaps", "main").unwrap();
    let h_snaps = hydrate_fresh_worktree_with_seed_options(
        &fx.repo,
        &wt_snaps,
        &sha,
        Some((&data_dir, "repo-1")),
        true,
        &mock,
    )
    .unwrap();
    assert!(h_snaps.write_denied.is_empty());
    assert!(
        wt_snaps.path.join("Pods/Demo/Demo.swift").is_file(),
        "snaps branch must seed unconditionally on both macOS and Linux"
    );
    // Writable without mutating main.
    fs::write(wt_snaps.path.join("Pods/Demo/Demo.swift"), "mutated\n").unwrap();
    assert_eq!(
        fs::read_to_string(fx.repo.join("Pods/Demo/Demo.swift")).unwrap(),
        "public let v = 1\n"
    );
    // Idempotent on re-run.
    hydrate_fresh_worktree_with_seed_options(
        &fx.repo,
        &wt_snaps,
        &sha,
        Some((&data_dir, "repo-1")),
        true,
        &mock,
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(wt_snaps.path.join("Pods/Demo/Demo.swift")).unwrap(),
        "mutated\n"
    );

    // 3. Branch WITHOUT AAF_DATA_DIR (`None`), passing symbolic ref `"main"`.
    let wt_no_data = worktree::create(&fx.repo, &fx.worktrees, "t-nodata", "main").unwrap();
    let h_no_data =
        hydrate_fresh_worktree_with_seed_options(&fx.repo, &wt_no_data, "main", None, true, &mock)
            .unwrap();
    assert!(h_no_data.write_denied.is_empty());
    assert!(
        wt_no_data.path.join("Pods/Demo/Demo.swift").is_file(),
        "no-data-dir branch must seed unconditionally on both macOS and Linux"
    );
}

#[test]
fn seed_hydrate_mid_clone_failure_with_setup_cmd_cleans_dst_and_returns_ok() {
    let (fx, _) = setup_pods_repo();
    write_nested(&fx.repo, "cache_dir/item.txt", "explicit-clone");
    let sha = commit_nested(
        &fx.repo,
        REPO_CONFIG_REL,
        "[worktree]\nclone_dirs = [\"cache_dir\"]\nsetup_cmd = \"printf 'setup-ok' > setup.stamp\"\n",
        "worktree config with clone_dirs and setup_cmd",
    );

    let data_dir = fx.repo.parent().unwrap().join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let wt = worktree::create(&fx.repo, &fx.worktrees, "t-broken-seed", "main").unwrap();

    // Finding #13a: MockBackend writes partial dst THEN fails clone.
    let failing_backend = MockBackend {
        fail_after_partial_write: true,
        ..MockBackend::default()
    };
    let hydrated = hydrate_fresh_worktree_with_seed_options(
        &fx.repo,
        &wt,
        &sha,
        Some((&data_dir, "repo-2")),
        true,
        &failing_backend,
    )
    .unwrap();
    assert!(
        wt.path.is_dir(),
        "worktree must remain intact when seed fails"
    );
    assert!(
        fs::symlink_metadata(wt.path.join("Pods")).is_err(),
        "partial dst written before clone error must be removed"
    );
    assert_eq!(
        fs::read_to_string(wt.path.join("setup.stamp")).unwrap(),
        "setup-ok"
    );
    assert_eq!(hydrated.write_denied, vec![wt.path.join("cache_dir")]);

    let canon_wt = fs::canonicalize(&wt.path).unwrap();
    let denied = worktree_write_denied_for_cwd(&wt.path).unwrap();
    assert_eq!(
        denied,
        vec![canon_wt.join("cache_dir")],
        "clone_dirs remains write_denied; seed never alters write_denied"
    );
}

#[test]
fn seed_hydrate_setup_cmd_creating_dst_preserves_it_as_dst_already_exists() {
    let (fx, _) = setup_pods_repo();
    let sha = commit_nested(
        &fx.repo,
        REPO_CONFIG_REL,
        "[worktree]\nsetup_cmd = \"mkdir -p Pods && printf 'from-setup' > Pods/x\"\n",
        "setup creates Pods",
    );
    let wt = worktree::create(&fx.repo, &fx.worktrees, "t-setup-dst", "main").unwrap();
    hydrate_fresh_worktree_with_seed_options(
        &fx.repo,
        &wt,
        &sha,
        None,
        true,
        &MockBackend::default(),
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(wt.path.join("Pods/x")).unwrap(),
        "from-setup",
        "setup_cmd output in Pods/x must be preserved"
    );
    assert!(
        !wt.path.join("Pods/Demo/Demo.swift").exists(),
        "seed must skip when setup_cmd already created dst"
    );
    // Finding #9e: directly verify the skip reason is `dst_already_exists`.
    let rep = seed_worktree_with(
        &fx.repo,
        &wt.path,
        &WorktreeConfig::default(),
        &sha,
        SEED_TIMEOUT,
        &MockBackend::default(),
    );
    assert!(!rep.entries[0].seeded);
    assert_eq!(
        rep.entries[0].skip_reason.as_deref(),
        Some("dst_already_exists")
    );
}

#[test]
fn seed_opt_out_env_disables_seeding_including_real_process_env_and_clone_dirs_fail_closed() {
    assert_eq!(SEED_CACHE_ENV, "AAF_SEED_CACHE");
    assert!(seed_cache_enabled_from(None));
    assert!(seed_cache_enabled_from(Some(OsStr::new(""))));
    assert!(seed_cache_enabled_from(Some(OsStr::new("1"))));
    assert!(!seed_cache_enabled_from(Some(OsStr::new("0"))));
    assert!(!seed_cache_enabled_from(Some(OsStr::new(" 0 "))));

    // When invoked as child subprocess with controlled env, run real `hydrate_fresh_worktree`.
    if std::env::var_os("AAF_TEST_SEED_ENV_CHILD").is_some() {
        let (fx, sha) = setup_pods_repo();
        let wt = worktree::create(&fx.repo, &fx.worktrees, "t-env-child", "main").unwrap();
        hydrate_fresh_worktree(&fx.repo, &wt, &sha).unwrap();
        let expect_seeded =
            std::env::var_os("AAF_TEST_EXPECT_SEEDED").as_deref() == Some(OsStr::new("1"));
        if expect_seeded {
            assert!(wt.path.join("Pods/Demo/Demo.swift").is_file());
        } else {
            assert!(!wt.path.join("Pods").exists());
        }
        return;
    }

    // 1. Explicit seed_enabled=false via MockBackend works unconditionally on both OSes.
    let (fx, sha) = setup_pods_repo();
    let wt = worktree::create(&fx.repo, &fx.worktrees, "t-optout", "main").unwrap();
    hydrate_fresh_worktree_with_seed_options(
        &fx.repo,
        &wt,
        &sha,
        None,
        seed_cache_enabled_from(Some(OsStr::new("0"))),
        &MockBackend::default(),
    )
    .unwrap();
    assert!(!wt.path.join("Pods").exists(), "opt-out must not seed Pods");

    // 2. Real subprocess test: re-exec this test binary with `AAF_DATA_DIR` removed and
    // `AAF_SEED_CACHE=0` (and on macOS APFS, also `AAF_SEED_CACHE=1`).
    let exe = std::env::current_exe().unwrap();
    let out_off = Command::new(&exe)
        .args([
            "--exact",
            "seed_opt_out_env_disables_seeding_including_real_process_env_and_clone_dirs_fail_closed",
        ])
        .env_remove("AAF_DATA_DIR")
        .env("AAF_SEED_CACHE", "0")
        .env("AAF_TEST_SEED_ENV_CHILD", "1")
        .env("AAF_TEST_EXPECT_SEEDED", "0")
        .output()
        .unwrap();
    let out_off_stdout = String::from_utf8_lossy(&out_off.stdout);
    assert!(
        out_off.status.success() && out_off_stdout.contains("1 passed"),
        "child with AAF_SEED_CACHE=0 must execute and pass 1 test:\nstdout: {out_off_stdout}\nstderr: {}",
        String::from_utf8_lossy(&out_off.stderr)
    );

    #[cfg(target_os = "macos")]
    {
        let out_on = Command::new(&exe)
            .args([
                "--exact",
                "seed_opt_out_env_disables_seeding_including_real_process_env_and_clone_dirs_fail_closed",
            ])
            .env_remove("AAF_DATA_DIR")
            .env_remove("AAF_SEED_CACHE")
            .env("AAF_TEST_SEED_ENV_CHILD", "1")
            .env("AAF_TEST_EXPECT_SEEDED", "1")
            .output()
            .unwrap();
        let out_on_stdout = String::from_utf8_lossy(&out_on.stdout);
        assert!(
            out_on.status.success() && out_on_stdout.contains("1 passed"),
            "child with default AAF_SEED_CACHE on macOS APFS must pass 1 test:\nstdout: {out_on_stdout}\nstderr: {}",
            String::from_utf8_lossy(&out_on.stderr)
        );
    }

    // 3. Explicit clone_dirs remains fail-closed when its source is missing.
    let fx_fail = Fixture::new();
    commit_file(&fx_fail.repo, ".gitignore", "missing_dir/\n", "ign");
    let bad_sha = commit_nested(
        &fx_fail.repo,
        REPO_CONFIG_REL,
        "[worktree]\nclone_dirs = [\"missing_dir\"]\n",
        "bad clone_dirs",
    );
    let wt_fail = worktree::create(&fx_fail.repo, &fx_fail.worktrees, "t-fail", "main").unwrap();
    assert!(
        hydrate_fresh_worktree(&fx_fail.repo, &wt_fail, &bad_sha).is_err(),
        "clone_dirs must remain fail-closed"
    );
}
