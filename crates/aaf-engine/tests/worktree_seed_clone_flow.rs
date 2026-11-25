#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Step 4 tests: filtered Unity clone, npm `.cache` removal, CocoaPods,
//! gitignore/negation/global-exclude rejections, symlink scanning (S2.6),
//! and overlap/0555 recovery (S3/S4).

#[path = "git_support.rs"]
mod git_support;
#[path = "seed_support.rs"]
mod seed_support;

use std::fs;
use std::path::Path;

use aaf_domain::worktree_config::WorktreeConfig;
use aaf_engine::git::hydration::seed::{
    SEED_TIMEOUT, UNITY_EXCLUDED_ENTRIES, seed_worktree, seed_worktree_with,
};
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
fn seed_unity_fixture_seeds_writable_library_without_excluded_entries() {
    let fx = Fixture::new();
    commit_file(&fx.repo, ".gitignore", "Library/\n", "ignore Library");
    commit_nested(
        &fx.repo,
        "ProjectSettings/ProjectVersion.txt",
        "m_EditorVersion: 2022.3.10f1\n",
        "unity ver",
    );
    let sha = commit_nested(
        &fx.repo,
        "Packages/manifest.json",
        "{\"dependencies\":{}}\n",
        "manifest",
    );

    write_nested(&fx.repo, "Library/Artifacts/a1.bin", "asset-cache");
    write_nested(&fx.repo, "Library/PackageCache/p.json", "{}");
    for excl in UNITY_EXCLUDED_ENTRIES {
        if excl.contains('.') {
            write_nested(&fx.repo, &format!("Library/{excl}"), "skip-me");
        } else {
            write_nested(&fx.repo, &format!("Library/{excl}/file.dat"), "skip-me");
        }
    }

    let wt = worktree::create(&fx.repo, &fx.worktrees, "t-unity", "main").unwrap();
    let report = seed_worktree_with(
        &fx.repo,
        &wt.path,
        &WorktreeConfig::default(),
        &sha,
        SEED_TIMEOUT,
        &MockBackend::default(),
    );
    assert_eq!(report.entries.len(), 1, "{report:?}");
    assert!(report.entries[0].seeded, "{report:?}");
    assert!(report.entries[0].files >= 2, "{report:?}");
    assert!(report.summary_line().unwrap().contains("aaf: seed"));

    let wt_art = wt.path.join("Library/Artifacts/a1.bin");
    assert_eq!(fs::read_to_string(&wt_art).unwrap(), "asset-cache");
    fs::write(&wt_art, "mutated-in-wt").unwrap();
    assert_eq!(
        fs::read_to_string(fx.repo.join("Library/Artifacts/a1.bin")).unwrap(),
        "asset-cache"
    );

    for excl in UNITY_EXCLUDED_ENTRIES {
        assert!(
            !wt.path.join("Library").join(excl).exists(),
            "excluded entry {excl} must not be seeded"
        );
    }

    // Real CoW backend E2E on macOS APFS (required) or Linux reflink probe.
    let wt_real = worktree::create(&fx.repo, &fx.worktrees, "t-unity-real", "main").unwrap();
    let real_rep = seed_worktree(&fx.repo, &wt_real.path, &WorktreeConfig::default(), &sha);
    #[cfg(target_os = "macos")]
    {
        assert!(
            real_rep.entries[0].seeded,
            "real APFS seed must succeed: {real_rep:?}"
        );
        assert!(wt_real.path.join("Library/Artifacts/a1.bin").is_file());
        assert!(!wt_real.path.join("Library/ScriptAssemblies").exists());
    }
    #[cfg(target_os = "linux")]
    {
        let probe_src = fx.repo.join(".reflink_probe_src");
        let probe_dst = fx.repo.join(".reflink_probe_dst");
        fs::write(&probe_src, "p").unwrap();
        let reflink_ok = std::process::Command::new("/bin/cp")
            .args(["--reflink=always", "--"])
            .arg(&probe_src)
            .arg(&probe_dst)
            .status()
            .is_ok_and(|s| s.success());
        let _ = fs::remove_file(&probe_src);
        let _ = fs::remove_file(&probe_dst);
        if reflink_ok {
            assert!(real_rep.entries[0].seeded, "{real_rep:?}");
        } else {
            assert!(!real_rep.entries[0].seeded, "{real_rep:?}");
            assert!(
                real_rep.entries[0]
                    .skip_reason
                    .as_deref()
                    .unwrap_or("")
                    .starts_with("reflink_unsupported"),
                "{real_rep:?}"
            );
            assert!(!wt_real.path.join("Library").exists());
        }
    }
}

