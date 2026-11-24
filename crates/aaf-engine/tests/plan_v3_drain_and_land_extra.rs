#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Plan v3 Wave 3 Slice 1 tests (part 2 — split from `plan_v3_drain_and_land.rs`):
//! `resolve_base` + `gate::base::resolve`, `sync_local_base`,
//! `commit_all --no-verify`, and `LandMode` ambient monotonicity.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use aaf_domain::config::LandMode;
use aaf_engine::config::{ConfigInputs, ConfigOverlay, RunOverlay};
use aaf_engine::factory::{BaseRef, GitOps, RealGit, resolve_base};
use aaf_engine::gate::base as gate_base;
use aaf_engine::git::pr_merge::sync_local_base;
use tempfile::TempDir;

fn git_cmd(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn resolve_base_and_gate_base_use_upstream_on_develop_without_origin_head() {
    let origin = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    git_cmd(origin.path(), &["init", "-q", "--bare", "-b", "develop"]);
    git_cmd(repo.path(), &["init", "-q", "-b", "develop"]);
    git_cmd(
        repo.path(),
        &["remote", "add", "origin", origin.path().to_str().unwrap()],
    );
    fs::write(repo.path().join("README.md"), "base\n").unwrap();
    git_cmd(repo.path(), &["add", "-A"]);
    git_cmd(repo.path(), &["commit", "-q", "-m", "init"]);
    let base_sha = git_cmd(repo.path(), &["rev-parse", "HEAD"]);
    git_cmd(repo.path(), &["push", "-q", "-u", "origin", "develop"]);

    fs::write(repo.path().join("app.txt"), "change\n").unwrap();
    git_cmd(repo.path(), &["add", "-A"]);
    git_cmd(repo.path(), &["commit", "-q", "-m", "local work"]);

    assert_eq!(
        resolve_base(repo.path(), "auto").unwrap(),
        BaseRef {
            remote: Some("origin".into()),
            branch: "develop".into()
        }
    );
    let gate_resolved = gate_base::resolve(repo.path(), "auto", Duration::from_secs(15)).unwrap();
    assert_eq!(gate_resolved.sha.as_deref(), Some(base_sha.as_str()));

    // `aaf/*` upstream branch must be rejected.
    git_cmd(repo.path(), &["checkout", "-q", "-b", "aaf/task-99"]);
    git_cmd(repo.path(), &["push", "-q", "-u", "origin", "aaf/task-99"]);
    assert_eq!(resolve_base(repo.path(), "auto").unwrap().remote, None);
}

#[test]
fn sync_local_base_cas_updates_base_when_checked_out_on_feature_branch() {
    let origin = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    let other = TempDir::new().unwrap();
    git_cmd(origin.path(), &["init", "-q", "--bare", "-b", "main"]);
    git_cmd(repo.path(), &["init", "-q", "-b", "main"]);
    git_cmd(
        repo.path(),
        &["remote", "add", "origin", origin.path().to_str().unwrap()],
    );
    fs::write(repo.path().join("a.txt"), "v1\n").unwrap();
    git_cmd(repo.path(), &["add", "-A"]);
    git_cmd(repo.path(), &["commit", "-q", "-m", "init"]);
    git_cmd(repo.path(), &["push", "-q", "-u", "origin", "main"]);
    git_cmd(repo.path(), &["checkout", "-q", "-b", "feat/work"]);

    let clone = other.path().join("c");
    git_cmd(
        other.path(),
        &[
            "clone",
            "-q",
            origin.path().to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    fs::write(clone.join("b.txt"), "v2\n").unwrap();
    git_cmd(&clone, &["add", "-A"]);
    git_cmd(&clone, &["commit", "-q", "-m", "upstream"]);
    let new_main = git_cmd(&clone, &["rev-parse", "HEAD"]);
    git_cmd(&clone, &["push", "-q", "origin", "main"]);

    let note = sync_local_base(repo.path(), "origin", "main");
    assert!(note.contains("update-ref"), "{note}");
    assert_eq!(
        git_cmd(repo.path(), &["rev-parse", "refs/heads/main"]),
        new_main
    );
    assert_eq!(
        git_cmd(repo.path(), &["symbolic-ref", "--short", "HEAD"]),
        "feat/work"
    );
}

#[test]
fn commit_all_bypasses_pre_commit_hook_and_gpg_signing() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    git_cmd(&repo, &["init", "-q", "-b", "main"]);
    git_cmd(&repo, &["config", "user.name", "tester"]);
    git_cmd(&repo, &["config", "user.email", "t@example.invalid"]);
    fs::write(repo.join("README.md"), "hi\n").unwrap();
    git_cmd(&repo, &["add", "-A"]);
    git_cmd(&repo, &["commit", "-q", "-m", "init"]);

    let husky = repo.join(".husky");
    fs::create_dir_all(&husky).unwrap();
    let marker = tmp.path().join("hook-executed");
    let hook = husky.join("pre-commit");
    fs::write(
        &hook,
        format!("#!/bin/sh\necho pwn > '{}'\nexit 1\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    git_cmd(
        &repo,
        &["config", "core.hooksPath", husky.to_str().unwrap()],
    );
    git_cmd(&repo, &["config", "commit.gpgsign", "true"]);
    git_cmd(&repo, &["config", "gpg.program", "/nonexistent/gpg"]);

    let real_git = RealGit::new(
        repo.clone(),
        tmp.path().join("worktrees"),
        tmp.path().join("locks"),
        resolve_base(&repo, "auto").unwrap(),
    );
    let wt = real_git.task_worktree("t-hook").unwrap();
    fs::write(wt.path.join("work.txt"), "hello\n").unwrap();
    real_git.commit_all(&wt, "aaf: test commit").unwrap();
    assert!(!marker.exists(), "commit_all must pass --no-verify");
    assert_eq!(git_cmd(&wt.path, &["status", "--porcelain"]), "");
}

#[test]
fn land_mode_ambient_overlay_can_only_lower_never_raise() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join(".autofactory")).unwrap();
    fs::write(
        repo.join(".autofactory/config.toml"),
        "[run]\nland = \"pr\"\n",
    )
    .unwrap();
    fs::write(
        repo.join(".autofactory/config.local.toml"),
        "[run]\nland = \"auto\"\n",
    )
    .unwrap();
    let eff = aaf_engine::config::load(&ConfigInputs {
        global_config: None,
        repo_root: repo.clone(),
        env: &|_| None,
        cli: ConfigOverlay::default(),
    })
    .unwrap();
    assert_eq!(eff.repo.run.land, LandMode::Pr);
    assert!(!eff.repo.run.auto_merge);
    assert!(
        eff.warnings
            .iter()
            .any(|w| w.contains("run.land = auto ignored"))
    );

    fs::write(
        repo.join(".autofactory/config.local.toml"),
        "[run]\nland = \"branch\"\n",
    )
    .unwrap();
    let lowered = aaf_engine::config::load(&ConfigInputs {
        global_config: None,
        repo_root: repo.clone(),
        env: &|_| None,
        cli: ConfigOverlay::default(),
    })
    .unwrap();
    assert_eq!(lowered.repo.run.land, LandMode::Branch);
    assert!(!lowered.repo.run.auto_merge);

    let cli_up = aaf_engine::config::load(&ConfigInputs {
        global_config: None,
        repo_root: repo,
        env: &|_| None,
        cli: ConfigOverlay {
            run: RunOverlay {
                land: Some(LandMode::PrMerge),
                ..RunOverlay::default()
            },
            ..ConfigOverlay::default()
        },
    })
    .unwrap();
    assert_eq!(cli_up.repo.run.land, LandMode::PrMerge);
    assert!(cli_up.repo.run.auto_merge);
}
