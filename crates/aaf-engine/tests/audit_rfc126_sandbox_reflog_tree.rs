#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Oracle verification for RFC-126: under macOS Seatbelt, an agent in linked
//! worktree `wt_a` must not alter reflog structure under `<common>/logs/**` or
//! `<common>/worktrees/wt_a/logs/**`, while `git add` + `commit` + `--amend`
//! inside `wt_a` must still succeed and append reflogs.

#[cfg(target_os = "macos")]
mod real {
    use std::fs;
    use std::path::{Path, PathBuf};

    use aaf_domain::ports::{AgentOutcome, AgentRequest, AgentRunner, SandboxPolicy};
    use aaf_engine::executors::{ExecEnv, generic::GenericRunner};

    const GIT_FLAGS: &str = "-c user.name=aaf-test -c user.email=aaf-test@example.invalid \
        -c commit.gpgsign=false -c core.hooksPath=/dev/null -c core.logAllRefUpdates=true";
    const OUTSIDE: [(&str, &str); 3] = [
        ("T1", "outside-T1\n"),
        ("T2", "outside-T2\n"),
        ("HEAD", "outside-HEAD\n"),
    ];

    fn git(repo: &Path, args: &[&str]) -> String {
        let mut full: Vec<&str> = GIT_FLAGS.split_whitespace().collect();
        full.extend_from_slice(args);
        aaf_fp::git(repo, &full, &[], aaf_fp::GIT_TIMEOUT).unwrap()
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        common: PathBuf,
        wt_a: PathBuf,
        outside: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let main = root.join("main");
        fs::create_dir(&main).unwrap();
        git(&main, &["init", "-q"]);
        fs::write(main.join("a.txt"), "initial\n").unwrap();
        git(&main, &["add", "a.txt"]);
        git(&main, &["commit", "-q", "-m", "first"]);
        for (name, branch) in [("wt_a", "aaf/T1"), ("wt_b", "aaf/T2")] {
            let wt = root.join(name);
            git(
                &main,
                &["worktree", "add", "-q", "-b", branch, wt.to_str().unwrap()],
            );
        }
        let outside = root.join("outside");
        fs::create_dir(&outside).unwrap();
        for (name, body) in OUTSIDE {
            fs::write(outside.join(name), body).unwrap();
        }
        let common = main.join(".git");
        for rel in [
            "logs/HEAD",
            "logs/refs/heads/aaf/T1",
            "logs/refs/heads/aaf/T2",
            "worktrees/wt_a/logs/HEAD",
        ] {
            let ok = fs::symlink_metadata(common.join(rel)).is_ok_and(|m| m.file_type().is_file());
            assert!(ok, "precondition: missing plain reflog {rel}");
        }
        Fixture {
            _dir: dir,
            common,
            wt_a: root.join("wt_a"),
            outside,
        }
    }

    fn run_sandboxed(cwd: &Path, script: &str) -> AgentOutcome {
        assert!(
            Path::new("/usr/bin/sandbox-exec").exists(),
            "sandbox-exec required"
        );
        let tpl = ["/bin/sh", "-c", script, "sh", "{prompt}"].map(String::from);
        let req = AgentRequest {
            run_id: format!("rfc126-{}", uuid::Uuid::new_v4()),
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

    fn assert_outside_intact(outside: &Path, log: &str) {
        for (name, expected) in OUTSIDE {
            let got = fs::read_to_string(outside.join(name)).unwrap();
            assert_eq!(got, expected, "outside/{name} modified; log:\n{log}");
        }
    }

    fn assert_plain_dir(path: &Path, log: &str) {
        let ok = fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_dir());
        assert!(
            ok,
            "{} must remain a plain dir; log:\n{log}",
            path.display()
        );
    }

    fn assert_plain_file(path: &Path, expected: &[u8], log: &str) {
        let ok = fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file());
        assert!(
            ok,
            "{} must remain a plain file; log:\n{log}",
            path.display()
        );
        assert_eq!(
            fs::read(path).unwrap(),
            expected,
            "{} changed; log:\n{log}",
            path.display()
        );
    }

