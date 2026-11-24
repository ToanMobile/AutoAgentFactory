#![allow(clippy::unwrap_used, clippy::expect_used)]
//! SEC-022 verification oracle: `BaseRef::start_point()` emits short ref names
//! (`"main"` / `"origin/main"`), which Git resolves against `refs/tags/*` before
//! `refs/heads/*` and `refs/remotes/*` (`gitrevisions(7)`).
//!
//! When a tag shadows the base branch (`refs/tags/main` or
//! `refs/tags/origin/main`):
//! - `RealGit::base_sha()` must still return the real branch commit SHA.
//! - `RealGit::task_worktree()` must cut cleanly from the real branch and must
//!   NOT hydrate/execute `[worktree].setup_cmd` from the shadowing tag commit.
//! - `RealGit::is_merged()` must check ancestry against the real branch, not
//!   the shadowing tag.
//! - `RealGit::rebase()` must rebase onto the real branch, not pull commits
//!   from the shadowing tag, and `load_base_worktree_config` fed `base_sha()`
//!   must not load the shadowing tag's `setup_cmd`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use aaf_engine::config::REPO_CONFIG_REL;
use aaf_engine::factory::{BaseRef, GitOps, RealGit, resolve_base};
use aaf_engine::git::hydration::load_base_worktree_config;
use tempfile::TempDir;

const SCRUBBED_GIT_ENV: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_PREFIX",
];

fn run_git(dir: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    for var in SCRUBBED_GIT_ENV {
        cmd.env_remove(var);
    }
    let out = cmd.output().expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed (status={:?}):\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn init_repo_config(repo: &Path) {
    for (key, value) in [
        ("user.name", "aaf sec022 test"),
        ("user.email", "sec022@example.invalid"),
        ("commit.gpgsign", "false"),
        ("core.hooksPath", "/dev/null"),
    ] {
        run_git(repo, &["config", key, value]);
    }
}

fn attacker_worktree_toml(marker: &Path) -> String {
    let escaped_path = marker.to_string_lossy().replace('\'', "'\\''");
    let setup_cmd = format!("touch '{escaped_path}'");
    let encoded_cmd = toml::Value::String(setup_cmd).to_string();
    format!("[worktree]\nsetup_cmd = {encoded_cmd}\n")
}

struct ShadowFixture {
    _tmp: TempDir,
    repo: PathBuf,
    trusted_base_sha: String,
    attacker_tag_sha: String,
    marker: PathBuf,
    git: RealGit,
}

impl ShadowFixture {
    /// Local repository with base `refs/heads/main`. Call `plant_shadow_tag()`
    /// immediately or after creating a pre-existing task worktree.
    fn local_main(plant_tag_now: bool) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "--quiet", "-b", "main"]);
        init_repo_config(&repo);

        write_file(&repo.join("README.md"), "trusted base\n");
        run_git(&repo, &["add", "-A"]);
        run_git(&repo, &["commit", "--quiet", "-m", "trusted base"]);
        let trusted_base_sha = run_git(&repo, &["rev-parse", "--verify", "refs/heads/main"]);

        let marker = tmp.path().join("sec022-local-rce.marker");
        run_git(&repo, &["checkout", "--quiet", "-b", "attacker-payload"]);
        write_file(
            &repo.join(REPO_CONFIG_REL),
            &attacker_worktree_toml(&marker),
        );
        write_file(&repo.join("pwned.txt"), "from shadowing tag\n");
        run_git(&repo, &["add", "-A"]);
        run_git(&repo, &["commit", "--quiet", "-m", "attacker setup_cmd"]);
        let attacker_tag_sha = run_git(&repo, &["rev-parse", "--verify", "HEAD"]);

        run_git(&repo, &["checkout", "--quiet", "main"]);
        // Unmerged task branch pointing at the attacker commit.
        run_git(&repo, &["branch", "aaf/attacker-local", &attacker_tag_sha]);

        if plant_tag_now {
            // Shadow `refs/heads/main` with `refs/tags/main`.
            run_git(&repo, &["tag", "main", &attacker_tag_sha]);
            assert_eq!(
                run_git(&repo, &["rev-parse", "--verify", "refs/tags/main"]),
                attacker_tag_sha
            );
        }

        assert_ne!(trusted_base_sha, attacker_tag_sha);
        assert_eq!(
            run_git(&repo, &["rev-parse", "--verify", "refs/heads/main"]),
            trusted_base_sha
        );

        let base = resolve_base(&repo, "auto").unwrap();
        assert_eq!(
            base,
            BaseRef {
                remote: None,
                branch: "main".to_string(),
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
            trusted_base_sha,
            attacker_tag_sha,
            marker,
            git,
        }
    }

    /// Repository with remote `origin` (`refs/remotes/origin/main`). Call
    /// `plant_tag_now = true` to immediately plant `refs/tags/origin/main`.
    fn remote_origin_main(plant_tag_now: bool) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        fs::create_dir_all(&origin).unwrap();
        run_git(&origin, &["init", "--bare", "--quiet", "-b", "main"]);

        let repo = tmp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "--quiet", "-b", "main"]);
        init_repo_config(&repo);
        run_git(
            &repo,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );

        write_file(&repo.join("README.md"), "trusted remote base\n");
        run_git(&repo, &["add", "-A"]);
        run_git(&repo, &["commit", "--quiet", "-m", "trusted remote base"]);
        run_git(&repo, &["push", "--quiet", "-u", "origin", "main"]);
        let trusted_base_sha = run_git(
            &repo,
            &["rev-parse", "--verify", "refs/remotes/origin/main"],
        );

        let marker = tmp.path().join("sec022-remote-rce.marker");
        run_git(&repo, &["checkout", "--quiet", "-b", "attacker-payload"]);
        write_file(
            &repo.join(REPO_CONFIG_REL),
            &attacker_worktree_toml(&marker),
        );
        write_file(&repo.join("pwned.txt"), "from shadowing remote tag\n");
        run_git(&repo, &["add", "-A"]);
        run_git(&repo, &["commit", "--quiet", "-m", "attacker setup_cmd"]);
        let attacker_tag_sha = run_git(&repo, &["rev-parse", "--verify", "HEAD"]);

        run_git(&repo, &["checkout", "--quiet", "main"]);
        // Unmerged task branch pointing at the attacker commit.
        run_git(&repo, &["branch", "aaf/attacker-remote", &attacker_tag_sha]);

        if plant_tag_now {
            // Shadow `refs/remotes/origin/main` with `refs/tags/origin/main`.
            run_git(&repo, &["tag", "origin/main", &attacker_tag_sha]);
            assert_eq!(
                run_git(&repo, &["rev-parse", "--verify", "refs/tags/origin/main"]),
                attacker_tag_sha
            );
        }

        assert_ne!(trusted_base_sha, attacker_tag_sha);
        assert_eq!(
            run_git(
                &repo,
                &["rev-parse", "--verify", "refs/remotes/origin/main"]
            ),
            trusted_base_sha
        );

        let base = resolve_base(&repo, "auto").unwrap();
        assert_eq!(
            base,
            BaseRef {
                remote: Some("origin".to_string()),
                branch: "main".to_string(),
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
            trusted_base_sha,
            attacker_tag_sha,
            marker,
            git,
        }
    }

    fn plant_tag(&self, tag_name: &str) {
        run_git(&self.repo, &["tag", tag_name, &self.attacker_tag_sha]);
    }
}

