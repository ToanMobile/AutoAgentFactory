#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Oracle for RFC-126 rework (F1): the reflog file Seatbelt reopens for a
//! linked worktree must be the task's own branch reflog, not whatever the
//! agent-writable `<common>/worktrees/<name>/HEAD` names when the next run's
//! profile is built. A retargeted HEAD must not open a sibling's or main's
//! reflog, and a run that starts detached must still be able to commit on the
//! task branch `aaf/<task_id>` (factory layout `<root>/<repo_id>/<task_id>`).

#[cfg(target_os = "macos")]
mod real {
    use std::fs;
    use std::path::{Path, PathBuf};

    use aaf_domain::ports::{AgentOutcome, AgentRequest, AgentRunner, SandboxPolicy};
    use aaf_engine::executors::{ExecEnv, generic::GenericRunner};

    /// Host-side fixture git: reflogs on, no hooks, no signing.
    const HOST_FLAGS: &str = "-c user.name=aaf-test -c user.email=aaf-test@example.invalid \
        -c commit.gpgsign=false -c core.hooksPath=/dev/null -c core.logAllRefUpdates=true";
    /// Agent-side git: no reflog override, so the injected run env applies.
    const AGENT_GIT: &str = "git -c user.name=aaf-test -c user.email=aaf-test@example.invalid \
        -c commit.gpgsign=false -c core.hooksPath=/dev/null";
    const TASK_ID: &str = "T0126-reopen";

    fn git(repo: &Path, args: &[&str]) -> String {
        let mut full: Vec<&str> = HOST_FLAGS.split_whitespace().collect();
        full.extend_from_slice(args);
        aaf_fp::git(repo, &full, &[], aaf_fp::GIT_TIMEOUT).unwrap()
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        main: PathBuf,
        common: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let main = root.join("main");
            fs::create_dir(&main).unwrap();
            git(&main, &["init", "-q", "-b", "main"]);
            fs::write(main.join("a.txt"), "initial\n").unwrap();
            git(&main, &["add", "a.txt"]);
            git(&main, &["commit", "-q", "-m", "first"]);
            let common = main.join(".git");
            Self {
                _dir: dir,
                root,
                main,
                common,
            }
        }

