#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Step 3 & Round-1 (Findings #5, #9, #10, #14) tests: S1 markers, S2.4 lock/filter
//! checks, S2.5 fail-closed Unity Editor detection, and S4 case-insensitive/fail-closed overlap.

#[path = "git_support.rs"]
mod git_support;
#[path = "seed_support.rs"]
mod seed_support;

use std::fs;
use std::path::Path;

use aaf_domain::worktree_config::WorktreeConfig;
use aaf_engine::git::hydration::gitignore_source_is_valid;
use aaf_engine::git::hydration::seed::{
    SEED_TIMEOUT, SeedCandidate, SeedKind, SeedTarget, check_npm_lock_match, check_pods_lock_match,
    check_unity_editor_not_open, check_unity_state_match, detect_seed_candidates,
    overlaps_worktree_config, seed_worktree_with, unity_editor_conflict,
};
use aaf_engine::git::worktree;
use git_support::{Fixture, commit_file, git};
use seed_support::MockBackend;

fn write_nested(root: &Path, rel: &str, content: &str) {
    let full = root.join(rel);
    if let Some(p) = full.parent() {
        fs::create_dir_all(p).unwrap();
    }
    fs::write(full, content).unwrap();
}

fn commit_nested(repo: &Path, rel: &str, content: &str, msg: &str) -> String {
    let full = repo.join(rel);
    if let Some(p) = full.parent() {
        fs::create_dir_all(p).unwrap();
    }
    commit_file(repo, rel, content, msg)
}

#[test]
fn seed_markers_read_from_base_sha_exact_list_root_pods_and_rejects_symlinks() {
    let fx = Fixture::new();
    let base_empty = fx.head(&fx.repo, "HEAD");

    // Uncommitted markers on disk must NOT be detected when inspecting base_empty.
    write_nested(&fx.repo, "package.json", "{}\n");
    write_nested(&fx.repo, "package-lock.json", "{}\n");
    write_nested(
        &fx.repo,
        "ProjectSettings/ProjectVersion.txt",
        "m_EditorVersion: 2022.3\n",
    );
    assert_eq!(
        detect_seed_candidates(&fx.repo, &base_empty).unwrap(),
        vec![]
    );

    // Commit npm + root Pods + ios Pods + Unity markers.
    commit_nested(&fx.repo, "package.json", "{}\n", "pkg");
    commit_nested(&fx.repo, "package-lock.json", "{}\n", "lock");
    commit_nested(&fx.repo, "Podfile", "platform :ios, '15.0'\n", "root-pod");
    commit_nested(&fx.repo, "Podfile.lock", "PODS: []\n", "root-podlock");
    commit_nested(
        &fx.repo,
        "ios/Podfile",
        "platform :ios, '15.0'\n",
        "ios-pod",
    );
    commit_nested(&fx.repo, "ios/Podfile.lock", "PODS: []\n", "ios-podlock");
    let sha = commit_nested(
        &fx.repo,
        "ProjectSettings/ProjectVersion.txt",
        "m_EditorVersion: 2022.3.10f1\n",
        "unity",
    );

    // Deleting from disk does not change base_sha detection; assert exact candidate list.
    fs::remove_file(fx.repo.join("package.json")).unwrap();
    let candidates = detect_seed_candidates(&fx.repo, &sha).unwrap();
    assert_eq!(
        candidates,
        vec![
            SeedCandidate::Target(SeedTarget {
                kind: SeedKind::Unity,
                rel_dir: "Library",
            }),
            SeedCandidate::Target(SeedTarget {
                kind: SeedKind::Npm,
                rel_dir: "node_modules",
            }),
            SeedCandidate::Target(SeedTarget {
                kind: SeedKind::CocoaPods {
                    lock_rel: "Podfile.lock",
                },
                rel_dir: "Pods",
            }),
            SeedCandidate::Target(SeedTarget {
                kind: SeedKind::CocoaPods {
                    lock_rel: "ios/Podfile.lock",
                },
                rel_dir: "ios/Pods",
            }),
        ]
    );

    // Symlink marker (git mode 120000) must be rejected, not treated as a regular blob.
    #[cfg(unix)]
    {
        let fx_sym = Fixture::new();
        write_nested(&fx_sym.repo, "real_ver.txt", "m_EditorVersion: 2022.3\n");
        fs::create_dir_all(fx_sym.repo.join("ProjectSettings")).unwrap();
        std::os::unix::fs::symlink(
            "../real_ver.txt",
            fx_sym.repo.join("ProjectSettings/ProjectVersion.txt"),
        )
        .unwrap();
        git(&fx_sym.repo, &["add", "-A"]);
        git(&fx_sym.repo, &["commit", "-q", "-m", "symlink marker"]);
        let sym_sha = fx_sym.head(&fx_sym.repo, "HEAD");
        assert_eq!(
            detect_seed_candidates(&fx_sym.repo, &sym_sha).unwrap(),
            vec![],
            "mode 120000 symlink marker must not be detected"
        );
    }
}

