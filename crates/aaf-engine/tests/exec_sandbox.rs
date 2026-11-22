#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Seatbelt profile tests. The macOS ones run real commands under
//! `/usr/bin/sandbox-exec`.

use aaf_domain::ports::{PortError, SandboxPolicy};
use aaf_engine::executors::pty::CommandSpec;
use aaf_engine::executors::sandbox::{SandboxPaths, canonical, seatbelt_profile, wrap};

fn policy(writable: &[&str], denied: &[&str]) -> SandboxPolicy {
    SandboxPolicy {
        enabled: true,
        read_only: false,
        writable_paths: writable.iter().map(|s| (*s).to_string()).collect(),
        read_denied_paths: denied.iter().map(|s| (*s).to_string()).collect(),
    }
}

#[test]
fn profile_denies_writes_by_default_and_orders_rules() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let (work, secret) = (root.join("work"), root.join("work/secret"));
    std::fs::create_dir_all(&secret).unwrap();
    let profile = seatbelt_profile(
        &policy(&[work.to_str().unwrap()], &[secret.to_str().unwrap()]),
        &SandboxPaths::default(),
    )
    .unwrap();

    let allow_default = profile.find("(allow default)").unwrap();
    let deny_writes = profile.find("(deny file-write*)").unwrap();
    let allow_work = profile
        .find(&format!(
            "(allow file-write* (subpath \"{}\"))",
            work.display()
        ))
        .unwrap();
    let deny_secret = profile
        .find(&format!(
            "(deny file-read* file-write* (subpath \"{}\"))",
            secret.display()
        ))
        .unwrap();
    // SBPL applies the last matching rule.
    assert!(allow_default < deny_writes && deny_writes < allow_work && allow_work < deny_secret);
}

#[test]
fn extra_paths_are_merged_and_duplicates_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap();
    let path = path.to_str().unwrap();
    let extra = SandboxPaths {
        writable: vec![path.to_string()],
        ..SandboxPaths::default()
    };
    let profile = seatbelt_profile(&policy(&[path], &[]), &extra).unwrap();
    assert_eq!(profile.matches(path).count(), 1, "{profile}");
}

#[test]
fn paths_are_canonicalised_through_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("real")).unwrap();
    std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();

    let via_link = root.join("link/not-yet/there");
    let resolved = canonical(via_link.to_str().unwrap()).unwrap();
    assert_eq!(resolved, root.join("real/not-yet/there"));

    let profile = seatbelt_profile(
        &policy(&[], &[via_link.to_str().unwrap()]),
        &SandboxPaths::default(),
    )
    .unwrap();
    assert!(profile.contains(root.join("real/not-yet/there").to_str().unwrap()));
    assert!(!profile.contains("/link/"));
}

#[cfg(target_os = "macos")]
#[test]
fn macos_tmp_resolves_into_private() {
    assert_eq!(
        canonical("/tmp/aaf-does-not-exist/x").unwrap(),
        std::path::PathBuf::from("/private/tmp/aaf-does-not-exist/x")
    );
    let profile = seatbelt_profile(
        &policy(&["/tmp"], &["/var/aaf-x"]),
        &SandboxPaths::default(),
    )
    .unwrap();
    assert!(profile.contains("(subpath \"/private/tmp\")"), "{profile}");
    assert!(profile.contains("(subpath \"/private/var/aaf-x\")"));
}

#[test]
fn unsafe_or_unresolvable_paths_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().to_str().unwrap().to_string();
    let bad = [
        // Would close the string and inject a rule.
        format!("{base}/a\")) (allow file-write* (subpath \"/"),
        format!("{base}/a\nb"),
        format!("{base}/a\u{7}b"),
        "relative/path".to_string(),
        String::new(),
        // `..` below a component that does not exist cannot be resolved.
        format!("{base}/missing/../../x"),
    ];
    for path in &bad {
        for (writable, denied) in [(vec![path.as_str()], vec![]), (vec![], vec![path.as_str()])] {
            let result = seatbelt_profile(&policy(&writable, &denied), &SandboxPaths::default());
            assert!(
                matches!(result, Err(PortError::Invalid(_))),
                "{path:?} gave {result:?}"
            );
        }
    }
}

