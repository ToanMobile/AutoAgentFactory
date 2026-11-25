#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Round-1 & Round-2 security / fail-closed regression tests (Findings #3–#9, #13, #14).

#[path = "git_support.rs"]
mod git_support;
#[path = "seed_support.rs"]
mod seed_support;

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use aaf_domain::worktree_config::WorktreeConfig;
use aaf_engine::git::hydration::hydrate_fresh_worktree_with_seed_options;
use aaf_engine::git::hydration::seed::{SEED_TIMEOUT, run_git_raw_bytes_timed, seed_worktree_with};
use aaf_engine::git::worktree;
use git_support::{Fixture, commit_file, git};
use seed_support::{MockBackend, write_nested};

fn commit_nested(repo: &Path, rel: &str, content: &str, msg: &str) -> String {
    let full = repo.join(rel);
    if let Some(p) = full.parent() {
        fs::create_dir_all(p).unwrap();
    }
    commit_file(repo, rel, content, msg)
}

#[test]
fn seed_post_clone_rejects_untracked_gitignore_and_info_exclude_hidden_files() {
    // Case 1 (Finding #6): tracked `.gitignore` has `Library/*` + `!Library/Keep.txt`,
    // and cloned `Library/.gitignore` re-ignores `Keep.txt` -> S2.7b hidden_files catches it!
    let fx = Fixture::new();
    commit_file(
        &fx.repo,
        ".gitignore",
        "Library/*\n!Library/Keep.txt\n",
        "negation rule",
    );
    commit_nested(
        &fx.repo,
        "ProjectSettings/ProjectVersion.txt",
        "m_EditorVersion: 2022.3\n",
        "unity",
    );
    let sha = commit_nested(&fx.repo, "Packages/manifest.json", "{}\n", "manifest");
    write_nested(&fx.repo, "Library/Artifacts/a.bin", "ok");
    write_nested(&fx.repo, "Library/Keep.txt", "hidden-payload");
    write_nested(&fx.repo, "Library/.gitignore", "Keep.txt\n");

    let wt = worktree::create(&fx.repo, &fx.worktrees, "t-hid-ign", "main").unwrap();
    let rep = seed_worktree_with(
        &fx.repo,
        &wt.path,
        &WorktreeConfig::default(),
        &sha,
        SEED_TIMEOUT,
        &MockBackend::default(),
    );
    assert!(!rep.entries[0].seeded, "{rep:?}");
    let reason = rep.entries[0].skip_reason.as_deref().unwrap_or("");
    assert!(
        reason.starts_with("hidden_files_after_seed"),
        "expected S2.7b hidden_files_after_seed, got: {reason}"
    );
    assert!(!wt.path.join("Library").exists());

    // Case 2 (Finding #9a): `.gitignore` has `Library/*` + `!Library/Keep/`, so NO `.gitignore`
    // rule decides `Library/Keep/f`; `.git/info/exclude` hides `Library/Keep/f`.
    // `git ls-files --others --exclude-standard -- Library` returns empty (S2.7a passes!),
    // so ONLY S2.7b (`aaf_fp::hidden_files`) catches it!
    let fx2 = Fixture::new();
    commit_file(
        &fx2.repo,
        ".gitignore",
        "Library/*\n!Library/Keep/\n",
        "negate dir only",
    );
    commit_nested(
        &fx2.repo,
        "ProjectSettings/ProjectVersion.txt",
        "m_EditorVersion: 2022.3\n",
        "unity",
    );
    let sha2 = commit_nested(&fx2.repo, "Packages/manifest.json", "{}\n", "manifest");
    write_nested(&fx2.repo, "Library/Artifacts/a.bin", "ok");
    write_nested(&fx2.repo, "Library/Keep/f", "info-exclude-hidden");
    write_nested(&fx2.repo, ".git/info/exclude", "Library/Keep/f\n");

    let wt2 = worktree::create(&fx2.repo, &fx2.worktrees, "t-hid-exc", "main").unwrap();
    let rep2 = seed_worktree_with(
        &fx2.repo,
        &wt2.path,
        &WorktreeConfig::default(),
        &sha2,
        SEED_TIMEOUT,
        &MockBackend::default(),
    );
    assert!(!rep2.entries[0].seeded, "{rep2:?}");
    let reason2 = rep2.entries[0].skip_reason.as_deref().unwrap_or("");
    assert!(
        reason2.starts_with("hidden_files_after_seed"),
        "expected S2.7b hidden_files_after_seed for info/exclude, got: {reason2}"
    );
    assert!(!wt2.path.join("Library").exists());
}

