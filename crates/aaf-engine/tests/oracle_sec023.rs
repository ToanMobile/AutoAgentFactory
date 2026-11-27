#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Oracle verification for SEC-023:
//! A worker running inside a linked worktree under macOS Seatbelt must not be
//! able to overwrite the primary checkout's per-worktree git state files
//! (`<main>/.git/{HEAD,index,index.lock,MERGE_HEAD,ORIG_HEAD}`), while still
//! being able to `git add` + `git commit` inside its own linked worktree, and
//! a primary (non-linked) checkout must still be able to write its own
//! `.git/index` and commit normally.

#[cfg(target_os = "macos")]
mod real {
    use std::fs;
    use std::path::{Path, PathBuf};

    use aaf_domain::ports::{AgentOutcome, AgentRequest, AgentRunner, SandboxPolicy};
    use aaf_engine::executors::ExecEnv;
    use aaf_engine::executors::generic::GenericRunner;

    /// Identity and hook settings fixed so the host's global git config
    /// cannot affect test runs.
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

    fn require_seatbelt() {
        assert!(
            Path::new("/usr/bin/sandbox-exec").exists(),
            "/usr/bin/sandbox-exec is required on macOS for SEC-023 oracle"
        );
    }

    fn git(repo: &Path, args: &[&str]) -> String {
        let mut full = GIT_FLAGS.to_vec();
        full.extend_from_slice(args);
        aaf_fp::git(repo, &full, &[], aaf_fp::GIT_TIMEOUT).unwrap()
    }

    fn init_primary_repo(root: &Path) -> PathBuf {
        let main = root.join("main");
        fs::create_dir(&main).unwrap();
        git(&main, &["init", "-q"]);
        fs::write(main.join("a.txt"), "initial\n").unwrap();
        git(&main, &["add", "a.txt"]);
        git(&main, &["commit", "-q", "-m", "first"]);
        main
    }