#[test]
fn backslash_is_escaped_not_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let odd = root.join("a\\b (c) ; #x");
    let profile = seatbelt_profile(
        &policy(&[odd.to_str().unwrap()], &[]),
        &SandboxPaths::default(),
    )
    .unwrap();
    assert!(profile.contains("a\\\\b (c) ; #x\"))"), "{profile}");
}

#[test]
fn wrap_keeps_the_command_as_separate_arguments() {
    let command = CommandSpec {
        program: "/bin/echo".to_string(),
        args: vec!["a b".to_string(), "-p".to_string()],
        cwd: Some("/".to_string()),
        env: vec![("K".to_string(), "V".to_string())],
        env_clear: true,
    };
    let wrapped = wrap(&command, "(version 1)(allow default)");
    if cfg!(target_os = "macos") {
        let wrapped = wrapped.unwrap();
        assert_eq!(wrapped.program, "/usr/bin/sandbox-exec");
        assert_eq!(
            wrapped.args,
            [
                "-p",
                "(version 1)(allow default)",
                "--",
                "/bin/echo",
                "a b",
                "-p"
            ]
        );
        assert!(wrapped.env_clear, "the cleared environment is kept");
        assert_eq!((wrapped.cwd, wrapped.env), (command.cwd, command.env));
    } else {
        assert!(matches!(wrapped, Err(PortError::Unsupported(_))));
    }
}

#[cfg(target_os = "macos")]
mod real {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use aaf_domain::ports::{AgentRequest, AgentRunner, SandboxPolicy};
    use aaf_engine::executors::ExecEnv;
    use aaf_engine::executors::generic::GenericRunner;
    use aaf_engine::executors::pty::{CommandSpec, PtyOutput, run_pty};
    use aaf_engine::executors::sandbox::{SandboxPaths, seatbelt_profile, wrap};

    struct Dirs {
        _keep: tempfile::TempDir,
        allowed: PathBuf,
        outside: PathBuf,
        denied: PathBuf,
    }

    fn dirs() -> Dirs {
        let keep = tempfile::tempdir().unwrap();
        let root = keep.path().to_path_buf();
        let dirs = Dirs {
            allowed: root.join("allowed"),
            outside: root.join("outside"),
            denied: root.join("denied"),
            _keep: keep,
        };
        for dir in [&dirs.allowed, &dirs.outside, &dirs.denied] {
            std::fs::create_dir(dir).unwrap();
        }
        std::fs::write(dirs.denied.join("secret"), "TOP-SECRET").unwrap();
        std::fs::write(dirs.outside.join("readme"), "PUBLIC-TEXT").unwrap();
        dirs
    }

    /// Runs `sh -c script` confined to `dirs` (paths given uncanonicalised, as
    /// the tempdir reports them under /var/folders).
    fn sandboxed(dirs: &Dirs, script: &str) -> PtyOutput {
        let policy = SandboxPolicy {
            enabled: true,
            read_only: false,
            writable_paths: vec![dirs.allowed.to_string_lossy().into_owned()],
            read_denied_paths: vec![dirs.denied.to_string_lossy().into_owned()],
        };
        let profile = seatbelt_profile(&policy, &SandboxPaths::default()).unwrap();
        let command = CommandSpec {
            program: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            ..CommandSpec::default()
        };
        let wrapped = wrap(&command, &profile).unwrap();
        let out = run_pty(&wrapped, Duration::from_secs(60), 16 * 1024, None).unwrap();
        // A failure must be the sandbox saying no, not a command stuck at a prompt.
        assert!(!out.timed_out, "{}", out.tail);
        out
    }

    fn q(path: &Path) -> String {
        format!("'{}'", path.display())
    }

    #[test]
    fn write_inside_an_allowed_dir_succeeds() {
        let dirs = dirs();
        let target = dirs.allowed.join("out.txt");
        let out = sandboxed(&dirs, &format!("echo written > {}", q(&target)));
        assert_eq!(out.exit_code, Some(0), "{}", out.tail);
        assert_eq!(std::fs::read_to_string(target).unwrap(), "written\n");
    }

    #[test]
    fn write_outside_fails() {
        let dirs = dirs();
        let target = dirs.outside.join("out.txt");
        let out = sandboxed(&dirs, &format!("echo written > {}", q(&target)));
        assert_ne!(out.exit_code, Some(0));
        assert!(out.tail.contains("Operation not permitted"), "{}", out.tail);
        assert!(!target.exists());
        // Existing files outside cannot be changed or removed either.
        let readme = dirs.outside.join("readme");
        let out = sandboxed(&dirs, &format!("rm -f {0} || echo x >> {0}", q(&readme)));
        assert_ne!(out.exit_code, Some(0));
        assert_eq!(std::fs::read_to_string(readme).unwrap(), "PUBLIC-TEXT");
    }

