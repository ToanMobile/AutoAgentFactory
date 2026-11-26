#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Hardening regression tests for primary `<main>/.git` state locking
//! (Seatbelt + Linux `bwrap`) and `[safety].deny_macos_keychain` policy.

use std::fs;
use std::path::Path;

use aaf_domain::config::GlobalConfig;
use aaf_domain::keychain::KeychainPolicy;
use aaf_engine::executors::linux_sandbox::build_bwrap_args;
use aaf_engine::executors::sandbox::SandboxPaths;
use aaf_engine::executors::should_deny_macos_keychain;

fn has_triple(argv: &[String], a: &str, b: &str, c: &str) -> bool {
    argv.windows(3).any(|w| w[0] == a && w[1] == b && w[2] == c)
}

#[test]
fn keychain_policy_serde_and_global_config_defaults() {
    for (policy, expected) in [
        (KeychainPolicy::Auto, "\"auto\""),
        (KeychainPolicy::Always, "\"always\""),
        (KeychainPolicy::Never, "\"never\""),
    ] {
        let json = serde_json::to_string(&policy).unwrap();
        assert_eq!(json, expected);
        let parsed: KeychainPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, policy);
    }

    let default_cfg: GlobalConfig = toml::from_str("").unwrap();
    assert_eq!(default_cfg.safety.deny_macos_keychain, KeychainPolicy::Auto);

    let always_cfg: GlobalConfig =
        toml::from_str("[safety]\ndeny_macos_keychain = \"always\"\n").unwrap();
    assert_eq!(
        always_cfg.safety.deny_macos_keychain,
        KeychainPolicy::Always
    );

    let never_cfg: GlobalConfig =
        toml::from_str("[safety]\ndeny_macos_keychain = \"never\"\n").unwrap();
    assert_eq!(never_cfg.safety.deny_macos_keychain, KeychainPolicy::Never);

    let bad = toml::from_str::<GlobalConfig>("[safety]\ndeny_macos_keychain = \"bogus\"\n");
    assert!(bad.is_err(), "invalid keychain policy must fail closed");
}

#[test]
fn should_deny_macos_keychain_matrix() {
    let empty: Vec<(String, String)> = Vec::new();
    let blank_key = vec![("ANTHROPIC_API_KEY".to_string(), "   ".to_string())];
    let api_key = vec![("ANTHROPIC_API_KEY".to_string(), "sk-ant-test".to_string())];

    // Auto: keep open for OAuth Claude and Antigravity (`agy`); close for API-key Claude and other executors.
    assert!(!should_deny_macos_keychain(
        KeychainPolicy::Auto,
        "claude",
        &empty
    ));
    assert!(!should_deny_macos_keychain(
        KeychainPolicy::Auto,
        "CLAUDE",
        &blank_key
    ));
    assert!(!should_deny_macos_keychain(
        KeychainPolicy::Auto,
        "antigravity",
        &empty
    ));
    assert!(!should_deny_macos_keychain(
        KeychainPolicy::Auto,
        "ANTIGRAVITY",
        &empty
    ));
    assert!(!should_deny_macos_keychain(
        KeychainPolicy::Auto,
        "agy",
        &empty
    ));
    assert!(should_deny_macos_keychain(
        KeychainPolicy::Auto,
        "claude",
        &api_key
    ));
    for exec in ["codex", "gemini", "opencode", "custom"] {
        assert!(
            should_deny_macos_keychain(KeychainPolicy::Auto, exec, &empty),
            "Auto must deny macOS Keychain for non-OAuth-Keychain executor {exec}"
        );
    }

    // Always: deny regardless of executor or auth method.
    assert!(should_deny_macos_keychain(
        KeychainPolicy::Always,
        "claude",
        &empty
    ));
    assert!(should_deny_macos_keychain(
        KeychainPolicy::Always,
        "antigravity",
        &empty
    ));
    assert!(should_deny_macos_keychain(
        KeychainPolicy::Always,
        "codex",
        &empty
    ));

    // Never: keep open regardless of executor or API key.
    assert!(!should_deny_macos_keychain(
        KeychainPolicy::Never,
        "claude",
        &api_key
    ));
    assert!(!should_deny_macos_keychain(
        KeychainPolicy::Never,
        "codex",
        &api_key
    ));
}