#[test]
fn seed_npm_fixture_seeds_node_modules_and_removes_dot_cache() {
    let fx = Fixture::new();
    let lock = r#"{"lockfileVersion":3,"packages":{"":{"name":"demo"},"node_modules/pkg":{"version":"1.0.0","integrity":"sha512-x"}}}"#;
    commit_file(&fx.repo, ".gitignore", "node_modules/\n", "ignore nm");
    commit_file(&fx.repo, "package.json", "{\"name\":\"demo\"}\n", "pkg");
    let sha = commit_file(&fx.repo, "package-lock.json", lock, "lock");

    write_nested(
        &fx.repo,
        "node_modules/.package-lock.json",
        r#"{"lockfileVersion":3,"packages":{"node_modules/pkg":{"version":"1.0.0","integrity":"sha512-x"}}}"#,
    );
    write_nested(
        &fx.repo,
        "node_modules/pkg/index.js",
        "module.exports = 1;\n",
    );
    write_nested(&fx.repo, "node_modules/.cache/babel/c.json", "stale");

    let wt = worktree::create(&fx.repo, &fx.worktrees, "t-npm", "main").unwrap();
    let report = seed_worktree_with(
        &fx.repo,
        &wt.path,
        &WorktreeConfig::default(),
        &sha,
        SEED_TIMEOUT,
        &MockBackend::default(),
    );
    assert!(report.entries[0].seeded, "{report:?}");
    assert!(wt.path.join("node_modules/pkg/index.js").is_file());
    assert!(
        !wt.path.join("node_modules/.cache").exists(),
        "node_modules/.cache must be removed after clone"
    );
}

#[test]
fn seed_cocoapods_mismatch_skips_and_match_seeds() {
    let fx = Fixture::new();
    commit_file(&fx.repo, ".gitignore", "Pods/\n", "ignore Pods");
    commit_file(&fx.repo, "Podfile", "platform :ios, '15.0'\n", "podfile");
    let sha = commit_file(
        &fx.repo,
        "Podfile.lock",
        "PODS:\n  - Foo (1.0)\n",
        "podlock",
    );

    write_nested(&fx.repo, "Pods/Manifest.lock", "PODS:\n  - Foo (0.9)\n");
    write_nested(&fx.repo, "Pods/Foo/Foo.swift", "let x = 1\n");

    let wt1 = worktree::create(&fx.repo, &fx.worktrees, "t-pods-bad", "main").unwrap();
    let rep1 = seed_worktree_with(
        &fx.repo,
        &wt1.path,
        &WorktreeConfig::default(),
        &sha,
        SEED_TIMEOUT,
        &MockBackend::default(),
    );
    assert!(!rep1.entries[0].seeded, "{rep1:?}");
    assert_eq!(
        rep1.entries[0].skip_reason.as_deref(),
        Some("pods_manifest_mismatch")
    );
    assert!(!wt1.path.join("Pods").exists());

    write_nested(&fx.repo, "Pods/Manifest.lock", "PODS:\n  - Foo (1.0)\n");
    let wt2 = worktree::create(&fx.repo, &fx.worktrees, "t-pods-ok", "main").unwrap();
    let rep2 = seed_worktree_with(
        &fx.repo,
        &wt2.path,
        &WorktreeConfig::default(),
        &sha,
        SEED_TIMEOUT,
        &MockBackend::default(),
    );
    assert!(rep2.entries[0].seeded, "{rep2:?}");
    assert!(wt2.path.join("Pods/Foo/Foo.swift").is_file());
}