#[cfg(unix)]
#[test]
fn seed_0555_source_is_writable_in_worktree_and_git_worktree_remove_force_succeeds() {
    use std::os::unix::fs::PermissionsExt;

    let fx = Fixture::new();
    commit_file(&fx.repo, ".gitignore", "Pods/\n", "ignore Pods");
    commit_file(&fx.repo, "Podfile", "platform :ios, '15.0'\n", "podfile");
    let sha = commit_file(&fx.repo, "Podfile.lock", "PODS: []\n", "podlock");
    write_nested(&fx.repo, "Pods/Manifest.lock", "PODS: []\n");
    write_nested(&fx.repo, "Pods/ReadOnlyPod/src.swift", "let a = 1\n");

    let ro_dir = fx.repo.join("Pods/ReadOnlyPod");
    fs::set_permissions(&ro_dir, fs::Permissions::from_mode(0o555)).unwrap();

    let wt = worktree::create(&fx.repo, &fx.worktrees, "t-ro-remove", "main").unwrap();
    let rep = seed_worktree_with(
        &fx.repo,
        &wt.path,
        &WorktreeConfig::default(),
        &sha,
        SEED_TIMEOUT,
        &MockBackend::default(),
    );
    assert!(rep.entries[0].seeded, "{rep:?}");

    let main_mode = fs::metadata(&ro_dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(main_mode, 0o555, "main source mode must remain untouched");

    worktree::remove(&fx.repo, &wt, true, true).unwrap();
    assert!(!wt.path.exists());
    assert!(git(&fx.repo, &["branch", "--list", &wt.branch]).is_empty());

    let _ = fs::set_permissions(&ro_dir, fs::Permissions::from_mode(0o755));
}

#[cfg(unix)]
#[test]
fn seed_dangling_symlink_with_parent_dir_through_symlink_is_rejected() {
    use std::os::unix::fs::symlink;

    let fx = Fixture::new();
    commit_file(&fx.repo, ".gitignore", "Pods/\n", "ignore Pods");
    commit_file(&fx.repo, "Podfile", "platform :ios, '15.0'\n", "podfile");
    let sha = commit_file(&fx.repo, "Podfile.lock", "PODS: []\n", "podlock");
    write_nested(&fx.repo, "Pods/Manifest.lock", "PODS: []\n");

    symlink("..", fx.repo.join("Pods/y")).unwrap();
    symlink("y/../out", fx.repo.join("Pods/x")).unwrap();

    let wt = worktree::create(&fx.repo, &fx.worktrees, "t-dang-esc", "main").unwrap();
    let rep = seed_worktree_with(
        &fx.repo,
        &wt.path,
        &WorktreeConfig::default(),
        &sha,
        SEED_TIMEOUT,
        &MockBackend::default(),
    );
    assert!(!rep.entries[0].seeded, "{rep:?}");
    assert!(
        rep.entries[0]
            .skip_reason
            .as_deref()
            .unwrap_or("")
            .starts_with("escaping_dangling_symlink"),
        "{rep:?}"
    );
    assert!(!wt.path.join("Pods").exists());
}

#[cfg(unix)]
#[test]
fn seed_any_post_clone_cleanup_failed_escalates_hydrate_to_err() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let fx = Fixture::new();
    commit_file(&fx.repo, ".gitignore", "Pods/\n", "ignore Pods");
    commit_file(&fx.repo, "Podfile", "platform :ios, '15.0'\n", "podfile");
    let sha = commit_file(&fx.repo, "Podfile.lock", "PODS: []\n", "podlock");
    write_nested(&fx.repo, "Pods/Manifest.lock", "PODS: []\n");

    // 1. Finding #4: non-security post-clone failure (Manifest.lock drift after clone)
    // where cleanup fails MUST also escalate hydrate to Err (fail-closed).
    let wt_drift = worktree::create(&fx.repo, &fx.worktrees, "t-fatal-drift", "main").unwrap();
    let res_drift = hydrate_fresh_worktree_with_seed_options(
        &fx.repo,
        &wt_drift,
        &sha,
        None,
        true,
        &MockBackend {
            corrupt_manifest_after_clone: true,
            lock_parent_after_clone: true,
            ..MockBackend::default()
        },
    );
    let _ = fs::set_permissions(&wt_drift.path, fs::Permissions::from_mode(0o755));
    let _ = worktree::remove(&fx.repo, &wt_drift, true, true);
    assert!(
        res_drift.is_err(),
        "ANY post-clone cleanup failure (incl. lock drift) must escalate hydrate to Err"
    );

    // 2. Security violation (S2.6 absolute symlink) where cleanup fails also escalates to Err.
    symlink("/etc/hosts", fx.repo.join("Pods/bad_abs")).unwrap();
    let wt_sec = worktree::create(&fx.repo, &fx.worktrees, "t-fatal-sec", "main").unwrap();
    let res_sec = hydrate_fresh_worktree_with_seed_options(
        &fx.repo,
        &wt_sec,
        &sha,
        None,
        true,
        &MockBackend {
            lock_parent_after_clone: true,
            ..MockBackend::default()
        },
    );
    let _ = fs::set_permissions(&wt_sec.path, fs::Permissions::from_mode(0o755));
    let _ = worktree::remove(&fx.repo, &wt_sec, true, true);
    assert!(res_sec.is_err());
}