#[test]
fn linux_bwrap_protects_linked_worktree_primary_git_state() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let main_repo = root.join("main");
    let git_dir = main_repo.join(".git");
    let wt_root = root.join("worktrees");
    let wt = wt_root.join("repo/t1");
    let wt_admin = git_dir.join("worktrees/t1");

    for dir in [
        git_dir.join("hooks"),
        git_dir.join("info"),
        git_dir.join("refs/heads/release"),
        git_dir.join("refs/aaf"),
        wt_admin.clone(),
        wt.clone(),
    ] {
        fs::create_dir_all(dir).unwrap();
    }
    for (rel, body) in [
        ("HEAD", "ref: refs/heads/main\n"),
        ("index", "DIRC"),
        ("packed-refs", "# pack-refs\n"),
        ("info/exclude", "*.log\n"),
        (
            "refs/heads/main",
            "1111111111111111111111111111111111111111\n",
        ),
        (
            "refs/heads/master",
            "1111111111111111111111111111111111111111\n",
        ),
        (
            "refs/heads/develop",
            "1111111111111111111111111111111111111111\n",
        ),
        (
            "refs/heads/release/v1",
            "1111111111111111111111111111111111111111\n",
        ),
        ("worktrees/t1/HEAD", "ref: refs/heads/aaf/t1\n"),
        ("worktrees/t1/commondir", "../..\n"),
        ("worktrees/t1/gitdir", "/tmp/wt/.git\n"),
    ] {
        fs::write(git_dir.join(rel), body).unwrap();
    }
    fs::write(wt.join(".git"), format!("gitdir: {}\n", wt_admin.display())).unwrap();

    let s = |p: &Path| p.to_string_lossy().into_owned();
    let linked_paths = SandboxPaths {
        worktree: Some(s(&wt)),
        worktrees_root: Some(s(&wt_root)),
        git_dirs: vec![s(&git_dir)],
        ..SandboxPaths::default()
    };
    let argv = build_bwrap_args(&linked_paths, "/usr/bin/codex", &[]).unwrap();

    for rel in [
        "HEAD",
        "index",
        "packed-refs",
        "info/exclude",
        "refs/heads/main",
        "refs/heads/master",
        "refs/heads/develop",
        "refs/heads/release",
    ] {
        let target = s(&git_dir.join(rel));
        assert!(
            has_triple(&argv, "--ro-bind", &target, &target),
            "expected --ro-bind for {rel} in linked worktree: {argv:?}"
        );
    }
    assert!(
        has_triple(&argv, "--bind", &s(&wt_admin), &s(&wt_admin)),
        "linked worktree admin dir must stay RW: {argv:?}"
    );
    assert!(has_triple(
        &argv,
        "--ro-bind",
        &s(&wt_admin.join("commondir")),
        &s(&wt_admin.join("commondir"))
    ));

    // Normal (primary) checkout still protects info/exclude and packed-refs,
    // without locking its own HEAD, index, or refs/heads/main.
    let primary_paths = SandboxPaths {
        worktree: Some(s(&main_repo)),
        git_dirs: vec![s(&git_dir)],
        ..SandboxPaths::default()
    };
    let primary_argv = build_bwrap_args(&primary_paths, "/usr/bin/codex", &[]).unwrap();
    for rel in ["info/exclude", "packed-refs"] {
        let target = s(&git_dir.join(rel));
        assert!(
            has_triple(&primary_argv, "--ro-bind", &target, &target),
            "expected --ro-bind for {rel} in primary checkout: {primary_argv:?}"
        );
    }
    for rel in ["HEAD", "index", "refs/heads/main"] {
        let target = s(&git_dir.join(rel));
        assert!(
            !has_triple(&primary_argv, "--ro-bind", &target, &target),
            "primary checkout must keep {rel} writable: {primary_argv:?}"
        );
    }
}

#[cfg(target_os = "macos")]
mod macos_real {
    use std::fs;
    use std::path::Path;
    use std::time::Duration;

    use aaf_domain::ports::SandboxPolicy;
    use aaf_engine::executors::pty::{CommandSpec, PtyOutput, run_pty};
    use aaf_engine::executors::sandbox::{SandboxPaths, seatbelt_profile, wrap};

    const GIT_FLAGS: [&str; 8] = [
        "-c",
        "user.name=aaf-test",
        "-c",
        "user.email=aaf-test@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "core.hooksPath=/dev/null",
    ];

    fn git(repo: &Path, args: &[&str]) -> String {
        let mut full = GIT_FLAGS.to_vec();
        full.extend_from_slice(args);
        aaf_fp::git(repo, &full, &[], aaf_fp::GIT_TIMEOUT).unwrap()
    }

    fn run_under_profile(cwd: &Path, profile: &str, script: &str) -> PtyOutput {
        let cmd = CommandSpec {
            program: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            cwd: Some(cwd.to_string_lossy().into_owned()),
            ..CommandSpec::default()
        };
        let wrapped = wrap(&cmd, profile).unwrap();
        let out = run_pty(&wrapped, Duration::from_secs(60), 32 * 1024, None).unwrap();
        assert!(!out.timed_out, "timed out: {}", out.tail);
        out
    }

