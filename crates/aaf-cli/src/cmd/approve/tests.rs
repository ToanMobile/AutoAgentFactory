//! Both sides of the approve guard, with the terminal and the run id
//! injected through `Ctx` (a spawned child cannot be given a real TTY).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use aaf_domain::config::RepoConfig;
use aaf_domain::gate::LockStatus;
use aaf_engine::gate::{GIT_TIMEOUT, base, head_lock};
use serde_json::json;
use tempfile::TempDir;

use super::{ApproveArgs, run};
use crate::ctx::Ctx;
use crate::output::{CmdOutput, Exit};

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=f@example.invalid",
        ])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// A repo where a committed, locked file (`tests/it.rs`) is modified.
fn fixture(locked_change: bool) -> (TempDir, Ctx) {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let repo = root.join("repo");
    fs::create_dir_all(repo.join("tests")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("tests/it.rs"), "// v1\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    if locked_change {
        fs::write(repo.join("tests/it.rs"), "// v2\n").unwrap();
    }
    let ctx = Ctx {
        cwd: repo,
        home: root.join("home"),
        data_dir: root.join("data"),
        config_dir: root.join("config"),
        worktrees_dir: root.join("worktrees"),
        path_var: std::env::var_os("PATH").unwrap_or_default(),
        run_id: None,
        stdin_is_tty: true,
        aaf_env: Vec::new(),
    };
    (tmp, ctx)
}

fn approve(ctx: &Ctx, reviewer: &str) -> CmdOutput {
    let args = ApproveArgs {
        reviewer: reviewer.to_string(),
    };
    run(&args, ctx).unwrap()
}

fn lock_status(ctx: &Ctx) -> LockStatus {
    let cfg = RepoConfig::default();
    let base = base::resolve(&ctx.cwd, &cfg.gate.base_ref, GIT_TIMEOUT).unwrap();
    head_lock::evaluate(&ctx.data_dir, &ctx.cwd, &cfg, &base.diff_target).status
}

fn approval_files(data_dir: &Path) -> Vec<PathBuf> {
    let Ok(repos) = fs::read_dir(data_dir.join("approvals")) else {
        return Vec::new();
    };
    repos
        .flat_map(|repo| fs::read_dir(repo.unwrap().path()).unwrap())
        .map(|file| file.unwrap().path())
        .collect()
}

#[test]
fn a_person_at_a_terminal_signs_an_approval_the_gate_accepts() {
    let (_tmp, ctx) = fixture(true);
    assert_eq!(lock_status(&ctx), LockStatus::Blocked);

    let out = approve(&ctx, " alice ");

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.status, "approved");
    assert_eq!(out.fields["reviewer"], json!("alice"));
    let files = approval_files(&ctx.data_dir);
    assert_eq!(files.len(), 1);
    assert_eq!(out.fields["approval_path"], json!(files[0]));
    let lock_fp = out.fields["lock_fp"].as_str().unwrap();
    assert!(files[0].to_string_lossy().contains(lock_fp));
    assert_eq!(lock_status(&ctx), LockStatus::Approved);

    // The approval covers that exact content only.
    fs::write(ctx.cwd.join("tests/it.rs"), "// v3\n").unwrap();
    assert_eq!(lock_status(&ctx), LockStatus::Blocked);
}

#[test]
fn an_agent_run_is_refused_even_at_a_terminal() {
    let (_tmp, mut ctx) = fixture(true);
    ctx.run_id = Some("run-7".to_string());

    let out = approve(&ctx, "alice");

    assert_eq!(out.exit, Exit::Policy);
    assert_eq!(out.status, "refused");
    assert!(
        out.fields["reason"]
            .as_str()
            .unwrap()
            .contains("AAF_RUN_ID")
    );
    assert!(approval_files(&ctx.data_dir).is_empty());
    assert_eq!(lock_status(&ctx), LockStatus::Blocked);
}

#[test]
fn no_terminal_is_refused() {
    let (_tmp, mut ctx) = fixture(true);
    ctx.stdin_is_tty = false;

    let out = approve(&ctx, "alice");

    assert_eq!(out.exit, Exit::Policy);
    assert_eq!(out.status, "refused");
    assert!(approval_files(&ctx.data_dir).is_empty());
}

#[test]
fn a_blank_reviewer_is_refused() {
    let (_tmp, ctx) = fixture(true);

    let out = approve(&ctx, "  ");

    assert_eq!(out.exit, Exit::Policy);
    assert!(approval_files(&ctx.data_dir).is_empty());
}

#[test]
fn nothing_locked_means_nothing_is_signed() {
    let (_tmp, ctx) = fixture(false);

    let out = approve(&ctx, "alice");

    assert_eq!(out.exit, Exit::Policy);
    assert_eq!(out.status, "nothing_to_approve");
    assert!(approval_files(&ctx.data_dir).is_empty());
}