#[test]
fn seed_flow_timeout_ps_err_empty_unity_cross_dev_nested_mount_and_real_git_deadline() {
    let fx = Fixture::new();
    commit_file(&fx.repo, ".gitignore", "Library/\n", "ign");
    commit_nested(
        &fx.repo,
        "ProjectSettings/ProjectVersion.txt",
        "m_EditorVersion: 2022.3\n",
        "unity",
    );
    let sha = commit_nested(&fx.repo, "Packages/manifest.json", "{}\n", "manifest");

    // Empty after exclude -> skips without creating dst.
    write_nested(&fx.repo, "Library/ScriptAssemblies/a.dll", "x");
    let wt_empty = worktree::create(&fx.repo, &fx.worktrees, "t-empty-u", "main").unwrap();
    let rep_empty = seed_worktree_with(
        &fx.repo,
        &wt_empty.path,
        &WorktreeConfig::default(),
        &sha,
        SEED_TIMEOUT,
        &MockBackend::default(),
    );
    assert!(!rep_empty.entries[0].seeded);
    assert_eq!(
        rep_empty.entries[0].skip_reason.as_deref(),
        Some("unity_library_empty_after_exclude")
    );
    assert!(!wt_empty.path.join("Library").exists());

    write_nested(&fx.repo, "Library/Artifacts/a.bin", "data");

    // Finding #9d: nested_mount wiring through seed_worktree_with.
    let wt_nm = worktree::create(&fx.repo, &fx.worktrees, "t-nm", "main").unwrap();
    let rep_nm = seed_worktree_with(
        &fx.repo,
        &wt_nm.path,
        &WorktreeConfig::default(),
        &sha,
        SEED_TIMEOUT,
        &MockBackend {
            nested_mount_err: true,
            ..MockBackend::default()
        },
    );
    assert!(!rep_nm.entries[0].seeded);
    assert!(
        rep_nm.entries[0]
            .skip_reason
            .as_deref()
            .unwrap_or("")
            .starts_with("nested_mount"),
        "{rep_nm:?}"
    );
    assert!(!wt_nm.path.join("Library").exists());

    // ps error -> unity_ps_failed.
    let wt_ps = worktree::create(&fx.repo, &fx.worktrees, "t-ps-err", "main").unwrap();
    let rep_ps = seed_worktree_with(
        &fx.repo,
        &wt_ps.path,
        &WorktreeConfig::default(),
        &sha,
        SEED_TIMEOUT,
        &MockBackend {
            ps_output: Err("ps failed".to_string()),
            ..MockBackend::default()
        },
    );
    assert!(!rep_ps.entries[0].seeded);
    assert!(
        rep_ps.entries[0]
            .skip_reason
            .as_deref()
            .unwrap_or("")
            .starts_with("unity_ps_failed"),
        "{rep_ps:?}"
    );

    // Cross-device -> cross_device* skip, 1 device check.
    let wt_xdev = worktree::create(&fx.repo, &fx.worktrees, "t-xdev", "main").unwrap();
    let xdev_backend = MockBackend {
        same_dev: false,
        ..MockBackend::default()
    };
    let rep_xdev = seed_worktree_with(
        &fx.repo,
        &wt_xdev.path,
        &WorktreeConfig::default(),
        &sha,
        SEED_TIMEOUT,
        &xdev_backend,
    );
    assert!(!rep_xdev.entries[0].seeded);
    assert!(
        rep_xdev.entries[0]
            .skip_reason
            .as_deref()
            .unwrap_or("")
            .starts_with("cross_device"),
        "{rep_xdev:?}"
    );
    assert_eq!(xdev_backend.same_dev_call_count(), 1);

    // Finding #5: REAL git command strictly respects S2.8 deadline (< 1s vs 5s sleep).
    let t0 = Instant::now();
    let git_err = run_git_raw_bytes_timed(
        &fx.repo,
        &["-c", "alias.slow=!sleep 5", "slow"],
        Duration::from_millis(120),
    )
    .unwrap_err();
    assert_eq!(git_err, "timeout");
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "real git must respect tight deadline, took {:?}",
        t0.elapsed()
    );

    // Normal seed calls same_cow_device exactly once per target (Finding #3).
    let wt_once = worktree::create(&fx.repo, &fx.worktrees, "t-once", "main").unwrap();
    let once_backend = MockBackend::default();
    let rep_once = seed_worktree_with(
        &fx.repo,
        &wt_once.path,
        &WorktreeConfig::default(),
        &sha,
        SEED_TIMEOUT,
        &once_backend,
    );
    assert!(rep_once.entries[0].seeded, "{rep_once:?}");
    assert_eq!(once_backend.same_dev_call_count(), 1);
}

