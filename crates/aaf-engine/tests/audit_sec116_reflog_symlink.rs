#![allow(clippy::unwrap_used, clippy::expect_used)]
//! SEC-116 verification: host-side git operations (`RealGit::commit_all`,
//! `RealGit::rebase`) must fail closed when any reflog under `<common>/logs/**`
//! or `<common>/worktrees/*/logs/**` is a symlink or multi-link hardlink,
//! without creating or appending to the target ref or host file.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

use aaf_engine::factory::{BaseRef, GitOps, RealGit, resolve_base};
use tempfile::TempDir;

fn git_cmd(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_PREFIX",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

fn run_git(dir: &Path, args: &[&str]) -> String {
    let out = git_cmd(dir, args).output().expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn git_succeeds(dir: &Path, args: &[&str]) -> bool {
    git_cmd(dir, args)
        .output()
        .expect("spawn git")
        .status
        .success()
}

fn git_path(dir: &Path, flag: &str) -> PathBuf {
    let raw = run_git(dir, &["rev-parse", flag]);
    let path = Path::new(&raw);
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        dir.join(path)
    };
    fs::canonicalize(abs).unwrap()
}

fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn prepare_slot(path: &Path) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    if path.symlink_metadata().is_ok() {
        fs::remove_file(path).unwrap();
    }
}

struct Fixture {
    tmp: TempDir,
    repo: PathBuf,
    git: RealGit,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "--quiet", "-b", "main"]);
        for (k, v) in [
            ("user.name", "aaf sec116 test"),
            ("user.email", "sec116@example.invalid"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", "/dev/null"),
        ] {
            run_git(&repo, &["config", k, v]);
        }
        write_file(&repo.join("README.md"), "base commit on main\n");
        run_git(&repo, &["add", "-A"]);
        run_git(&repo, &["commit", "--quiet", "-m", "initial commit"]);

        let base = resolve_base(&repo, "auto").unwrap();
        assert_eq!(
            base,
            BaseRef {
                remote: None,
                branch: "main".into()
            }
        );
        let git = RealGit::new(
            repo.clone(),
            tmp.path().join("worktrees"),
            tmp.path().join("locks"),
            base,
        );
        Self { tmp, repo, git }
    }
}

#[test]
fn commit_all_refuses_branch_reflog_symlink_to_new_ref() {
    let fx = Fixture::new();
    let wt = fx.git.task_worktree("sec116-branch-sym").unwrap();
    let common = git_path(&wt.path, "--git-common-dir");
    let branch_log = common.join("logs/refs/heads").join(&wt.branch);
    let develop_ref = common.join("refs/heads/develop");
    prepare_slot(&branch_log);
    symlink(&develop_ref, &branch_log).unwrap();

    write_file(&wt.path.join("agent.txt"), "unreviewed agent edit\n");
    let res = fx.git.commit_all(&wt, "auto-commit after agent exit");
    let exists = develop_ref.symlink_metadata().is_ok();
    let show_ref_ok = git_succeeds(
        &fx.repo,
        &["show-ref", "--verify", "--quiet", "refs/heads/develop"],
    );
    let content = fs::read_to_string(&develop_ref).unwrap_or_default();
    assert!(
        res.is_err(),
        "SEC-116: commit_all must fail on branch reflog symlink; got Ok(()), exists={exists}, show_ref_ok={show_ref_ok}, content={content:?}"
    );
    assert!(
        !exists && !show_ref_ok,
        "SEC-116: refs/heads/develop must not exist"
    );
}

#[test]
fn commit_all_refuses_worktree_head_log_symlink_to_host_file() {
    let fx = Fixture::new();
    let wt = fx.git.task_worktree("sec116-head-sym").unwrap();
    let head_log = git_path(&wt.path, "--git-dir").join("logs/HEAD");
    let victim = fx.tmp.path().join("victim.txt");
    write_file(&victim, "orig\n");
    prepare_slot(&head_log);
    symlink(&victim, &head_log).unwrap();

    write_file(&wt.path.join("agent.txt"), "unreviewed agent edit\n");
    let res = fx.git.commit_all(&wt, "auto-commit after agent exit");
    let after = fs::read_to_string(&victim).unwrap();
    assert!(
        res.is_err(),
        "SEC-116: commit_all must fail on worktree HEAD reflog symlink; got Ok(()), victim={after:?}"
    );
    assert_eq!(after, "orig\n", "SEC-116: host file must remain untouched");
}

