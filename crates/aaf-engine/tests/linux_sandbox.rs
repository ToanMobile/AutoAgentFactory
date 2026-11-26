#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

use aaf_engine::executors::linux_sandbox::{
    LinuxIsolation, build_bwrap_args, detect_container, linux_sandbox_status, wrap,
};
use aaf_engine::executors::pty::CommandSpec;
use aaf_engine::executors::sandbox::SandboxPaths;

fn find_flag_pair(argv: &[String], flag: &str, a: &str, b: &str) -> Option<usize> {
    argv.windows(3)
        .position(|w| w[0] == flag && w[1] == a && w[2] == b)
}

fn find_flag_single(argv: &[String], flag: &str, val: &str) -> Option<usize> {
    argv.windows(2).position(|w| w[0] == flag && w[1] == val)
}

#[test]
fn detect_container_recognizes_docker_podman_kubernetes_containerd_and_none() {
    assert_eq!(detect_container(true, None, None), Some("docker"));
    assert_eq!(
        detect_container(false, None, Some("podman")),
        Some("podman")
    );
    assert_eq!(
        detect_container(false, None, Some("docker")),
        Some("docker")
    );
    assert_eq!(
        detect_container(
            false,
            Some("0::/kubepods.slice/kubepods-burstable.slice/pod123"),
            None
        ),
        Some("kubernetes")
    );
    assert_eq!(
        detect_container(
            false,
            Some("1:name=systemd:/system.slice/containerd.service"),
            None
        ),
        Some("containerd")
    );
    assert_eq!(
        detect_container(false, Some("0::/docker/abcdef123456"), None),
        Some("docker")
    );
    assert_eq!(
        detect_container(false, Some("0::/user.slice/user-1000.slice"), None),
        None
    );
    assert_eq!(detect_container(false, None, Some("   ")), None);
}

#[test]
fn linux_sandbox_status_prefers_bubblewrap_then_container_then_none() {
    let bwrap = PathBuf::from("/usr/bin/bwrap");
    assert_eq!(
        linux_sandbox_status(Some(bwrap.clone()), true, Some("docker"), Some("podman")),
        LinuxIsolation::Bubblewrap { bwrap_bin: bwrap }
    );
    let container = linux_sandbox_status(None, false, Some("0::/kubepods/p1"), None);
    assert_eq!(
        container,
        LinuxIsolation::HardenedContainer {
            indicator: "kubernetes"
        }
    );
    assert!(container.is_isolated());
    assert!(container.summary().contains("kubernetes"));

    let none = linux_sandbox_status(None, false, None, None);
    assert_eq!(none, LinuxIsolation::None);
    assert!(!none.is_isolated());
    assert!(none.summary().contains("bubblewrap"));
}