    #[test]
    fn linked_worktree_allows_commit_and_denies_main_git_state() {
        if !Path::new("/usr/bin/sandbox-exec").exists() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (main, wt) = (root.join("main"), root.join("wt-t1"));
        fs::create_dir(&main).unwrap();
        git(&main, &["init", "-q", "-b", "main"]);
        fs::write(main.join("a.txt"), "a\n").unwrap();
        git(&main, &["add", "a.txt"]);
        git(&main, &["commit", "-q", "-m", "initial"]);
        git(&main, &["branch", "develop"]);
        git(&main, &["branch", "release/v1"]);
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "aaf/t1",
                wt.to_str().unwrap(),
            ],
        );

        let git_dir = main.join(".git");
        fs::write(
            git_dir.join("packed-refs"),
            "# pack-refs with: peeled fully-peeled sorted \n",
        )
        .unwrap();
        fs::write(git_dir.join("info/exclude"), "# exclude\n").unwrap();

        let head_before = fs::read(git_dir.join("HEAD")).unwrap();
        let index_before = fs::read(git_dir.join("index")).unwrap();
        let packed_before = fs::read(git_dir.join("packed-refs")).unwrap();
        let exclude_before = fs::read(git_dir.join("info/exclude")).unwrap();
        let main_ref_before = fs::read(git_dir.join("refs/heads/main")).unwrap();
        let dev_ref_before = fs::read(git_dir.join("refs/heads/develop")).unwrap();

        let extra = SandboxPaths {
            worktree: Some(wt.to_string_lossy().into_owned()),
            git_dirs: vec![git_dir.to_string_lossy().into_owned()],
            ..SandboxPaths::default()
        };
        let profile = seatbelt_profile(
            &SandboxPolicy {
                enabled: true,
                ..SandboxPolicy::default()
            },
            &extra,
        )
        .unwrap();

        let flags = GIT_FLAGS.join(" ");
        let commit_out = run_under_profile(
            &wt,
            &profile,
            &format!("echo ok > b.txt && git add b.txt && git {flags} commit -q -m 'wt commit'"),
        );
        assert_eq!(commit_out.exit_code, Some(0), "{}", commit_out.tail);
        assert_eq!(git(&wt, &["log", "-1", "--format=%s"]), "wt commit");

        let g = git_dir.display();
        let probes = [
            ("head", format!("echo x > '{g}/HEAD'")),
            ("index", format!("echo x > '{g}/index'")),
            ("packed-refs", format!("echo x > '{g}/packed-refs'")),
            ("packed-lock", format!("echo x > '{g}/packed-refs.lock'")),
            ("info-exclude", format!("echo x > '{g}/info/exclude'")),
            ("ref-main", format!("echo x > '{g}/refs/heads/main'")),
            ("ref-develop", format!("echo x > '{g}/refs/heads/develop'")),
            (
                "ref-release",
                format!("echo x > '{g}/refs/heads/release/v1'"),
            ),
            ("mv-info", format!("mv '{g}/info' '{g}/info-moved'")),
            (
                "mv-heads",
                format!("mv '{g}/refs/heads' '{g}/refs/heads-moved'"),
            ),
        ];
        for (label, script) in probes {
            let out = run_under_profile(&wt, &profile, &script);
            assert_ne!(
                out.exit_code,
                Some(0),
                "expected {label} write to fail: {}",
                out.tail
            );
            assert!(
                out.tail.contains("Operation not permitted"),
                "expected EPERM for {label}: {}",
                out.tail
            );
        }

        assert_eq!(fs::read(git_dir.join("HEAD")).unwrap(), head_before);
        assert_eq!(fs::read(git_dir.join("index")).unwrap(), index_before);
        assert_eq!(
            fs::read(git_dir.join("packed-refs")).unwrap(),
            packed_before
        );
        assert_eq!(
            fs::read(git_dir.join("info/exclude")).unwrap(),
            exclude_before
        );
        assert_eq!(
            fs::read(git_dir.join("refs/heads/main")).unwrap(),
            main_ref_before
        );
        assert_eq!(
            fs::read(git_dir.join("refs/heads/develop")).unwrap(),
            dev_ref_before
        );
        assert!(!git_dir.join("packed-refs.lock").exists());
    }

    #[test]
    fn deny_keychain_dir_blocks_reads_and_emits_mach_lookup_denies() {
        if !Path::new("/usr/bin/sandbox-exec").exists() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let work = root.join("work");
        let kc_dir = root.join("Library/Keychains");
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&kc_dir).unwrap();
        let secret = kc_dir.join("login.keychain-db");
        fs::write(&secret, "SECRET-KEYCHAIN-TOKEN").unwrap();

        let extra = SandboxPaths {
            worktree: Some(work.to_string_lossy().into_owned()),
            deny_keychain_dir: Some(kc_dir.to_string_lossy().into_owned()),
            ..SandboxPaths::default()
        };
        let profile = seatbelt_profile(
            &SandboxPolicy {
                enabled: true,
                ..SandboxPolicy::default()
            },
            &extra,
        )
        .unwrap();

        assert!(profile.contains("com.apple.SecurityServer"), "{profile}");
        assert!(profile.contains("com.apple.securityd.xpc"), "{profile}");
        assert!(
            profile.contains(&format!(
                "(deny file-read* file-write* (subpath \"{}\"))",
                kc_dir.display()
            )),
            "{profile}"
        );

        let out = run_under_profile(&work, &profile, &format!("cat '{}'", secret.display()));
        assert_ne!(out.exit_code, Some(0), "{}", out.tail);
        assert!(out.tail.contains("Operation not permitted"), "{}", out.tail);
        assert!(!out.tail.contains("SECRET-KEYCHAIN-TOKEN"), "{}", out.tail);
    }
}