        fn add_worktree(&self, path: &Path, branch: &str) {
            git(
                &self.main,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    branch,
                    path.to_str().unwrap(),
                ],
            );
        }

        /// `wt_a` on `aaf/T1` and `wt_b` on `aaf/T2`, both beside `main`.
        fn with_siblings() -> (Self, PathBuf) {
            let fx = Self::new();
            let wt_a = fx.root.join("wt_a");
            fx.add_worktree(&wt_a, "aaf/T1");
            fx.add_worktree(&fx.root.join("wt_b"), "aaf/T2");
            for rel in ["logs/refs/heads/main", "logs/refs/heads/aaf/T1"] {
                assert!(fx.common.join(rel).is_file(), "precondition: {rel}");
            }
            assert!(fx.common.join("logs/refs/heads/aaf/T2").is_file());
            (fx, wt_a)
        }
    }

    fn run_sandboxed(cwd: &Path, script: &str) -> AgentOutcome {
        assert!(
            Path::new("/usr/bin/sandbox-exec").exists(),
            "sandbox-exec required"
        );
        let tpl = ["/bin/sh", "-c", script, "sh", "{prompt}"].map(String::from);
        let req = AgentRequest {
            run_id: format!("rfc126r-{}", uuid::Uuid::new_v4()),
            prompt: "unused".into(),
            cwd: cwd.to_string_lossy().into_owned(),
            account: None,
            timeout_secs: 60,
            sandbox: SandboxPolicy {
                enabled: true,
                ..SandboxPolicy::default()
            },
            json_schema: None,
            max_turns: None,
        };
        let out = GenericRunner::new(tpl.to_vec(), ExecEnv::default())
            .unwrap()
            .run(&req)
            .unwrap();
        assert!(!out.timed_out, "sandboxed run timed out: {}", out.log_tail);
        out
    }

    fn run_attack(cwd: &Path, body: &str) -> String {
        let out = run_sandboxed(cwd, &format!("exec 2>&1; {body}; exit 0"));
        assert_eq!(out.exit_code, Some(0), "attack exit != 0: {}", out.log_tail);
        out.log_tail
    }

    /// Run 1 points `wt_a`'s HEAD at `target_ref`; run 2 (fresh profile) tries
    /// to append to and truncate `<common>/logs/<target_ref>`.
    fn assert_retarget_does_not_reopen(target_ref: &str) {
        let (fx, wt_a) = Fixture::with_siblings();
        let own_head = fx.common.join("worktrees/wt_a/HEAD");
        let victim = fx.common.join("logs").join(target_ref);
        let before = fs::read(&victim).unwrap();
        let log1 = run_attack(
            &wt_a,
            &format!(
                "printf 'ref: {target_ref}\\n' > '{}'; echo head_rc=$?",
                own_head.display()
            ),
        );
        let v = victim.display();
        let log2 = run_attack(
            &wt_a,
            &format!("printf X >> '{v}'; echo append_rc=$?; true > '{v}'; echo trunc_rc=$?"),
        );
        let ok = fs::symlink_metadata(&victim).is_ok_and(|m| m.file_type().is_file());
        assert!(
            ok,
            "{v} must remain a plain file; run1:\n{log1}\nrun2:\n{log2}"
        );
        assert_eq!(
            fs::read(&victim).unwrap(),
            before,
            "{v} changed after HEAD retarget to {target_ref}; run1:\n{log1}\nrun2:\n{log2}"
        );
    }

    #[test]
    fn retargeted_head_does_not_reopen_sibling_reflog() {
        assert_retarget_does_not_reopen("refs/heads/aaf/T2");
    }

    #[test]
    fn retargeted_head_does_not_reopen_main_reflog() {
        assert_retarget_does_not_reopen("refs/heads/main");
    }

    fn lines(path: &Path) -> usize {
        fs::read_to_string(path).unwrap().lines().count()
    }

    fn commit_script(prefix: &str) -> String {
        format!(
            "exec 2>&1; {prefix}printf 'change\\n' > b.txt && {AGENT_GIT} add b.txt && \
             {AGENT_GIT} commit -q -m x"
        )
    }

    #[test]
    fn factory_layout_task_reflog_reopened_even_when_detached() {
        let fx = Fixture::new();
        let repo_id = aaf_fp::repo_id(&fx.main);
        assert_eq!(repo_id.len(), 16, "repo_id must be 16 hex chars");
        assert!(repo_id.bytes().all(|b| b.is_ascii_hexdigit()));
        let wts = fx.root.join("wts");
        let wt = wts.join(&repo_id).join(TASK_ID);
        fs::create_dir_all(wt.parent().unwrap()).unwrap();
        let branch = format!("aaf/{TASK_ID}");
        fx.add_worktree(&wt, &branch);
        let branch_log = fx.common.join("logs/refs/heads").join(&branch);
        assert!(branch_log.is_file(), "precondition: task branch reflog");
        let before = lines(&branch_log);

        let out1 = run_sandboxed(&wt, &format!("exec 2>&1; {AGENT_GIT} checkout -q --detach"));
        assert_eq!(out1.exit_code, Some(0), "detach failed: {}", out1.log_tail);
        assert!(git(&wt, &["status", "-sb"]).starts_with("## HEAD (no branch)"));

        let out2 = run_sandboxed(
            &wt,
            &commit_script(&format!("{AGENT_GIT} checkout -q {branch} && ")),
        );
        assert_eq!(
            out2.exit_code,
            Some(0),
            "commit on {branch} after a detached run failed: {}",
            out2.log_tail
        );
        assert_eq!(
            lines(&branch_log),
            before + 1,
            "task branch reflog not appended"
        );
    }

    #[test]
    fn own_branch_commit_still_works() {
        let (fx, wt_a) = Fixture::with_siblings();
        let t1_log = fx.common.join("logs/refs/heads/aaf/T1");
        let head_log = fx.common.join("worktrees/wt_a/logs/HEAD");
        let (t1_before, head_before) = (lines(&t1_log), lines(&head_log));
        let ref_before = git(&wt_a, &["rev-parse", "refs/heads/aaf/T1"]);
        let out = run_sandboxed(&wt_a, &commit_script(""));
        assert_eq!(
            out.exit_code,
            Some(0),
            "own commit failed: {}",
            out.log_tail
        );
        assert_ne!(git(&wt_a, &["rev-parse", "refs/heads/aaf/T1"]), ref_before);
        assert_eq!(lines(&t1_log), t1_before + 1);
        assert_eq!(lines(&head_log), head_before + 1);
    }
}