#[test]
fn build_bwrap_args_enforces_least_privilege_mounts_and_hides_data_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    let worktree = root.join("worktree");
    let git_dir = root.join("repo.git");
    let data_dir = root.join("aaf-data");
    let cargo_cache = root.join("home/.cargo");
    let missing_cache = root.join("home/.nonexistent-cache");
    let sibling_worktree = root.join("worktrees/other-task");
    let worktrees_root = root.join("worktrees");

    fs::create_dir_all(worktree.join(".claude")).unwrap();
    fs::write(worktree.join(".claude/settings.json"), "{}").unwrap();
    fs::write(worktree.join(".git"), "gitdir: ../repo.git/worktrees/wt1\n").unwrap();

    fs::create_dir_all(git_dir.join("hooks")).unwrap();
    fs::write(git_dir.join("config"), "[core]\n").unwrap();
    fs::create_dir_all(git_dir.join("refs/aaf")).unwrap();

    fs::create_dir_all(data_dir.join("throttle/guest")).unwrap();
    fs::write(data_dir.join("receipt.key"), "super-secret-signing-key").unwrap();

    fs::create_dir_all(cargo_cache.join("bin")).unwrap();
    fs::write(cargo_cache.join("config.toml"), "").unwrap();
    fs::create_dir_all(&sibling_worktree).unwrap();

    let wt_str = worktree.to_str().unwrap().to_string();
    let git_str = git_dir.to_str().unwrap().to_string();
    let data_str = data_dir.to_str().unwrap().to_string();
    let cargo_str = cargo_cache.to_str().unwrap().to_string();
    let missing_str = missing_cache.to_str().unwrap().to_string();
    let sibling_str = sibling_worktree.to_str().unwrap().to_string();
    let wt_root_str = worktrees_root.to_str().unwrap().to_string();
    let cargo_bin_str = cargo_cache.join("bin").to_str().unwrap().to_string();
    let cargo_cfg_prefix = cargo_cache.join("config").to_str().unwrap().to_string();

    let paths = SandboxPaths {
        writable: vec![
            "/tmp".to_string(),
            cargo_str.clone(),
            missing_str.clone(),
            sibling_str.clone(),
        ],
        writable_regex: vec![],
        write_denied: vec![cargo_bin_str.clone()],
        write_denied_prefix: vec![cargo_cfg_prefix],
        read_denied: vec![data_str.clone()],
        read_reopened: vec![],
        write_reopened: vec![],
        worktree: Some(wt_str.clone()),
        git_dirs: vec![git_str.clone()],
        worktrees_root: Some(wt_root_str),
        extra_worktrees_roots: vec![],
        path_dirs: vec![],
        run_mark: None,
        deny_keychain_dir: None,
    };

    let argv = build_bwrap_args(
        &paths,
        "/usr/bin/claude",
        &["-p".to_string(), "hello".to_string()],
    )
    .unwrap();

    // Base namespace and rootfs hardening.
    assert!(argv.contains(&"--die-with-parent".to_string()));
    assert!(argv.contains(&"--unshare-pid".to_string()));
    assert!(argv.contains(&"--unshare-ipc".to_string()));
    assert!(argv.contains(&"--new-session".to_string()));
    assert!(find_flag_pair(&argv, "--ro-bind", "/", "/").is_some());
    assert!(find_flag_single(&argv, "--dev", "/dev").is_some());
    assert!(find_flag_single(&argv, "--proc", "/proc").is_some());
    assert!(find_flag_single(&argv, "--tmpfs", "/tmp").is_some());

    // `/tmp` must stay private tmpfs (never host-bound).
    assert!(find_flag_pair(&argv, "--bind", "/tmp", "/tmp").is_none());

    // Under Plan v3 (V2-SEC-06), `.cargo` root stays RO (never bound RW) so agents
    // cannot create `.cargo/config.toml` or `bin/*`; missing cache and sibling are skipped.
    assert!(find_flag_pair(&argv, "--bind", &cargo_str, &cargo_str).is_none());
    assert!(find_flag_pair(&argv, "--bind", &missing_str, &missing_str).is_none());
    assert!(find_flag_pair(&argv, "--bind", &sibling_str, &sibling_str).is_none());

    // Worktree and git common dir are writable.
    let wt_bind = find_flag_pair(&argv, "--bind", &wt_str, &wt_str).unwrap();
    let git_bind = find_flag_pair(&argv, "--bind", &git_str, &git_str).unwrap();

    // Sensitive git & claude & cargo entries are re-mounted read-only AFTER writable binds.
    let hooks_ro = find_flag_pair(
        &argv,
        "--ro-bind",
        git_dir.join("hooks").to_str().unwrap(),
        git_dir.join("hooks").to_str().unwrap(),
    )
    .unwrap();
    let config_ro = find_flag_pair(
        &argv,
        "--ro-bind",
        git_dir.join("config").to_str().unwrap(),
        git_dir.join("config").to_str().unwrap(),
    )
    .unwrap();
    let refs_aaf_ro = find_flag_pair(
        &argv,
        "--ro-bind",
        git_dir.join("refs/aaf").to_str().unwrap(),
        git_dir.join("refs/aaf").to_str().unwrap(),
    )
    .unwrap();
    let settings_ro = find_flag_pair(
        &argv,
        "--ro-bind",
        worktree.join(".claude/settings.json").to_str().unwrap(),
        worktree.join(".claude/settings.json").to_str().unwrap(),
    )
    .unwrap();
    let dot_git_ro = find_flag_pair(
        &argv,
        "--ro-bind",
        worktree.join(".git").to_str().unwrap(),
        worktree.join(".git").to_str().unwrap(),
    )
    .unwrap();
    let cargo_bin_ro = find_flag_pair(&argv, "--ro-bind", &cargo_bin_str, &cargo_bin_str).unwrap();
    let cargo_toml_ro = find_flag_pair(
        &argv,
        "--ro-bind",
        cargo_cache.join("config.toml").to_str().unwrap(),
        cargo_cache.join("config.toml").to_str().unwrap(),
    )
    .unwrap();

    assert!(hooks_ro > git_bind);
    assert!(config_ro > git_bind);
    assert!(refs_aaf_ro > git_bind);
    assert!(settings_ro > wt_bind);
    assert!(dot_git_ro > wt_bind);
    assert!(cargo_bin_ro > 0);
    assert!(cargo_toml_ro > 0);

    // Critical: `AAF_DATA_DIR` is hidden behind `--tmpfs`, masking `receipt.key`,
    // and only `throttle` (ro) + `throttle/guest` (rw) are re-bound AFTER it.
    let data_tmpfs = find_flag_single(&argv, "--tmpfs", &data_str).unwrap();
    let throttle_ro = find_flag_pair(
        &argv,
        "--ro-bind",
        data_dir.join("throttle").to_str().unwrap(),
        data_dir.join("throttle").to_str().unwrap(),
    )
    .unwrap();
    let guest_rw = find_flag_pair(
        &argv,
        "--bind",
        data_dir.join("throttle/guest").to_str().unwrap(),
        data_dir.join("throttle/guest").to_str().unwrap(),
    )
    .unwrap();
    assert!(throttle_ro > data_tmpfs);
    assert!(guest_rw > data_tmpfs);

    // Working directory and final `-- <program> <args...>`.
    assert!(find_flag_single(&argv, "--chdir", &wt_str).is_some());
    let tail = &argv[argv.len() - 4..];
    assert_eq!(tail, &["--", "/usr/bin/claude", "-p", "hello"]);
}

