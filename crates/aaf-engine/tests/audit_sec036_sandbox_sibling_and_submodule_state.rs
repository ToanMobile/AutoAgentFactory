#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Oracle verification for SEC-036:
//! A worker running inside linked worktree `wt_a` under macOS Seatbelt must
//! not be able to overwrite the per-worktree git state of a sibling linked
//! worktree (`<main>/.git/worktrees/wt_b/{HEAD,index,index.lock}`) nor the
//! checkout state of an existing submodule git dir
//! (`<main>/.git/modules/sub/{HEAD,index,index.lock}`), while `git add` +
//! `git commit` inside `wt_a` itself must keep working.

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

    const STATE_FILES: [&str; 2] = ["HEAD", "index"];

    fn require_seatbelt() {
        assert!(
            Path::new("/usr/bin/sandbox-exec").exists(),
            "/usr/bin/sandbox-exec is required on macOS for SEC-036 oracle"
        );
    }

    fn git(repo: &Path, args: &[&str]) -> String {
        let mut full = GIT_FLAGS.to_vec();
        full.extend_from_slice(args);
        aaf_fp::git(repo, &full, &[], aaf_fp::GIT_TIMEOUT).unwrap()
    }

    fn init_repo(dir: &Path, file: &str, content: &str) {
        fs::create_dir(dir).unwrap();
        git(dir, &["init", "-q"]);
        fs::write(dir.join(file), content).unwrap();
        git(dir, &["add", file]);
        git(dir, &["commit", "-q", "-m", "first"]);
    }

    fn add_worktree(main: &Path, root: &Path, name: &str) -> PathBuf {
        let wt = root.join(name);
        let branch = format!("task-sec036-{name}");
        git(
            main,
            &["worktree", "add", "-q", "-b", &branch, wt.to_str().unwrap()],
        );
        wt
    }

    /// Adds a real submodule `sub` to `main` (git dir at
    /// `<main>/.git/modules/sub`) and commits it.
    fn add_submodule(main: &Path, root: &Path) {
        let src = root.join("subsrc");
        init_repo(&src, "s.txt", "sub-initial\n");
        git(
            main,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                src.to_str().unwrap(),
                "sub",
            ],
        );
        git(main, &["commit", "-q", "-m", "add submodule"]);
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        main: PathBuf,
        wt_a: PathBuf,
    }

    fn fixture(with_wt_b: bool, with_sub: bool) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let main = root.join("main");
        init_repo(&main, "a.txt", "initial\n");
        if with_sub {
            add_submodule(&main, &root);
        }
        let wt_a = add_worktree(&main, &root, "wt_a");
        if with_wt_b {
            add_worktree(&main, &root, "wt_b");
        }
        Fixture {
            _dir: dir,
            main,
            wt_a,
        }
    }

    fn run_sandboxed(cwd: &Path, script: &str) -> AgentOutcome {
        require_seatbelt();
        let template = ["/bin/sh", "-c", script, "sh", "{prompt}"].map(String::from);
        let outcome = GenericRunner::new(template.to_vec(), ExecEnv::default())
            .unwrap()
            .run(&AgentRequest {
                run_id: format!("sec036-{}", uuid::Uuid::new_v4()),
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
        assert!(
            !outcome.timed_out,
            "sandboxed run timed out: {}",
            outcome.log_tail
        );
        outcome
    }

    fn snapshot(state_dir: &Path) -> Vec<Vec<u8>> {
        STATE_FILES
            .iter()
            .map(|name| fs::read(state_dir.join(name)).unwrap())
            .collect()
    }

    /// From `wt_a`, tries to overwrite `HEAD`, `index` and create
    /// `index.lock` in `state_dir`, then asserts every attempt was denied and
    /// nothing changed on disk.
    fn assert_state_dir_closed(wt_a: &Path, state_dir: &Path, what: &str) {
        assert!(
            state_dir.is_dir(),
            "precondition failed: {what} state dir {state_dir:?} missing"
        );
        let before = snapshot(state_dir);
        assert!(
            !state_dir.join("index.lock").exists(),
            "precondition failed: {what} index.lock should not exist yet"
        );

        let g = state_dir.display();
        let script = format!(
            "attempt() {{ \
               target=\"$1\"; tag=\"$2\"; \
               if (printf 'evil-sec036\\n' > \"$target\") 2>/dev/null; then \
                 echo \"${{tag}}-WRITABLE\"; \
               else \
                 echo \"${{tag}}-denied\"; \
               fi; \
             }}; \
             attempt '{g}/HEAD' head; \
             attempt '{g}/index' index; \
             attempt '{g}/index.lock' index_lock"
        );
        let outcome = run_sandboxed(wt_a, &script);
        let log = &outcome.log_tail;

        for tag in ["head", "index", "index_lock"] {
            assert!(
                log.contains(&format!("{tag}-denied")) && !log.contains(&format!("{tag}-WRITABLE")),
                "expected Seatbelt to deny writing {what} `{tag}` from wt_a, got log:\n{log}"
            );
        }
        for (name, before) in STATE_FILES.iter().zip(&before) {
            assert_eq!(
                &fs::read(state_dir.join(name)).unwrap(),
                before,
                "{what} {name} was modified from wt_a"
            );
        }
        assert!(
            !state_dir.join("index.lock").exists(),
            "{what} index.lock was created from wt_a"
        );
    }

    /// Case 1: sibling linked worktree `wt_b`'s per-worktree state under
    /// `<main>/.git/worktrees/wt_b/` must be closed to a worker in `wt_a`.
    #[test]
    fn case1_wt_a_cannot_write_sibling_wt_b_git_state() {
        let fx = fixture(true, false);
        let state = fx.main.join(".git/worktrees/wt_b");
        assert!(
            state.join("HEAD").is_file() && state.join("index").is_file(),
            "precondition failed: wt_b admin dir lacks HEAD/index"
        );
        assert_state_dir_closed(&fx.wt_a, &state, "sibling worktree wt_b");
    }

    /// Case 2: existing submodule git dir `<main>/.git/modules/sub` must keep
    /// its `HEAD`/`index` closed to a worker in linked worktree `wt_a`.
    #[test]
    fn case2_wt_a_cannot_write_submodule_git_state() {
        let fx = fixture(false, true);
        let state = fx.main.join(".git/modules/sub");
        assert!(
            state.join("HEAD").is_file() && state.join("index").is_file(),
            "precondition failed: submodule git dir lacks HEAD/index"
        );
        assert_state_dir_closed(&fx.wt_a, &state, "submodule sub");
    }

    /// Case 3 (control): in the same sandbox environment (sibling `wt_b` and
    /// submodule `sub` present), `git add` + `git commit` inside `wt_a` works
    /// and leaves `wt_b` / `sub` state untouched.
    #[test]
    fn case3_wt_a_git_add_and_commit_succeed_under_seatbelt() {
        let fx = fixture(true, true);
        let wt_b_state = fx.main.join(".git/worktrees/wt_b");
        let sub_state = fx.main.join(".git/modules/sub");
        let wt_b_before = snapshot(&wt_b_state);
        let sub_before = snapshot(&sub_state);

        let flags = GIT_FLAGS.join(" ");
        let script = format!(
            "echo wt-a-change > b.txt && \
             git add b.txt && \
             git {flags} commit -q -m 'wt-a-commit' && \
             echo commit-ok"
        );
        let outcome = run_sandboxed(&fx.wt_a, &script);
        assert_eq!(
            outcome.exit_code,
            Some(0),
            "git add/commit in wt_a failed: {}",
            outcome.log_tail
        );
        assert!(
            outcome.log_tail.contains("commit-ok"),
            "missing commit-ok marker: {}",
            outcome.log_tail
        );
        assert_eq!(git(&fx.wt_a, &["log", "-1", "--format=%s"]), "wt-a-commit");
        assert_eq!(snapshot(&wt_b_state), wt_b_before);
        assert_eq!(snapshot(&sub_state), sub_before);
    }
}