    #[test]
    fn reading_a_denied_file_fails() {
        let dirs = dirs();
        let out = sandboxed(&dirs, &format!("cat {}", q(&dirs.denied.join("secret"))));
        assert_ne!(out.exit_code, Some(0));
        assert!(!out.tail.contains("TOP-SECRET"), "{}", out.tail);
        let out = sandboxed(&dirs, &format!("ls {}", q(&dirs.denied)));
        assert_ne!(out.exit_code, Some(0));
        assert!(!out.tail.contains("secret"), "{}", out.tail);
    }

    #[test]
    fn reading_elsewhere_works() {
        let dirs = dirs();
        let out = sandboxed(&dirs, &format!("cat {}", q(&dirs.outside.join("readme"))));
        assert_eq!(out.exit_code, Some(0), "{}", out.tail);
        assert!(out.tail.contains("PUBLIC-TEXT"));
    }

    #[test]
    fn dev_null_and_the_terminal_stay_writable() {
        let dirs = dirs();
        let out = sandboxed(&dirs, "echo quiet > /dev/null && echo to-tty > /dev/tty");
        assert_eq!(out.exit_code, Some(0), "{}", out.tail);
        assert!(out.tail.contains("to-tty"));
    }

    #[test]
    fn denied_dir_inside_a_writable_dir_stays_closed() {
        let dirs = dirs();
        let inner = dirs.allowed.join("vault");
        std::fs::create_dir(&inner).unwrap();
        let policy = SandboxPolicy {
            enabled: true,
            read_only: false,
            writable_paths: vec![dirs.allowed.to_string_lossy().into_owned()],
            read_denied_paths: vec![inner.to_string_lossy().into_owned()],
        };
        let profile = seatbelt_profile(&policy, &SandboxPaths::default()).unwrap();
        let command = CommandSpec {
            program: "/bin/sh".to_string(),
            args: vec![
                "-c".to_string(),
                format!("echo x > {}", q(&inner.join("f"))),
            ],
            ..CommandSpec::default()
        };
        let out = run_pty(
            &wrap(&command, &profile).unwrap(),
            Duration::from_secs(60),
            4096,
            None,
        )
        .unwrap();
        assert_ne!(out.exit_code, Some(0));
        assert!(!inner.join("f").exists());
    }

    /// The whole runner path: the worktree (cwd) is writable without being
    /// listed, everything else follows the policy.
    #[test]
    fn runner_confines_an_agent_to_its_worktree() {
        let dirs = dirs();
        let script = format!(
            "echo in > in.txt && echo wrote-in; echo out > {} || echo denied-out; cat {} || echo denied-read; cat {}",
            q(&dirs.outside.join("out.txt")),
            q(&dirs.denied.join("secret")),
            q(&dirs.outside.join("readme")),
        );
        let template = ["/bin/sh", "-c", &script, "sh", "{prompt}"].map(String::from);
        // No home and no temp dirs: the test dirs live under TMPDIR, which a
        // real environment would make writable.
        let runner = GenericRunner::new(template.to_vec(), ExecEnv::default()).unwrap();
        let outcome = runner
            .run(&AgentRequest {
                run_id: format!("test-{}", uuid::Uuid::new_v4()),
                prompt: "unused".to_string(),
                cwd: dirs.allowed.to_string_lossy().into_owned(),
                account: None,
                timeout_secs: 60,
                sandbox: SandboxPolicy {
                    enabled: true,
                    read_only: false,
                    writable_paths: Vec::new(),
                    read_denied_paths: vec![dirs.denied.to_string_lossy().into_owned()],
                },
            })
            .unwrap();
        let log = &outcome.log_tail;
        assert!(log.contains("wrote-in"), "{log}");
        assert!(log.contains("denied-out"), "{log}");
        assert!(log.contains("denied-read"), "{log}");
        assert!(log.contains("PUBLIC-TEXT"), "{log}");
        assert!(!log.contains("TOP-SECRET"), "{log}");
        assert!(dirs.allowed.join("in.txt").exists());
        assert!(!dirs.outside.join("out.txt").exists());
    }
}