#[test]
fn seed_gitignore_not_tracked_global_exclude_or_negation_skips_cleanly() {
    let fx = Fixture::new();
    let global_exc = fx.repo.parent().unwrap().join("global_exclude");
    fs::write(&global_exc, "Library/\n").unwrap();
    git(
        &fx.repo,
        &["config", "core.excludesfile", global_exc.to_str().unwrap()],
    );
    commit_nested(
        &fx.repo,
        "ProjectSettings/ProjectVersion.txt",
        "m_EditorVersion: 2022.3\n",
        "unity",
    );
    let sha_a = commit_nested(&fx.repo, "Packages/manifest.json", "{}\n", "manifest");
    write_nested(&fx.repo, "Library/Artifacts/a.bin", "data");

    let wt_a = worktree::create(&fx.repo, &fx.worktrees, "t-ign-a", "main").unwrap();
    let rep_a = seed_worktree_with(
        &fx.repo,
        &wt_a.path,
        &WorktreeConfig::default(),
        &sha_a,
        SEED_TIMEOUT,
        &MockBackend::default(),
    );
    assert!(!rep_a.entries[0].seeded, "{rep_a:?}");
    assert!(
        rep_a.entries[0]
            .skip_reason
            .as_deref()
            .unwrap_or("")
            .starts_with("not_tracked_gitignored"),
        "{rep_a:?}"
    );
    assert!(!wt_a.path.join("Library").exists());
    assert!(git(&wt_a.path, &["status", "--porcelain"]).is_empty());

    let fx2 = Fixture::new();
    commit_file(
        &fx2.repo,
        ".gitignore",
        "Library/*\n!Library/Keep.txt\n",
        "negation",
    );
    commit_nested(
        &fx2.repo,
        "ProjectSettings/ProjectVersion.txt",
        "m_EditorVersion: 2022.3\n",
        "unity",
    );
    let sha_b = commit_nested(&fx2.repo, "Packages/manifest.json", "{}\n", "manifest");
    write_nested(&fx2.repo, "Library/Artifacts/a.bin", "data");
    write_nested(&fx2.repo, "Library/Keep.txt", "would-be-committed!");

    let wt_b = worktree::create(&fx2.repo, &fx2.worktrees, "t-ign-b", "main").unwrap();
    let rep_b = seed_worktree_with(
        &fx2.repo,
        &wt_b.path,
        &WorktreeConfig::default(),
        &sha_b,
        SEED_TIMEOUT,
        &MockBackend::default(),
    );
    assert!(!rep_b.entries[0].seeded, "{rep_b:?}");
    assert_eq!(
        rep_b.entries[0].skip_reason.as_deref(),
        Some("unignored_files_after_seed")
    );
    assert!(!wt_b.path.join("Library").exists());
    assert!(git(&wt_b.path, &["status", "--porcelain"]).is_empty());
}

#[cfg(unix)]
#[test]
fn seed_symlinks_relative_inside_ok_absolute_or_escaping_rejected() {
    use std::os::unix::fs::symlink;

    let fx = Fixture::new();
    commit_file(&fx.repo, ".gitignore", "Pods/\n", "ignore Pods");
    commit_file(&fx.repo, "Podfile", "platform :ios, '15.0'\n", "podfile");
    let sha = commit_file(&fx.repo, "Podfile.lock", "PODS: []\n", "podlock");

    write_nested(&fx.repo, "Pods/Manifest.lock", "PODS: []\n");
    write_nested(&fx.repo, "Pods/Real/file.txt", "hello");
    fs::create_dir_all(fx.repo.join("Pods/Alias")).unwrap();
    symlink("../Real/file.txt", fx.repo.join("Pods/Alias/link.txt")).unwrap();

    let wt_ok = worktree::create(&fx.repo, &fx.worktrees, "t-sym-ok", "main").unwrap();
    let rep_ok = seed_worktree_with(
        &fx.repo,
        &wt_ok.path,
        &WorktreeConfig::default(),
        &sha,
        SEED_TIMEOUT,
        &MockBackend::default(),
    );
    assert!(rep_ok.entries[0].seeded, "{rep_ok:?}");

    symlink("/etc/hosts", fx.repo.join("Pods/abs_link")).unwrap();
    let wt_abs = worktree::create(&fx.repo, &fx.worktrees, "t-sym-abs", "main").unwrap();
    let rep_abs = seed_worktree_with(
        &fx.repo,
        &wt_abs.path,
        &WorktreeConfig::default(),
        &sha,
        SEED_TIMEOUT,
        &MockBackend::default(),
    );
    assert!(!rep_abs.entries[0].seeded, "{rep_abs:?}");
    assert!(
        rep_abs.entries[0]
            .skip_reason
            .as_deref()
            .unwrap_or("")
            .starts_with("escaping_absolute_symlink"),
        "{rep_abs:?}"
    );
    assert!(!wt_abs.path.join("Pods").exists());
    fs::remove_file(fx.repo.join("Pods/abs_link")).unwrap();

    symlink("../../../../etc/hosts", fx.repo.join("Pods/esc_link")).unwrap();
    let wt_esc = worktree::create(&fx.repo, &fx.worktrees, "t-sym-esc", "main").unwrap();
    let rep_esc = seed_worktree_with(
        &fx.repo,
        &wt_esc.path,
        &WorktreeConfig::default(),
        &sha,
        SEED_TIMEOUT,
        &MockBackend::default(),
    );
    assert!(!rep_esc.entries[0].seeded, "{rep_esc:?}");
    assert!(
        rep_esc.entries[0]
            .skip_reason
            .as_deref()
            .unwrap_or("")
            .starts_with("escaping_relative_symlink"),
        "{rep_esc:?}"
    );
    assert!(!wt_esc.path.join("Pods").exists());
}