#[cfg(unix)]
#[test]
fn seed_s2_1_rejects_symlink_parent_component_ios_in_source_and_worktree() {
    use std::os::unix::fs::symlink;

    // 1. Source side: main `ios` is a symlink -> `source_invalid`.
    let fx = Fixture::new();
    commit_file(&fx.repo, ".gitignore", "ios/Pods/\n", "ign ios pods");
    commit_nested(&fx.repo, "ios/Podfile", "platform :ios, '15.0'\n", "pod");
    let sha = commit_nested(&fx.repo, "ios/Podfile.lock", "PODS: []\n", "lock");

    let outside = tempfile::tempdir().unwrap();
    write_nested(outside.path(), "Pods/Manifest.lock", "PODS: []\n");
    fs::remove_dir_all(fx.repo.join("ios")).unwrap();
    symlink(outside.path(), fx.repo.join("ios")).unwrap();

    let wt = worktree::create(&fx.repo, &fx.worktrees, "t-ios-sym", "main").unwrap();
    let rep = seed_worktree_with(
        &fx.repo,
        &wt.path,
        &WorktreeConfig::default(),
        &sha,
        SEED_TIMEOUT,
        &MockBackend::default(),
    );
    assert!(!rep.entries[0].seeded, "{rep:?}");
    assert!(
        rep.entries[0]
            .skip_reason
            .as_deref()
            .unwrap_or("")
            .starts_with("source_invalid"),
        "{rep:?}"
    );

    // 2. Finding #9c (worktree side): `ios` is a symlink in `wt_root` -> `wt_prefix_invalid`.
    let fx2 = Fixture::new();
    commit_file(&fx2.repo, ".gitignore", "ios/Pods/\n", "ign ios pods");
    commit_nested(&fx2.repo, "ios/Podfile", "platform :ios, '15.0'\n", "pod");
    let sha2 = commit_nested(&fx2.repo, "ios/Podfile.lock", "PODS: []\n", "lock");
    write_nested(&fx2.repo, "ios/Pods/Manifest.lock", "PODS: []\n");

    let wt2 = worktree::create(&fx2.repo, &fx2.worktrees, "t-wt-ios-sym", "main").unwrap();
    let wt_outside = tempfile::tempdir().unwrap();
    fs::remove_dir_all(wt2.path.join("ios")).unwrap();
    symlink(wt_outside.path(), wt2.path.join("ios")).unwrap();

    let rep2 = seed_worktree_with(
        &fx2.repo,
        &wt2.path,
        &WorktreeConfig::default(),
        &sha2,
        SEED_TIMEOUT,
        &MockBackend::default(),
    );
    assert!(!rep2.entries[0].seeded, "{rep2:?}");
    assert!(
        rep2.entries[0]
            .skip_reason
            .as_deref()
            .unwrap_or("")
            .starts_with("wt_prefix_invalid"),
        "{rep2:?}"
    );
    assert!(!wt_outside.path().join("Pods").exists());
}