#[test]
fn build_bwrap_args_rejects_nul_empty_and_relative_paths() {
    let valid = SandboxPaths {
        worktree: Some("/workspace/repo".to_string()),
        ..SandboxPaths::default()
    };

    assert!(build_bwrap_args(&valid, "", &[]).is_err());
    assert!(build_bwrap_args(&valid, "/bin/sh\0evil", &[]).is_err());
    assert!(
        build_bwrap_args(
            &valid,
            "/bin/sh",
            &["ok".to_string(), "bad\0arg".to_string()]
        )
        .is_err()
    );

    let nul_wt = SandboxPaths {
        worktree: Some("/workspace/\0repo".to_string()),
        ..SandboxPaths::default()
    };
    assert!(build_bwrap_args(&nul_wt, "/bin/sh", &[]).is_err());

    let rel_wt = SandboxPaths {
        worktree: Some("relative/worktree".to_string()),
        ..SandboxPaths::default()
    };
    assert!(build_bwrap_args(&rel_wt, "/bin/sh", &[]).is_err());

    let nul_denied = SandboxPaths {
        read_denied: vec!["/var/aaf\0data".to_string()],
        ..SandboxPaths::default()
    };
    assert!(build_bwrap_args(&nul_denied, "/bin/sh", &[]).is_err());

    let empty_writable = SandboxPaths {
        writable: vec![String::new()],
        ..SandboxPaths::default()
    };
    assert!(build_bwrap_args(&empty_writable, "/bin/sh", &[]).is_err());
}

#[test]
fn wrap_rewrites_command_spec_to_bwrap() {
    let paths = SandboxPaths {
        worktree: Some("/workspace/app".to_string()),
        read_denied: vec!["/home/aaf/.local/share/autofactory".to_string()],
        ..SandboxPaths::default()
    };
    let spec = CommandSpec {
        program: "/usr/bin/env".to_string(),
        args: vec!["claude".to_string()],
        cwd: Some("/workspace/app".to_string()),
        env: vec![("AAF_RUN_ID".to_string(), "run-1".to_string())],
        env_clear: true,
    };
    let wrapped = wrap(Path::new("/usr/bin/bwrap"), &paths, &spec).unwrap();
    assert_eq!(wrapped.program, "/usr/bin/bwrap");
    assert_eq!(wrapped.env, spec.env);
    assert!(wrapped.env_clear);
    assert_eq!(
        &wrapped.args[wrapped.args.len() - 3..],
        &["--", "/usr/bin/env", "claude"]
    );
}
