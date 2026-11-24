#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Sandbox isolation between task worktrees, and protection of the parts of
//! a git directory that git executes later outside the sandbox.

use aaf_domain::ports::SandboxPolicy;
use aaf_engine::executors::sandbox::{SandboxPaths, seatbelt_profile};

#[test]
fn listed_writable_paths_under_a_worktrees_dir_are_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let sibling = root.join("autofactory/worktrees/repo/other-task");
    let own = root.join("autofactory/worktrees/repo/my-task");
    let policy = SandboxPolicy {
        enabled: true,
        read_only: false,
        writable_paths: vec![sibling.to_string_lossy().into_owned()],
        read_denied_paths: Vec::new(),
    };
    let extra = SandboxPaths {
        writable: vec![sibling.to_string_lossy().into_owned()],
        worktree: Some(own.to_string_lossy().into_owned()),
        worktrees_root: Some(
            root.join("autofactory/worktrees")
                .to_string_lossy()
                .into_owned(),
        ),
        ..SandboxPaths::default()
    };
    let profile = seatbelt_profile(&policy, &extra).unwrap();
    assert!(!profile.contains("other-task"), "{profile}");
    let close_all = profile
        .find(&format!(
            "(deny file-write* (subpath \"{}\"))",
            root.join("autofactory/worktrees").display()
        ))
        .unwrap();
    let open_own = profile
        .find(&format!(
            "(allow file-write* (subpath \"{}\"))",
            own.display()
        ))
        .unwrap();
    assert!(close_all < open_own);
}

/// The worktrees root is whatever was configured (`AAF_WORKTREE_DIR`), not a
/// path that contains `autofactory/worktrees`.
#[test]
fn custom_worktrees_root_closes_siblings_and_reopens_only_the_own_worktree() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let wt_root = root.join("my-wt");
    let sibling = wt_root.join("repo/other-task");
    let own = wt_root.join("repo/my-task");
    let policy = SandboxPolicy {
        enabled: true,
        read_only: false,
        writable_paths: vec![
            sibling.to_string_lossy().into_owned(),
            root.to_string_lossy().into_owned(),
        ],
        read_denied_paths: Vec::new(),
    };
    let extra = SandboxPaths {
        worktree: Some(own.to_string_lossy().into_owned()),
        worktrees_root: Some(wt_root.to_string_lossy().into_owned()),
        ..SandboxPaths::default()
    };
    let profile = seatbelt_profile(&policy, &extra).unwrap();
    assert!(!profile.contains("other-task"), "{profile}");
    let pos = |rule: String| {
        profile
            .find(&rule)
            .unwrap_or_else(|| panic!("{rule}\n{profile}"))
    };
    let open_ancestor = pos(format!(
        "(allow file-write* (subpath \"{}\"))",
        root.display()
    ));
    let close_all = pos(format!(
        "(deny file-write* (subpath \"{}\"))",
        wt_root.display()
    ));
    let open_own = pos(format!(
        "(allow file-write* (subpath \"{}\"))",
        own.display()
    ));
    assert!(
        open_ancestor < close_all && close_all < open_own,
        "{profile}"
    );

    // A run outside the worktrees root (the main checkout) still cannot write
    // into any task worktree, and its own allow does not re-open the root.
    let outside = SandboxPaths {
        worktree: Some(root.to_string_lossy().into_owned()),
        worktrees_root: Some(wt_root.to_string_lossy().into_owned()),
        ..SandboxPaths::default()
    };
    let profile = seatbelt_profile(&SandboxPolicy::default(), &outside).unwrap();
    let open_own = profile
        .rfind(&format!(
            "(allow file-write* (subpath \"{}\"))",
            root.display()
        ))
        .unwrap();
    let close_all = profile
        .find(&format!(
            "(deny file-write* (subpath \"{}\"))",
            wt_root.display()
        ))
        .unwrap();
    assert!(open_own < close_all, "{profile}");
}

#[test]
fn git_dir_guards_come_after_the_allow_and_escape_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let git_dir = dir.path().canonicalize().unwrap().join("re.po+x/.git");
    let extra = SandboxPaths {
        git_dirs: vec![git_dir.to_string_lossy().into_owned()],
        ..SandboxPaths::default()
    };
    let profile = seatbelt_profile(&SandboxPolicy::default(), &extra).unwrap();
    let allow = profile
        .find(&format!(
            "(allow file-write* (subpath \"{}\"))",
            git_dir.display()
        ))
        .unwrap();
    let deny = profile
        .find(&format!(
            "(deny file-write* (subpath \"{}/hooks\")",
            git_dir.display()
        ))
        .unwrap();
    assert!(allow < deny);
    assert!(profile.contains(&format!("(literal \"{}/config\")", git_dir.display())));
    // Regex metacharacters of the path are literal in the pattern.
    assert!(
        profile
            .contains(r#"re\.po\+x/\.git/worktrees/[^/]+/(hooks|config|config\.worktree)(/|$)")"#),
        "{profile}"
    );
}