#[cfg(unix)]
#[test]
fn seed_fatal_cleanup_restores_parent_chain_and_wt_root_so_worktree_remove_succeeds() {
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;

    use aaf_engine::git::hydration::seed::SeedBackend;
    use aaf_engine::git::hydration::stage::CloneSkip;

    struct LockedParentChainBackend;

    impl SeedBackend for LockedParentChainBackend {
        fn same_cow_device(&self, _src: &Path, _dst_parent: &Path) -> Result<bool, String> {
            Ok(true)
        }

        fn clone_dir_strict(
            &self,
            _src: &Path,
            dst: &Path,
            _timeout: Duration,
        ) -> Result<(), CloneSkip> {
            let nested = dst.join("sub");
            fs::create_dir_all(&nested).unwrap();
            fs::write(nested.join("locked.txt"), "partial").unwrap();
            fs::set_permissions(&nested, fs::Permissions::from_mode(0o555)).unwrap();
            fs::set_permissions(dst, fs::Permissions::from_mode(0o555)).unwrap();
            let intermediate = dst.parent().expect("ios parent dir");
            let wt_root = intermediate.parent().expect("wt_root");
            fs::set_permissions(intermediate, fs::Permissions::from_mode(0o555)).unwrap();
            fs::set_permissions(wt_root, fs::Permissions::from_mode(0o555)).unwrap();
            Err(CloneSkip::cleanup_failed(
                "permission denied",
                "injected_clone_error",
            ))
        }

        fn clone_entries_strict(
            &self,
            _src_dir: &Path,
            _entries: &[OsString],
            _dst_dir: &Path,
            _timeout: Duration,
        ) -> Result<(), CloneSkip> {
            unreachable!("ios/Pods uses clone_dir_strict")
        }

        fn ps_listing(&self, _timeout: Duration) -> Result<String, String> {
            Ok(String::new())
        }

        fn check_no_nested_mount(
            &self,
            _src: &Path,
            _sub_entries: Option<&[OsString]>,
            _deadline: Instant,
        ) -> Result<(), String> {
            Ok(())
        }
    }

    struct UnlockOnDrop(std::path::PathBuf);
    impl Drop for UnlockOnDrop {
        fn drop(&mut self) {
            if self.0.exists() {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
                let ios = self.0.join("ios");
                let _ = fs::set_permissions(&ios, fs::Permissions::from_mode(0o755));
                let pods = ios.join("Pods");
                let _ = fs::set_permissions(&pods, fs::Permissions::from_mode(0o755));
                let _ = fs::set_permissions(pods.join("sub"), fs::Permissions::from_mode(0o755));
            }
        }
    }

    let fx = Fixture::new();
    commit_file(&fx.repo, ".gitignore", "ios/Pods/\n", "ignore ios pods");
    commit_nested(&fx.repo, "ios/Podfile", "platform :ios, '15.0'\n", "pod");
    let sha = commit_nested(&fx.repo, "ios/Podfile.lock", "PODS: []\n", "lock");
    write_nested(&fx.repo, "ios/Pods/Manifest.lock", "PODS: []\n");

    let wt = worktree::create(&fx.repo, &fx.worktrees, "t-m4-parent-chain", "main").unwrap();
    let _unlock_guard = UnlockOnDrop(wt.path.clone());

    let res = hydrate_fresh_worktree_with_seed_options(
        &fx.repo,
        &wt,
        &sha,
        None,
        true,
        &LockedParentChainBackend,
    );
    assert!(
        res.is_err(),
        "hydrate must fail closed when seed cleanup fails: {res:?}"
    );
    assert!(
        fs::symlink_metadata(&wt.path).is_err(),
        "worktree directory must be completely removed even when wt_root and ios/ were 0555"
    );

    let porcelain = git(&fx.repo, &["worktree", "list", "--porcelain"]);
    let wt_str = wt.path.to_str().unwrap();
    assert!(
        !porcelain.contains(wt_str) && !porcelain.contains(&wt.branch),
        "git worktree list --porcelain must no longer list removed worktree:\n{porcelain}"
    );
}