#[test]
fn local_base_sha_resolves_refs_heads_not_shadowing_tag() {
    let fx = ShadowFixture::local_main(true);
    let resolved = fx.git.base_sha().unwrap();
    assert_eq!(
        resolved, fx.trusted_base_sha,
        "RealGit::base_sha() resolved shadowing tag refs/tags/main ({}) instead of refs/heads/main ({})",
        fx.attacker_tag_sha, fx.trusted_base_sha,
    );
}

#[test]
fn local_task_worktree_cuts_from_branch_and_blocks_tag_setup_cmd() {
    let fx = ShadowFixture::local_main(true);
    let res = fx.git.task_worktree("sec022-local");
    assert!(
        res.is_ok(),
        "RealGit::task_worktree() must cut cleanly from refs/heads/main even when refs/tags/main exists; got error: {:?}",
        res.as_ref().err(),
    );
    let wt = res.unwrap();

    assert!(
        !fx.marker.exists(),
        "attacker [worktree].setup_cmd from shadowing tag refs/tags/main executed on host and created {}",
        fx.marker.display(),
    );
    let wt_head = run_git(&wt.path, &["rev-parse", "--verify", "HEAD"]);
    assert_eq!(
        wt_head, fx.trusted_base_sha,
        "task_worktree cut from shadowing tag ({}) instead of refs/heads/main ({})",
        fx.attacker_tag_sha, fx.trusted_base_sha,
    );
    assert!(
        !wt.path.join("pwned.txt").exists(),
        "attacker file from shadowing tag leaked into fresh worktree"
    );
}