    fn init_repo_with_linked_worktree(root: &Path) -> (PathBuf, PathBuf) {
        let main = init_primary_repo(root);
        let worktree = root.join("wt");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "task-sec023",
                worktree.to_str().unwrap(),
            ],
        );
        (main, worktree)
    }

    fn run_sandboxed(cwd: &Path, script: &str) -> AgentOutcome {
        require_seatbelt();
        let template = ["/bin/sh", "-c", script, "sh", "{prompt}"].map(String::from);
        let outcome = GenericRunner::new(template.to_vec(), ExecEnv::default())
            .unwrap()
            .run(&AgentRequest {
                run_id: format!("sec023-{}", uuid::Uuid::new_v4()),
                prompt: "unused".to_string(),
                cwd: cwd.to_string_lossy().into_owned(),
                account: None,
                timeout_secs: 60,
                sandbox: SandboxPolicy {
                    enabled: true,
                    ..SandboxPolicy::default()
                },
                json_schema: None,
                max_turns: None,
            })
            .unwrap();
        assert!(
            !outcome.timed_out,
            "sandboxed run timed out: {}",
            outcome.log_tail
        );
        outcome
    }

    /// (a) Inside a linked worktree under Seatbelt, writes to the primary
    /// checkout's `.git/{HEAD,index,index.lock,MERGE_HEAD,ORIG_HEAD}` must be
    /// denied and leave the primary checkout's files untouched.
    #[test]
    fn linked_worktree_cannot_write_main_checkout_git_state() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (main, worktree) = init_repo_with_linked_worktree(&root);
        let main_git = main.join(".git");

        let head_before = fs::read(main_git.join("HEAD")).unwrap();
        let index_before = fs::read(main_git.join("index")).unwrap();
        for absent in ["index.lock", "MERGE_HEAD", "ORIG_HEAD"] {
            assert!(
                !main_git.join(absent).exists(),
                "precondition failed: {absent} should not exist yet"
            );
        }

        let g = main_git.display();
        let script = format!(
            "attempt() {{ \
               target=\"$1\"; tag=\"$2\"; \
               if (printf 'evil-sec023\\n' > \"$target\") 2>/dev/null; then \
                 echo \"${{tag}}-WRITABLE\"; \
               else \
                 echo \"${{tag}}-denied\"; \
               fi; \
             }}; \
             attempt '{g}/HEAD' head; \
             attempt '{g}/index' index; \
             attempt '{g}/index.lock' index_lock; \
             attempt '{g}/MERGE_HEAD' merge_head; \
             attempt '{g}/ORIG_HEAD' orig_head"
        );
        let outcome = run_sandboxed(&worktree, &script);
        let log = &outcome.log_tail;

        for tag in ["head", "index", "index_lock", "merge_head", "orig_head"] {
            assert!(
                log.contains(&format!("{tag}-denied")) && !log.contains(&format!("{tag}-WRITABLE")),
                "expected Seatbelt to deny writing main .git state for `{tag}`, got log:\n{log}"
            );
        }

        assert_eq!(
            fs::read(main_git.join("HEAD")).unwrap(),
            head_before,
            "main .git/HEAD was modified from linked worktree"
        );
        assert_eq!(
            fs::read(main_git.join("index")).unwrap(),
            index_before,
            "main .git/index was modified from linked worktree"
        );
        for absent in ["index.lock", "MERGE_HEAD", "ORIG_HEAD"] {
            assert!(
                !main_git.join(absent).exists(),
                "main .git/{absent} was created from linked worktree"
            );
        }
    }

    /// (b) Inside the same linked worktree under Seatbelt, normal `git add`
    /// and `git commit` must still succeed without touching main checkout state.
    #[test]
    fn linked_worktree_git_add_and_commit_succeed_under_seatbelt() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (main, worktree) = init_repo_with_linked_worktree(&root);
        let main_git = main.join(".git");
        let head_before = fs::read(main_git.join("HEAD")).unwrap();
        let index_before = fs::read(main_git.join("index")).unwrap();

        let flags = GIT_FLAGS.join(" ");
        let script = format!(
            "echo worktree-change > b.txt && \
             git add b.txt && \
             git {flags} commit -q -m 'linked-worktree-commit' && \
             echo commit-ok"
        );
        let outcome = run_sandboxed(&worktree, &script);
        assert_eq!(
            outcome.exit_code,
            Some(0),
            "git commit in linked worktree failed: {}",
            outcome.log_tail
        );
        assert!(
            outcome.log_tail.contains("commit-ok"),
            "missing commit-ok marker: {}",
            outcome.log_tail
        );
        assert_eq!(
            git(&worktree, &["log", "-1", "--format=%s"]),
            "linked-worktree-commit"
        );
        assert_eq!(fs::read(main_git.join("HEAD")).unwrap(), head_before);
        assert_eq!(fs::read(main_git.join("index")).unwrap(), index_before);
    }

    /// (c) In a primary (non-linked) checkout under Seatbelt, `git add` /
    /// writing `.git/index` and committing must continue to succeed.
    #[test]
    fn primary_checkout_can_write_git_index_and_commit_under_seatbelt() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let repo = init_primary_repo(&root);
        let index_before = fs::read(repo.join(".git/index")).unwrap();

        let flags = GIT_FLAGS.join(" ");
        let script = format!(
            "echo primary-change > b.txt && \
             git add b.txt && \
             git {flags} commit -q -m 'primary-commit' && \
             echo staged-only > c.txt && \
             git add c.txt && \
             echo primary-ok"
        );
        let outcome = run_sandboxed(&repo, &script);
        assert_eq!(
            outcome.exit_code,
            Some(0),
            "git add/commit in primary checkout failed: {}",
            outcome.log_tail
        );
        assert!(
            outcome.log_tail.contains("primary-ok"),
            "missing primary-ok marker: {}",
            outcome.log_tail
        );
        assert_eq!(git(&repo, &["log", "-1", "--format=%s"]), "primary-commit");
        assert_ne!(
            fs::read(repo.join(".git/index")).unwrap(),
            index_before,
            "primary checkout .git/index should have been updated by git add"
        );
        assert_eq!(git(&repo, &["status", "--porcelain"]), "A  c.txt");
    }
}
