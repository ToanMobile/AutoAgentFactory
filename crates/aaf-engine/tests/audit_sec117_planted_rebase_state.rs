#![allow(clippy::unwrap_used, clippy::expect_used)]
//! SEC-117 verification: host-side `RealGit::rebase` must not allow planted
//! `rebase-merge/` or `rebase-apply/` state in a task worktree's admin dir
//! (`<common>/worktrees/<own>/`) to hijack `git rebase --abort` into updating
//! `refs/heads/main` (or any ref outside the task branch).

use std::fs;
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

struct Fixture {
    _tmp: TempDir,
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
            ("user.name", "aaf sec117 test"),
            ("user.email", "sec117@example.invalid"),
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
                branch: "main".into(),
            }
        );
        let git = RealGit::new(
            repo.clone(),
            tmp.path().join("worktrees"),
            tmp.path().join("locks"),
            base,
        );
        Self {
            _tmp: tmp,
            repo,
            git,
        }
    }

    fn advance_main(&self) -> String {
        write_file(&self.repo.join("main-advance.txt"), "new commit on main\n");
        run_git(&self.repo, &["add", "-A"]);
        run_git(&self.repo, &["commit", "--quiet", "-m", "advance main"]);
        run_git(&self.repo, &["rev-parse", "--verify", "refs/heads/main"])
    }
}

#[test]
fn rebase_with_planted_rebase_merge_does_not_move_main() {
    let fx = Fixture::new();
    let wt = fx.git.task_worktree("sec117-rebase-merge").unwrap();

    write_file(&wt.path.join("agent.txt"), "ungated agent commit\n");
    fx.git.commit_all(&wt, "agent commit").unwrap();
    let agent_oid = run_git(&wt.path, &["rev-parse", "--verify", "HEAD"]);
    let main_before = fx.advance_main();
    assert_ne!(agent_oid, main_before);

    let own = git_path(&wt.path, "--git-dir");
    let state_dir = own.join("rebase-merge");
    write_file(&state_dir.join("head-name"), "refs/heads/main\n");
    write_file(&state_dir.join("orig-head"), &format!("{agent_oid}\n"));
    write_file(&state_dir.join("onto"), &format!("{agent_oid}\n"));
    write_file(&state_dir.join("interactive"), "");

    let res = fx.git.rebase(&wt);
    let main_after = run_git(&fx.repo, &["rev-parse", "--verify", "refs/heads/main"]);
    assert_eq!(
        main_after, main_before,
        "SEC-117: refs/heads/main moved {main_before} -> {main_after} (agent_oid={agent_oid}) \
         during rebase with planted rebase-merge (result={res:?})"
    );
}

#[test]
fn rebase_with_planted_rebase_apply_does_not_move_main() {
    let fx = Fixture::new();
    let wt = fx.git.task_worktree("sec117-rebase-apply").unwrap();

    write_file(&wt.path.join("agent.txt"), "ungated agent commit\n");
    fx.git.commit_all(&wt, "agent commit").unwrap();
    let agent_oid = run_git(&wt.path, &["rev-parse", "--verify", "HEAD"]);
    let main_before = fx.advance_main();
    assert_ne!(agent_oid, main_before);

    let own = git_path(&wt.path, "--git-dir");
    let state_dir = own.join("rebase-apply");
    write_file(&state_dir.join("head-name"), "refs/heads/main\n");
    write_file(&state_dir.join("orig-head"), &format!("{agent_oid}\n"));
    write_file(&state_dir.join("onto"), &format!("{agent_oid}\n"));
    write_file(&state_dir.join("rebasing"), "");

    let res = fx.git.rebase(&wt);
    let main_after = run_git(&fx.repo, &["rev-parse", "--verify", "refs/heads/main"]);
    assert_eq!(
        main_after, main_before,
        "SEC-117: refs/heads/main moved {main_before} -> {main_after} (agent_oid={agent_oid}) \
         during rebase with planted rebase-apply (result={res:?})"
    );
}

#[test]
fn rebase_without_planted_state_still_rebases() {
    let fx = Fixture::new();
    let wt = fx.git.task_worktree("sec117-clean").unwrap();
    let branch_ref = format!("refs/heads/{}", wt.branch);

    write_file(&wt.path.join("agent.txt"), "legitimate task change\n");
    fx.git.commit_all(&wt, "legitimate task commit").unwrap();
    let main_before = fx.advance_main();

    let conflicts = fx
        .git
        .rebase(&wt)
        .expect("clean rebase without planted state should succeed");
    assert!(
        conflicts.is_empty(),
        "clean rebase should have no conflicts, got: {conflicts:?}"
    );

    let main_after = run_git(&fx.repo, &["rev-parse", "--verify", "refs/heads/main"]);
    assert_eq!(main_after, main_before, "refs/heads/main must not change");
    assert!(
        git_succeeds(
            &fx.repo,
            &["merge-base", "--is-ancestor", &main_before, &branch_ref],
        ),
        "task branch {branch_ref} should contain the new main commit {main_before}"
    );
    assert_eq!(run_git(&wt.path, &["symbolic-ref", "HEAD"]), branch_ref);
    assert!(wt.path.join("agent.txt").is_file());
    assert!(wt.path.join("main-advance.txt").is_file());
}