#[test]
fn commit_all_refuses_branch_reflog_hardlink_to_host_file() {
    let fx = Fixture::new();
    let wt = fx.git.task_worktree("sec116-hardlink").unwrap();
    let common = git_path(&wt.path, "--git-common-dir");
    let branch_log = common.join("logs/refs/heads").join(&wt.branch);
    let victim = fx.tmp.path().join("victim-hardlink.txt");
    write_file(&victim, "orig\n");
    prepare_slot(&branch_log);
    fs::hard_link(&victim, &branch_log).unwrap();

    write_file(&wt.path.join("agent.txt"), "unreviewed agent edit\n");
    let res = fx.git.commit_all(&wt, "auto-commit after agent exit");
    let after = fs::read_to_string(&victim).unwrap();
    assert!(
        res.is_err(),
        "SEC-116: commit_all must fail on branch reflog hardlink; got Ok(()), victim={after:?}"
    );
    assert_eq!(after, "orig\n", "SEC-116: host file must remain untouched");
}

#[test]
fn rebase_refuses_branch_reflog_symlink() {
    let fx = Fixture::new();
    let wt = fx.git.task_worktree("sec116-rebase-sym").unwrap();
    write_file(&fx.repo.join("main-advance.txt"), "new commit on main\n");
    run_git(&fx.repo, &["add", "-A"]);
    run_git(&fx.repo, &["commit", "--quiet", "-m", "advance main"]);

    let common = git_path(&wt.path, "--git-common-dir");
    let branch_log = common.join("logs/refs/heads").join(&wt.branch);
    let develop_ref = common.join("refs/heads/develop");
    prepare_slot(&branch_log);
    symlink(&develop_ref, &branch_log).unwrap();

    let res = fx.git.rebase(&wt);
    let exists = develop_ref.symlink_metadata().is_ok();
    let show_ref_ok = git_succeeds(
        &fx.repo,
        &["show-ref", "--verify", "--quiet", "refs/heads/develop"],
    );
    let content = fs::read_to_string(&develop_ref).unwrap_or_default();
    assert!(
        res.is_err(),
        "SEC-116: rebase must fail on branch reflog symlink; got {:?}, exists={exists}, show_ref_ok={show_ref_ok}, content={content:?}",
        res.as_ref().ok(),
    );
    assert!(
        !exists && !show_ref_ok,
        "SEC-116: refs/heads/develop must not exist"
    );
}

#[test]
fn commit_all_with_plain_reflogs_still_commits() {
    let fx = Fixture::new();
    let wt = fx.git.task_worktree("sec116-plain").unwrap();
    let branch_ref = format!("refs/heads/{}", wt.branch);
    let main_before = run_git(&fx.repo, &["rev-parse", "--verify", "refs/heads/main"]);
    let branch_before = run_git(&fx.repo, &["rev-parse", "--verify", &branch_ref]);

    write_file(&wt.path.join("work.txt"), "legitimate task change\n");
    let res = fx.git.commit_all(&wt, "legitimate task commit");
    assert!(
        res.is_ok(),
        "plain commit_all failed: {:?}",
        res.as_ref().err()
    );

    let main_after = run_git(&fx.repo, &["rev-parse", "--verify", "refs/heads/main"]);
    let branch_after = run_git(&fx.repo, &["rev-parse", "--verify", &branch_ref]);
    let count = run_git(
        &fx.repo,
        &[
            "rev-list",
            "--count",
            &format!("{branch_before}..{branch_after}"),
        ],
    );
    assert_eq!(count, "1", "task branch should advance by 1 commit");
    assert_eq!(main_after, main_before, "refs/heads/main must not change");
}