#[test]
fn local_is_merged_checks_refs_heads_not_shadowing_tag() {
    let fx = ShadowFixture::local_main(true);
    assert!(
        !fx.git.is_merged("attacker-local").unwrap(),
        "RealGit::is_merged() treated unmerged attacker branch as merged via shadowing tag refs/tags/main"
    );
}

#[test]
fn local_rebase_and_base_config_ignore_shadowing_tag() {
    let fx = ShadowFixture::local_main(false);
    let wt = fx.git.task_worktree("pre-tag-local").unwrap();
    write_file(&wt.path.join("task.txt"), "legit change\n");
    fx.git.commit_all(&wt, "legit task commit").unwrap();

    fx.plant_tag("main");

    let conflicts = fx.git.rebase(&wt).unwrap();
    assert!(
        conflicts.is_empty(),
        "unexpected rebase conflicts: {conflicts:?}"
    );
    assert!(
        !wt.path.join("pwned.txt").exists(),
        "RealGit::rebase() silently rebased task worktree onto shadowing tag refs/tags/main"
    );
    let base_sha = fx.git.base_sha().unwrap();
    let cfg = load_base_worktree_config(&fx.repo, &base_sha).unwrap();
    assert!(
        cfg.setup_cmd.is_empty(),
        "load_base_worktree_config(RealGit::base_sha()) loaded attacker setup_cmd from refs/tags/main: {:?}",
        cfg.setup_cmd,
    );
}

#[test]
fn remote_base_sha_resolves_refs_remotes_not_shadowing_tag() {
    let fx = ShadowFixture::remote_origin_main(true);
    let resolved = fx.git.base_sha().unwrap();
    assert_eq!(
        resolved, fx.trusted_base_sha,
        "RealGit::base_sha() resolved shadowing tag refs/tags/origin/main ({}) instead of refs/remotes/origin/main ({})",
        fx.attacker_tag_sha, fx.trusted_base_sha,
    );
}

#[test]
fn remote_task_worktree_cuts_from_remote_branch_and_blocks_tag_setup_cmd() {
    let fx = ShadowFixture::remote_origin_main(true);
    let res = fx.git.task_worktree("sec022-remote");
    assert!(
        res.is_ok(),
        "RealGit::task_worktree() must cut cleanly from refs/remotes/origin/main even when refs/tags/origin/main exists; got error: {:?}",
        res.as_ref().err(),
    );
    let wt = res.unwrap();

    assert!(
        !fx.marker.exists(),
        "attacker [worktree].setup_cmd from shadowing tag refs/tags/origin/main executed on host and created {}",
        fx.marker.display(),
    );
    let wt_head = run_git(&wt.path, &["rev-parse", "--verify", "HEAD"]);
    assert_eq!(
        wt_head, fx.trusted_base_sha,
        "task_worktree cut from shadowing tag ({}) instead of refs/remotes/origin/main ({})",
        fx.attacker_tag_sha, fx.trusted_base_sha,
    );
    assert!(
        !wt.path.join("pwned.txt").exists(),
        "attacker file from shadowing tag leaked into fresh worktree"
    );
}

#[test]
fn remote_is_merged_checks_refs_remotes_not_shadowing_tag() {
    let fx = ShadowFixture::remote_origin_main(true);
    assert!(
        !fx.git.is_merged("attacker-remote").unwrap(),
        "RealGit::is_merged() treated unmerged attacker branch as merged via shadowing tag refs/tags/origin/main"
    );
}

#[test]
fn remote_rebase_and_base_config_ignore_shadowing_tag() {
    let fx = ShadowFixture::remote_origin_main(false);
    let wt = fx.git.task_worktree("pre-tag-remote").unwrap();
    write_file(&wt.path.join("task.txt"), "legit remote change\n");
    fx.git.commit_all(&wt, "legit task commit").unwrap();

    fx.plant_tag("origin/main");

    let conflicts = fx.git.rebase(&wt).unwrap();
    assert!(
        conflicts.is_empty(),
        "unexpected rebase conflicts: {conflicts:?}"
    );
    assert!(
        !wt.path.join("pwned.txt").exists(),
        "RealGit::rebase() silently rebased task worktree onto shadowing tag refs/tags/origin/main"
    );
    let base_sha = fx.git.base_sha().unwrap();
    let cfg = load_base_worktree_config(&fx.repo, &base_sha).unwrap();
    assert!(
        cfg.setup_cmd.is_empty(),
        "load_base_worktree_config(RealGit::base_sha()) loaded attacker setup_cmd from refs/tags/origin/main: {:?}",
        cfg.setup_cmd,
    );
}