/// A normal checkout keeps `.git` inside the worktree: the worktree allow must
/// not come after the hook and config denies, or it re-opens them.
#[test]
fn git_dir_guards_come_after_the_worktree_allow_in_a_normal_checkout() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().canonicalize().unwrap().join("repo");
    let git_dir = repo.join(".git");
    let extra = SandboxPaths {
        worktree: Some(repo.to_string_lossy().into_owned()),
        git_dirs: vec![git_dir.to_string_lossy().into_owned()],
        ..SandboxPaths::default()
    };
    let profile = seatbelt_profile(&SandboxPolicy::default(), &extra).unwrap();
    let last_allow_covering_git = profile
        .rfind(&format!(
            "(allow file-write* (subpath \"{}\"))",
            repo.display()
        ))
        .unwrap()
        .max(
            profile
                .rfind(&format!(
                    "(allow file-write* (subpath \"{}\"))",
                    git_dir.display()
                ))
                .unwrap(),
        );
    let deny = profile
        .find(&format!(
            "(deny file-write* (subpath \"{}/hooks\") (literal \"{}/config\")",
            git_dir.display(),
            git_dir.display()
        ))
        .unwrap();
    assert!(last_allow_covering_git < deny, "{profile}");
    // No allow rule of any kind follows the guards.
    assert!(!profile[deny..].contains("(allow "), "{profile}");
}

#[cfg(target_os = "macos")]
mod real {
    use std::path::Path;

    use aaf_domain::ports::{AgentOutcome, AgentRequest, AgentRunner, SandboxPolicy};
    use aaf_engine::executors::ExecEnv;
    use aaf_engine::executors::generic::GenericRunner;

    fn run_sandboxed(cwd: &Path, env: ExecEnv, script: &str) -> AgentOutcome {
        let template = ["/bin/sh", "-c", script, "sh", "{prompt}"].map(String::from);
        let outcome = GenericRunner::new(template.to_vec(), env)
            .unwrap()
            .run(&AgentRequest {
                run_id: format!("test-{}", uuid::Uuid::new_v4()),
                prompt: "unused".to_string(),
                cwd: cwd.to_string_lossy().into_owned(),
                account: None,
                timeout_secs: 60,
                sandbox: SandboxPolicy {
                    enabled: true,
                    ..SandboxPolicy::default()
                },
                json_schema: None,
            })
            .unwrap();
        assert!(!outcome.timed_out, "{}", outcome.log_tail);
        outcome
    }

    /// Runs task-a under `worktrees` (a directory below the writable `root`)
    /// and checks that only its own worktree is writable there.
    fn assert_only_own_worktree_is_writable(root: &Path, worktrees: &Path, env: ExecEnv) {
        if !Path::new("/usr/bin/sandbox-exec").exists() {
            return;
        }
        let mine = worktrees.join("repo1/task-a");
        let sibling = worktrees.join("repo1/task-b");
        let other_repo = worktrees.join("repo2/task-c");
        for d in [&mine, &sibling, &other_repo] {
            std::fs::create_dir_all(d).unwrap();
        }
        // The whole tree sits under a writable temp dir, as it would with a
        // worktree directory below /tmp; the worktrees rule must still win.
        let env = ExecEnv {
            temp_dirs: vec![root.to_string_lossy().into_owned()],
            ..env
        };
        let script = format!(
            "echo a > own.txt && echo own-ok; \
             echo b > ../task-b/x.txt || echo sibling-denied; \
             echo c > ../../repo2/task-c/x.txt || echo other-repo-denied; \
             echo d > '{}/scratch.txt' && echo temp-ok",
            root.display()
        );
        let log = run_sandboxed(&mine, env, &script).log_tail;
        for marker in ["own-ok", "sibling-denied", "other-repo-denied", "temp-ok"] {
            assert!(log.contains(marker), "missing {marker}: {log}");
        }
        assert!(mine.join("own.txt").exists());
        assert!(!sibling.join("x.txt").exists());
        assert!(!other_repo.join("x.txt").exists());
        assert!(root.join("scratch.txt").exists());
    }