#[cfg(unix)]
#[test]
fn seed_overlap_skips_and_post_clone_reject_cleans_0555_tree() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let fx = Fixture::new();
    commit_file(&fx.repo, ".gitignore", "Pods/\n", "ignore Pods");
    commit_file(&fx.repo, "Podfile", "platform :ios, '15.0'\n", "podfile");
    let sha = commit_file(&fx.repo, "Podfile.lock", "PODS: []\n", "podlock");
    write_nested(&fx.repo, "Pods/Manifest.lock", "PODS: []\n");
    write_nested(&fx.repo, "Pods/ro/data.txt", "x");

    let cfg = WorktreeConfig {
        gate_copy_files: vec!["Pods/secret.plist".to_string()],
        ..WorktreeConfig::default()
    };
    let wt_ov = worktree::create(&fx.repo, &fx.worktrees, "t-ov", "main").unwrap();
    let rep_ov = seed_worktree_with(
        &fx.repo,
        &wt_ov.path,
        &cfg,
        &sha,
        SEED_TIMEOUT,
        &MockBackend::default(),
    );
    assert!(!rep_ov.entries[0].seeded, "{rep_ov:?}");
    assert!(
        rep_ov.entries[0]
            .skip_reason
            .as_deref()
            .unwrap_or("")
            .starts_with("overlaps_gate_copy_files"),
        "{rep_ov:?}"
    );
    assert!(!wt_ov.path.join("Pods").exists());

    symlink("/etc/hosts", fx.repo.join("Pods/ro/bad_abs")).unwrap();
    fs::set_permissions(fx.repo.join("Pods/ro"), fs::Permissions::from_mode(0o555)).unwrap();

    let wt_ro = worktree::create(&fx.repo, &fx.worktrees, "t-ro", "main").unwrap();
    let rep_ro = seed_worktree_with(
        &fx.repo,
        &wt_ro.path,
        &WorktreeConfig::default(),
        &sha,
        SEED_TIMEOUT,
        &MockBackend::default(),
    );
    let _ = fs::set_permissions(fx.repo.join("Pods/ro"), fs::Permissions::from_mode(0o755));
    assert!(!rep_ro.entries[0].seeded, "{rep_ro:?}");
    assert!(
        rep_ro.entries[0]
            .skip_reason
            .as_deref()
            .unwrap_or("")
            .starts_with("escaping_absolute_symlink"),
        "{rep_ro:?}"
    );
    assert!(
        fs::symlink_metadata(wt_ro.path.join("Pods")).is_err(),
        "0555 rejected clone must be fully removed"
    );
}

#[test]
fn seed_clone_pre_skip_created_false_preserves_existing_sentinel_at_dst() {
    let fx = Fixture::new();
    commit_file(&fx.repo, ".gitignore", "Pods/\n", "ignore Pods");
    commit_file(&fx.repo, "Podfile", "platform :ios, '15.0'\n", "podfile");
    let sha = commit_file(&fx.repo, "Podfile.lock", "PODS: []\n", "podlock");
    write_nested(&fx.repo, "Pods/Manifest.lock", "PODS: []\n");

    let wt = worktree::create(&fx.repo, &fx.worktrees, "t-pre-skip", "main").unwrap();
    let backend = MockBackend {
        pre_skip_with_sentinel: Some("raced_pre_skip".to_string()),
        ..MockBackend::default()
    };
    let report = seed_worktree_with(
        &fx.repo,
        &wt.path,
        &WorktreeConfig::default(),
        &sha,
        SEED_TIMEOUT,
        &backend,
    );
    assert_eq!(report.entries.len(), 1, "{report:?}");
    assert!(!report.entries[0].seeded, "{report:?}");
    assert_eq!(
        report.entries[0].skip_reason.as_deref(),
        Some("raced_pre_skip"),
        "{report:?}"
    );
    assert!(
        report.entries[0].fatal_cleanup_error.is_none(),
        "{report:?}"
    );
    assert_eq!(
        fs::read_to_string(wt.path.join("Pods/sentinel.txt")).unwrap(),
        "keep-me",
        "caller !skip.created branch must preserve sentinel at dst without deleting it"
    );
}