#[test]
fn seed_markers_yarn_and_pnpm_skip_unsupported_package_manager() {
    let fx = Fixture::new();
    commit_nested(&fx.repo, "package.json", "{}\n", "pkg");
    let sha_yarn = commit_nested(&fx.repo, "yarn.lock", "# yarn\n", "yarn");
    assert_eq!(
        detect_seed_candidates(&fx.repo, &sha_yarn).unwrap(),
        vec![SeedCandidate::Skipped {
            rel_dir: "node_modules",
            reason: "unsupported_package_manager".to_string(),
        }]
    );

    let fx2 = Fixture::new();
    commit_nested(&fx2.repo, "package.json", "{}\n", "pkg");
    commit_nested(&fx2.repo, "package-lock.json", "{}\n", "npm-lock");
    let sha_pnpm = commit_nested(&fx2.repo, "pnpm-lock.yaml", "lockfileVersion: 9\n", "pnpm");
    assert_eq!(
        detect_seed_candidates(&fx2.repo, &sha_pnpm).unwrap(),
        vec![SeedCandidate::Skipped {
            rel_dir: "node_modules",
            reason: "unsupported_package_manager".to_string(),
        }]
    );
}

#[test]
fn seed_npm_lock_matching_rules_full_edge_cases() {
    let base_v2 = br#"{
      "lockfileVersion": 2,
      "packages": {
        "": { "name": "app", "version": "1.0.0" },
        "node_modules/lodash": {
          "version": "4.17.21",
          "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
          "integrity": "sha512-abc",
          "link": false
        },
        "node_modules/fsevents": {
          "version": "2.3.3",
          "resolved": "https://registry.npmjs.org/fsevents/-/fsevents-2.3.3.tgz",
          "integrity": "sha512-fse",
          "optional": true
        },
        "node_modules/@esbuild/linux-x64": {
          "version": "0.21.5",
          "resolved": "https://registry.npmjs.org/@esbuild/linux-x64/-/linux-x64-0.21.5.tgz",
          "integrity": "sha512-esb",
          "devOptional": true
        }
      }
    }"#;

    let installed_v3_ok = br#"{
      "lockfileVersion": 3,
      "packages": {
        "node_modules/lodash": {
          "version": "4.17.21",
          "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
          "integrity": "sha512-abc",
          "link": false
        }
      }
    }"#;
    check_npm_lock_match(base_v2, installed_v3_ok).unwrap();

    // Finding #9b: Rule (a) must ignore root key `""` in installed lock `I` even if it differs.
    let installed_with_different_root_key = br#"{
      "lockfileVersion": 3,
      "packages": {
        "": { "name": "different-root", "version": "9.9.9" },
        "node_modules/lodash": {
          "version": "4.17.21",
          "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
          "integrity": "sha512-abc",
          "link": false
        }
      }
    }"#;
    check_npm_lock_match(base_v2, installed_with_different_root_key).unwrap();

    // Version, integrity, resolved, and link mismatches -> Err.
    for bad in [
        r#"{"lockfileVersion":3,"packages":{"node_modules/lodash":{"version":"4.17.20","resolved":"https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz","integrity":"sha512-abc","link":false}}}"#,
        r#"{"lockfileVersion":3,"packages":{"node_modules/lodash":{"version":"4.17.21","resolved":"https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz","integrity":"sha512-BAD","link":false}}}"#,
        r#"{"lockfileVersion":3,"packages":{"node_modules/lodash":{"version":"4.17.21","resolved":"https://evil.example/lodash.tgz","integrity":"sha512-abc","link":false}}}"#,
        r#"{"lockfileVersion":3,"packages":{"node_modules/lodash":{"version":"4.17.21","resolved":"https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz","integrity":"sha512-abc","link":true}}}"#,
        // devOptional present in I but mismatched integrity -> Err (rule (a)).
        r#"{"lockfileVersion":3,"packages":{"node_modules/lodash":{"version":"4.17.21","resolved":"https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz","integrity":"sha512-abc","link":false},"node_modules/@esbuild/linux-x64":{"version":"0.21.5","resolved":"https://registry.npmjs.org/@esbuild/linux-x64/-/linux-x64-0.21.5.tgz","integrity":"sha512-WRONG"}}}"#,
        // optional present in I but mismatched -> Err (rule (a)).
        r#"{"lockfileVersion":3,"packages":{"node_modules/lodash":{"version":"4.17.21","resolved":"https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz","integrity":"sha512-abc","link":false},"node_modules/fsevents":{"version":"2.3.3","resolved":"https://registry.npmjs.org/fsevents/-/fsevents-2.3.3.tgz","integrity":"sha512-WRONG"}}}"#,
        // extra pkg in I, missing required pkg, v1, missing packages, packages=[], packages=null.
        r#"{"lockfileVersion":3,"packages":{"node_modules/lodash":{"version":"4.17.21","resolved":"https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz","integrity":"sha512-abc","link":false},"node_modules/rogue":{"version":"1.0.0"}}}"#,
        r#"{"lockfileVersion":3,"packages":{}}"#,
        r#"{"lockfileVersion":1,"packages":{}}"#,
        r#"{"lockfileVersion":3}"#,
        r#"{"lockfileVersion":3,"packages":[]}"#,
        r#"{"lockfileVersion":3,"packages":null}"#,
    ] {
        assert!(
            check_npm_lock_match(base_v2, bad.as_bytes()).is_err(),
            "expected Err for installed={bad}"
        );
    }
    assert!(
        check_npm_lock_match(br#"{"lockfileVersion":1,"packages":{}}"#, installed_v3_ok).is_err()
    );
    assert!(
        check_npm_lock_match(br#"{"lockfileVersion":3,"packages":[]}"#, installed_v3_ok).is_err()
    );
    assert!(
        check_npm_lock_match(br#"{"lockfileVersion":3,"packages":null}"#, installed_v3_ok).is_err()
    );
    assert!(check_npm_lock_match(b"{broken", installed_v3_ok).is_err());
}

#[test]
fn seed_pods_manifest_exact_byte_match() {
    let lock = b"PODS:\n  - Alamofire (5.9.0)\n";
    assert!(check_pods_lock_match(lock, lock).is_ok());
    assert!(check_pods_lock_match(lock, b"PODS:\n  - Alamofire (5.8.0)\n").is_err());
}

#[test]
fn seed_unity_state_match_missing_packages_lock_and_crlf_filter() {
    let fx = Fixture::new();
    commit_nested(
        &fx.repo,
        "ProjectSettings/ProjectVersion.txt",
        "m_EditorVersion: 2022.3.10f1\n",
        "unity version",
    );
    let sha_no_lock = commit_nested(
        &fx.repo,
        "Packages/manifest.json",
        "{\"dependencies\":{}}\n",
        "unity manifest",
    );
    check_unity_state_match(&fx.repo, &sha_no_lock).unwrap();

    write_nested(
        &fx.repo,
        "Packages/packages-lock.json",
        "{\"dependencies\":{}}\n",
    );
    assert!(check_unity_state_match(&fx.repo, &sha_no_lock).is_err());

    let sha_with_lock = commit_nested(
        &fx.repo,
        "Packages/packages-lock.json",
        "{\"dependencies\":{}}\n",
        "unity lock",
    );
    check_unity_state_match(&fx.repo, &sha_with_lock).unwrap();

    write_nested(
        &fx.repo,
        "ProjectSettings/ProjectVersion.txt",
        "m_EditorVersion: 6000.0\n",
    );
    assert!(check_unity_state_match(&fx.repo, &sha_with_lock).is_err());

    write_nested(
        &fx.repo,
        "ProjectSettings/ProjectVersion.txt",
        "m_EditorVersion: 2022.3.10f1\n",
    );
    write_nested(
        &fx.repo,
        "Packages/manifest.json",
        "{\"dependencies\":{\"a\":\"1\"}}\n",
    );
    assert!(check_unity_state_match(&fx.repo, &sha_with_lock).is_err());

    // CRLF filter via .gitattributes: blob in git is LF, checked-out file on disk is CRLF.
    let fx_crlf = Fixture::new();
    commit_file(
        &fx_crlf.repo,
        ".gitattributes",
        "* text=auto eol=crlf\n",
        "crlf attr",
    );
    commit_nested(
        &fx_crlf.repo,
        "ProjectSettings/ProjectVersion.txt",
        "m_EditorVersion: 2022.3.10f1\n",
        "ver",
    );
    let sha_crlf = commit_nested(
        &fx_crlf.repo,
        "Packages/manifest.json",
        "{\"dependencies\":{}}\n",
        "manifest",
    );
    // Re-checkout to apply eol=crlf filter on disk.
    fs::remove_file(fx_crlf.repo.join("ProjectSettings/ProjectVersion.txt")).unwrap();
    fs::remove_file(fx_crlf.repo.join("Packages/manifest.json")).unwrap();
    git(
        &fx_crlf.repo,
        &[
            "checkout",
            "--",
            "ProjectSettings/ProjectVersion.txt",
            "Packages/manifest.json",
        ],
    );
    let disk_ver = fs::read(fx_crlf.repo.join("ProjectSettings/ProjectVersion.txt")).unwrap();
    assert!(
        disk_ver.ends_with(b"\r\n"),
        "expected CRLF checkout on disk: {disk_ver:?}"
    );
    check_unity_state_match(&fx_crlf.repo, &sha_crlf).unwrap();
}

#[test]
fn seed_unity_editor_ps_detection_fail_closed_spaces_quotes_hub_and_lockfile() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let main_spaces = root.join("My Unity Game");
    let main_child = main_spaces.join("SubProject");
    let sibling = root.join("My Unity Game 2");
    fs::create_dir_all(&main_child).unwrap();
    fs::create_dir_all(&sibling).unwrap();

    // 1. Unity open on main (path with spaces) -> conflict.
    let ps_open = format!(
        "101 /sbin/launchd\n\
         456 /Applications/Unity/Hub/Editor/2022.3/Unity.app/Contents/MacOS/Unity -projectPath {} -batchmode -quit\n",
        main_spaces.display()
    );
    assert_eq!(
        unity_editor_conflict(&ps_open, &main_spaces, false).as_deref(),
        Some("unity_editor_open")
    );

    // 2. Child path of main & quoted path & case-insensitive -projectpath -> conflict.
    let ps_child = format!(
        "457 /opt/unity/Unity -projectpath \"{}\"\n",
        main_child.display()
    );
    assert_eq!(
        unity_editor_conflict(&ps_child, &main_spaces, false).as_deref(),
        Some("unity_editor_open")
    );

    // 3. Existing sibling dir -> NOT a conflict.
    let ps_sib = format!(
        "458 /opt/unity/Unity -projectPath {} -quit\n",
        sibling.display()
    );
    assert_eq!(unity_editor_conflict(&ps_sib, &main_spaces, false), None);

    // 4. Relative or nonexistent -projectPath -> FAIL-CLOSED `unity_project_path_unresolved`.
    let ps_rel = "459 /opt/unity/Unity -projectPath ./relative/proj -quit\n";
    assert_eq!(
        unity_editor_conflict(ps_rel, &main_spaces, false).as_deref(),
        Some("unity_project_path_unresolved")
    );
    let ps_missing = format!(
        "460 /opt/unity/Unity -projectPath {} -quit\n",
        root.join("does-not-exist").display()
    );
    assert_eq!(
        unity_editor_conflict(&ps_missing, &main_spaces, false).as_deref(),
        Some("unity_project_path_unresolved")
    );

    // 5. Unity Hub must NOT be misidentified as Unity Editor (even when UnityLockfile exists!).
    let ps_hub = "500 /Applications/Unity Hub.app/Contents/MacOS/Unity Hub --no-sandbox\n";
    assert_eq!(unity_editor_conflict(ps_hub, &main_spaces, true), None);

    // 6. Real Temp/UnityLockfile on disk + unknown Unity Editor process -> conflict.
    let ps_unknown = "800 /Applications/Unity.app/Contents/MacOS/Unity -useHub\n";
    assert!(check_unity_editor_not_open(&main_spaces, Ok(ps_unknown)).is_ok());
    write_nested(&main_spaces, "Temp/UnityLockfile", "");
    let err = check_unity_editor_not_open(&main_spaces, Ok(ps_unknown)).unwrap_err();
    assert_eq!(err, "unity_lockfile_with_unknown_unity_process");

    // 7. `ps` error -> fail-closed.
    let ps_err = check_unity_editor_not_open(&main_spaces, Err("ps timed out")).unwrap_err();
    assert!(ps_err.starts_with("unity_ps_failed"), "{ps_err}");

    // 8. Finding #6: main path containing ` -<letter>` (e.g. `My -Game`) when prefix `My` ALSO
    // exists on disk must still be detected as `unity_editor_open` (fail-closed raw match).
    let dash_prefix = root.join("My");
    let dash_main = root.join("My -Game");
    fs::create_dir_all(&dash_prefix).unwrap();
    fs::create_dir_all(&dash_main).unwrap();
    let ps_dash = format!(
        "900 /Applications/Unity.app/Contents/MacOS/Unity -projectPath {} -batchmode\n",
        dash_main.display()
    );
    assert_eq!(
        unity_editor_conflict(&ps_dash, &dash_main, false).as_deref(),
        Some("unity_editor_open"),
        "Unity project path containing ` -<letter>` must still be detected"
    );
}

#[test]
fn seed_overlap_checks_clone_copy_gate_case_insensitive_and_invalid_fail_closed() {
    let cfg = WorktreeConfig {
        clone_dirs: vec!["Library".to_string()],
        copy_files: vec!["node_modules/custom.cfg".to_string()],
        gate_copy_files: vec!["IOS/Pods/Keys.xcconfig".to_string()],
        setup_cmd: String::new(),
    };
    assert!(overlaps_worktree_config(Path::new("Library"), &cfg).is_some());
    assert!(overlaps_worktree_config(Path::new("library"), &cfg).is_some());
    assert!(overlaps_worktree_config(Path::new("node_modules"), &cfg).is_some());
    // Case-insensitive APFS overlap: `IOS/Pods/Keys.xcconfig` blocks `ios/Pods`.
    assert!(overlaps_worktree_config(Path::new("ios/Pods"), &cfg).is_some());
    assert!(overlaps_worktree_config(Path::new("Pods"), &cfg).is_none());
    assert!(overlaps_worktree_config(Path::new("LibraryExtra"), &cfg).is_none());

    // Invalid entry in config must fail closed (treated as overlap, not silently ignored).
    let bad_cfg = WorktreeConfig {
        clone_dirs: vec!["../bad".to_string()],
        ..WorktreeConfig::default()
    };
    assert!(
        overlaps_worktree_config(Path::new("Pods"), &bad_cfg).is_some(),
        "invalid worktree config entry must fail closed as overlap"
    );
}

#[test]
fn gitignore_source_is_valid_accepts_repo_gitignore_and_rejects_untrusted_sources() {
    assert!(gitignore_source_is_valid(".gitignore"));
    assert!(gitignore_source_is_valid("sub/.gitignore"));
    assert!(gitignore_source_is_valid("a/b/c/.gitignore"));

    for rejected in [
        "info/exclude",
        ".git/info/exclude",
        "/.gitignore",
        "/tmp/repo/.gitignore",
        "sub/.GIT/x/.gitignore",
        "sub/.git/.gitignore",
        ".gitignore.local",
        "sub/exclude",
        "../.gitignore",
        "./.gitignore",
    ] {
        assert!(
            !gitignore_source_is_valid(rejected),
            "expected gitignore_source_is_valid({rejected:?}) to be false"
        );
    }
}

#[test]
fn seed_target_ignored_only_via_git_info_exclude_skips_and_does_not_create_dst() {
    let fx = Fixture::new();
    commit_file(&fx.repo, "Podfile", "platform :ios, '15.0'\n", "podfile");
    let sha = commit_file(
        &fx.repo,
        "Podfile.lock",
        "PODS:\n  - Foo (1.0)\n",
        "podlock",
    );
    write_nested(&fx.repo, "Pods/Manifest.lock", "PODS:\n  - Foo (1.0)\n");
    write_nested(&fx.repo, "Pods/Foo/Foo.swift", "let x = 1\n");

    // Ignore Pods/ ONLY via `.git/info/exclude`, with no tracked `.gitignore` rule.
    fs::write(fx.repo.join(".git/info/exclude"), "Pods/\n").unwrap();

    let wt = worktree::create(&fx.repo, &fx.worktrees, "t-info-exclude", "main").unwrap();
    let report = seed_worktree_with(
        &fx.repo,
        &wt.path,
        &WorktreeConfig::default(),
        &sha,
        SEED_TIMEOUT,
        &MockBackend::default(),
    );
    assert_eq!(report.entries.len(), 1);
    assert!(!report.entries[0].seeded, "{report:?}");
    let reason = report.entries[0].skip_reason.as_deref().unwrap_or("");
    assert!(
        reason.starts_with("not_tracked_gitignored"),
        "expected not_tracked_gitignored skip reason, got {reason:?}"
    );
    assert!(
        reason.contains("must be ignored by a repo .gitignore"),
        "expected repo .gitignore source rejection, got {reason:?}"
    );
    assert!(
        !wt.path.join("Pods").exists(),
        "dst must not be created when ignored only via .git/info/exclude"
    );
}