    #[test]
    fn sibling_task_worktree_is_not_writable_even_under_a_writable_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let worktrees = root.join("autofactory/worktrees");
        let env = ExecEnv {
            worktrees_root: Some(worktrees.to_string_lossy().into_owned()),
            ..ExecEnv::default()
        };
        assert_only_own_worktree_is_writable(&root, &worktrees, env);
    }

    /// `AAF_WORKTREE_DIR` may point anywhere; the rule follows the configured
    /// root, not a path that happens to contain `autofactory/worktrees`.
    #[test]
    fn custom_worktrees_root_blocks_sibling_task_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let worktrees = root.join("my-wt");
        let env = ExecEnv {
            worktrees_root: Some(worktrees.to_string_lossy().into_owned()),
            ..ExecEnv::default()
        };
        assert_only_own_worktree_is_writable(&root, &worktrees, env);
    }

    /// No root configured: the default below the home directory applies.
    #[test]
    fn default_worktrees_root_below_home_blocks_sibling_task_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        // Not named `autofactory/worktrees` by coincidence of the temp path:
        // the root is derived from `home`.
        let worktrees = root.join("home/.cache/autofactory/worktrees");
        let env = ExecEnv {
            home: Some(root.join("home").to_string_lossy().into_owned()),
            ..ExecEnv::default()
        };
        assert_only_own_worktree_is_writable(&root, &worktrees, env);
    }

    /// The signing key must stay unreadable even when the caller's policy
    /// lists no denied path at all.
    #[test]
    fn data_dir_is_unreadable_with_a_bare_enabled_policy() {
        if !Path::new("/usr/bin/sandbox-exec").exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (data, work) = (root.join("data"), root.join("work"));
        std::fs::create_dir_all(data.join("receipts")).unwrap();
        std::fs::create_dir(&work).unwrap();
        std::fs::write(data.join("receipt.key"), "KEY-BYTES").unwrap();
        std::fs::write(root.join("readme"), "PUBLIC-TEXT").unwrap();
        let env = ExecEnv {
            data_dir: Some(data.to_string_lossy().into_owned()),
            // A writable ancestor must not re-open it either.
            temp_dirs: vec![root.to_string_lossy().into_owned()],
            ..ExecEnv::default()
        };
        let d = data.display();
        let script = format!(
            "cat '{d}/receipt.key' || echo key-denied; \
             ls '{d}' || echo list-denied; \
             echo forged > '{d}/receipts/latest.json' || echo write-denied; \
             cat '{}/readme'",
            root.display()
        );
        // `run_sandboxed` passes `SandboxPolicy {{ enabled: true, ..default }}`.
        let log = run_sandboxed(&work, env, &script).log_tail;
        for marker in ["key-denied", "list-denied", "write-denied", "PUBLIC-TEXT"] {
            assert!(log.contains(marker), "missing {marker}: {log}");
        }
        assert!(!log.contains("KEY-BYTES"), "{log}");
        assert!(!data.join("receipts/latest.json").exists());
    }

    /// Identity and hook settings fixed so the user's global git config
    /// cannot change what the test does.
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

    /// A normal checkout (not a linked worktree): `.git` is inside the
    /// writable worktree, and its hooks and config must stay closed.
    #[test]
    fn normal_checkout_keeps_git_hooks_and_config_closed() {
        if !Path::new("/usr/bin/sandbox-exec").exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().canonicalize().unwrap().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        std::fs::write(repo.join("a.txt"), "a").unwrap();
        git(&repo, &["add", "a.txt"]);
        git(&repo, &["commit", "-q", "-m", "first"]);
        let git_dir = repo.join(".git");
        let config_before = std::fs::read_to_string(git_dir.join("config")).unwrap();

        let flags = GIT_FLAGS.join(" ");
        let script = format!(
            "echo new > b.txt && echo file-ok; \
             git add b.txt && git {flags} commit -q -m second && echo commit-ok; \
             echo evil > .git/hooks/pre-commit || echo hook-denied; \
             echo evil >> .git/config || echo config-denied; \
             git config core.hooksPath /tmp/evil || echo git-config-denied; \
             mv .git .git-old || echo rename-denied"
        );
        let log = run_sandboxed(&repo, ExecEnv::default(), &script).log_tail;
        for marker in [
            "file-ok",
            "commit-ok",
            "hook-denied",
            "config-denied",
            "git-config-denied",
            // Moving `.git` aside would let a fresh one carry its own hooks.
            "rename-denied",
        ] {
            assert!(log.contains(marker), "missing {marker}: {log}");
        }
        assert!(git_dir.is_dir() && !repo.join(".git-old").exists());
        assert_eq!(
            std::fs::read_to_string(repo.join("b.txt")).unwrap(),
            "new\n"
        );
        assert_eq!(git(&repo, &["log", "-1", "--format=%s"]), "second");
        assert!(!git_dir.join("hooks/pre-commit").exists());
        assert_eq!(
            std::fs::read_to_string(git_dir.join("config")).unwrap(),
            config_before
        );
    }

    /// The pointers that tell git where a linked worktree's git dir and
    /// common dir are: redirecting either would make later git commands run
    /// hooks from a directory the agent controls.
    #[test]
    fn linked_worktree_cannot_be_repointed_to_another_git_dir() {
        if !Path::new("/usr/bin/sandbox-exec").exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (main, worktree) = (root.join("main"), root.join("wt"));
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init", "-q"]);
        std::fs::write(main.join("a.txt"), "a").unwrap();
        git(&main, &["add", "a.txt"]);
        git(&main, &["commit", "-q", "-m", "first"]);
        let wt = worktree.to_str().unwrap();
        git(&main, &["worktree", "add", "-q", "-b", "task", wt]);
        let admin = main.join(".git/worktrees/wt");
        let before = ["commondir", "gitdir"].map(|f| std::fs::read(admin.join(f)).unwrap());
        let dot_git = std::fs::read(worktree.join(".git")).unwrap();

        let a = admin.display();
        let flags = GIT_FLAGS.join(" ");
        let script = format!(
            "echo new > b.txt && git add b.txt && git {flags} commit -q -m second && echo commit-ok; \
             echo 'gitdir: /tmp/evil' > .git || echo dotgit-denied; \
             rm -f .git; test -f .git && echo dotgit-kept; \
             echo /tmp/evil > '{a}/commondir' || echo commondir-denied; \
             echo /tmp/evil > '{a}/gitdir' || echo gitdir-denied"
        );
        let log = run_sandboxed(&worktree, ExecEnv::default(), &script).log_tail;
        for marker in [
            "commit-ok",
            "dotgit-denied",
            "dotgit-kept",
            "commondir-denied",
            "gitdir-denied",
        ] {
            assert!(log.contains(marker), "missing {marker}: {log}");
        }
        assert_eq!(std::fs::read(worktree.join(".git")).unwrap(), dot_git);
        let after = ["commondir", "gitdir"].map(|f| std::fs::read(admin.join(f)).unwrap());
        assert_eq!(after, before);
    }

    #[test]
    fn commits_work_but_git_hooks_and_config_are_not_writable() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (main, worktree) = (root.join("main"), root.join("wt"));
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init", "-q"]);
        std::fs::write(main.join("a.txt"), "a").unwrap();
        git(&main, &["add", "a.txt"]);
        git(&main, &["commit", "-q", "-m", "first"]);
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "task",
                worktree.to_str().unwrap(),
            ],
        );
        let git_dir = main.join(".git");
        let config_before = std::fs::read_to_string(git_dir.join("config")).unwrap();

        let g = git_dir.display();
        let flags = GIT_FLAGS.join(" ");
        let script = format!(
            "echo new > b.txt && git add b.txt && git {flags} commit -q -m second && echo commit-ok; \
             echo evil > '{g}/hooks/pre-commit' || echo hook-denied; \
             echo evil >> '{g}/config' || echo config-denied; \
             git config core.hooksPath /tmp/evil || echo git-config-denied; \
             echo evil > '{g}/worktrees/wt/config.worktree' || echo wt-config-denied; \
             mkdir '{g}/worktrees/wt/hooks' || echo wt-hooks-denied"
        );
        let log = run_sandboxed(&worktree, ExecEnv::default(), &script).log_tail;
        for marker in [
            "commit-ok",
            "hook-denied",
            "config-denied",
            "git-config-denied",
            "wt-config-denied",
            "wt-hooks-denied",
        ] {
            assert!(log.contains(marker), "missing {marker}: {log}");
        }
        assert_eq!(git(&worktree, &["log", "-1", "--format=%s"]), "second");
        assert!(!git_dir.join("hooks/pre-commit").exists());
        assert!(!git_dir.join("worktrees/wt/config.worktree").exists());
        assert!(!git_dir.join("worktrees/wt/hooks").exists());
        assert_eq!(
            std::fs::read_to_string(git_dir.join("config")).unwrap(),
            config_before
        );
    }
}