    fn assert_file_replace_denied(fx: &Fixture, rel: &str, outside_name: &str) {
        let target = fx.common.join(rel);
        let before = fs::read(&target).unwrap();
        let (t, o) = (target.display(), fx.outside.display());
        let log = run_attack(
            &fx.wt_a,
            &format!("rm -f '{t}' && ln -s '{o}/{outside_name}' '{t}'"),
        );
        assert_plain_file(&target, &before, &log);
        assert_outside_intact(&fx.outside, &log);
    }

    #[test]
    fn logs_dir_swap_is_denied() {
        let fx = fixture();
        let aaf = fx.common.join("logs/refs/heads/aaf");
        let moved = fx.common.join("logs/refs/heads/aaf.x");
        let (a, m, o) = (aaf.display(), moved.display(), fx.outside.display());
        let log = run_attack(&fx.wt_a, &format!("mv '{a}' '{m}' && ln -s '{o}' '{a}'"));
        assert_plain_dir(&aaf, &log);
        assert!(
            fs::symlink_metadata(&moved).is_err(),
            "{m} created; log:\n{log}"
        );
        assert_outside_intact(&fx.outside, &log);
    }

    #[test]
    fn sibling_reflog_file_replace_is_denied() {
        assert_file_replace_denied(&fixture(), "logs/refs/heads/aaf/T2", "T2");
    }

    #[test]
    fn own_reflog_file_replace_is_denied() {
        assert_file_replace_denied(&fixture(), "logs/refs/heads/aaf/T1", "T1");
    }

    #[test]
    fn new_file_under_logs_is_denied() {
        let fx = fixture();
        let new_link = fx.common.join("logs/refs/heads/aaf/T9");
        let new_dir = fx.common.join("logs/refs/heads/zz");
        let (o, l, d) = (fx.outside.display(), new_link.display(), new_dir.display());
        let log = run_attack(&fx.wt_a, &format!("ln -s '{o}/HEAD' '{l}'; mkdir -p '{d}'"));
        for p in [&new_link, &new_dir] {
            assert!(
                fs::symlink_metadata(p).is_err(),
                "{} created; log:\n{log}",
                p.display()
            );
        }
        assert_outside_intact(&fx.outside, &log);
    }

    #[test]
    fn common_logs_head_replace_is_denied() {
        assert_file_replace_denied(&fixture(), "logs/HEAD", "HEAD");
    }

    #[test]
    fn own_worktree_logs_replace_is_denied() {
        let fx = fixture();
        let wt_logs = fx.common.join("worktrees/wt_a/logs");
        let moved = fx.common.join("worktrees/wt_a/logs.x");
        let wt_head = wt_logs.join("HEAD");
        let before = fs::read(&wt_head).unwrap();
        let (h, o, d, m) = (
            wt_head.display(),
            fx.outside.display(),
            wt_logs.display(),
            moved.display(),
        );
        let cmd = format!("rm -f '{h}' && ln -s '{o}/HEAD' '{h}'; mv '{d}' '{m}'");
        let log = run_attack(&fx.wt_a, &cmd);
        assert_plain_dir(&wt_logs, &log);
        assert!(
            fs::symlink_metadata(&moved).is_err(),
            "{m} created; log:\n{log}"
        );
        assert_plain_file(&wt_head, &before, &log);
        assert_outside_intact(&fx.outside, &log);
    }

    #[test]
    fn own_commit_and_amend_still_work() {
        let fx = fixture();
        let t1_log = fx.common.join("logs/refs/heads/aaf/T1");
        let wt_head_log = fx.common.join("worktrees/wt_a/logs/HEAD");
        let lines = |p: &Path| fs::read_to_string(p).unwrap().lines().count();
        let ref_before = git(&fx.wt_a, &["rev-parse", "refs/heads/aaf/T1"]);
        let (t1_before, head_before) = (lines(&t1_log), lines(&wt_head_log));
        let script = format!(
            "printf 'change\\n' > b.txt && git add b.txt && \
             git {GIT_FLAGS} commit -q -m x && git {GIT_FLAGS} commit -q --amend -m y"
        );
        let out = run_sandboxed(&fx.wt_a, &script);
        assert_eq!(
            out.exit_code,
            Some(0),
            "commit+amend failed: {}",
            out.log_tail
        );
        assert_ne!(
            git(&fx.wt_a, &["rev-parse", "refs/heads/aaf/T1"]),
            ref_before
        );
        assert_eq!(lines(&t1_log), t1_before + 2);
        assert_eq!(lines(&wt_head_log), head_before + 2);
    }
}
