#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
//! Once a task's repository is gone, its handover may only borrow the config
//! of the current repository when that is the SAME repository (same common
//! repo id). A different repository's config (e.g. `proof.required = false`)
//! must not waive the task's proof: without its own config, proof is
//! required (fail closed). Applies to `aaf handover` and the cockpit `h`.

#[path = "../src/ctx.rs"]
mod ctx;
#[path = "../src/output.rs"]
mod output;

#[path = "../src/cmd"]
mod cmd {
    pub mod board {
        pub mod store;
    }
    pub mod cockpit;
    pub mod cockpit_actions;
    pub mod handover;
    pub mod scope;
    pub mod task_ops;
}

use std::fs;
use std::io::Cursor;
use std::path::Path;
use std::process::{Command, Stdio};

use aaf_domain::ports::TaskStore;
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use tempfile::TempDir;

use crate::cmd::cockpit::run_cockpit;
use crate::cmd::handover::{HandoverArgs, HandoverFormat};
use crate::ctx::Ctx;

const PROOF_MISSING: &str = "missing verified device proof (proof.required = true)";

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
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
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?} failed");
}

/// A committed repo at `dir` whose config says `proof.required = <required>`.
fn repo_with_proof(dir: &Path, required: bool) {
    fs::create_dir_all(dir.join(".autofactory")).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    fs::write(
        dir.join(".autofactory/config.toml"),
        format!("[proof]\nrequired = {required}\n"),
    )
    .unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "initial"]);
}

fn ctx_in(root: &Path, cwd: &Path) -> Ctx {
    let ctx = Ctx {
        cwd: cwd.to_path_buf(),
        home: root.join("home"),
        data_dir: root.join("data"),
        config_dir: root.join("config"),
        worktrees_dir: root.join("worktrees"),
        path_var: std::env::var_os("PATH").unwrap_or_default(),
        run_id: None,
        stdin_is_tty: true,
        aaf_env: Vec::new(),
    };
    fs::create_dir_all(&ctx.data_dir).unwrap();
    ctx
}

fn insert_review_task(ctx: &Ctx, id: &str, repo_root: &Path) {
    let db = Db::open(&ctx.data_dir.join("aaf.db")).unwrap();
    let mut task = Task::new_root(
        id,
        repo_root.to_string_lossy(),
        "Task whose repo was removed",
        ImmutableSpec {
            description: "desc".into(),
            acceptance: vec!["ok".into()],
        },
        Priority::P1,
        1_700_000_000,
    );
    task.state = TaskState::Review;
    db.insert_task(&task).unwrap();
}

fn handover_missing(ctx: &Ctx, id: &str) -> Vec<String> {
    let args = HandoverArgs {
        task_id: Some(id.to_string()),
        format: Some(HandoverFormat::Json),
        out: None,
        open: false,
        since: None,
        summary: false,
    };
    let cli = cmd::handover::run(&args, ctx).unwrap();
    cli.fields["handover"]["missing"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

fn cockpit_handover(ctx: &Ctx, id: &str) -> String {
    let script = format!("h {id}\nq\n");
    let mut reader = Cursor::new(script.into_bytes());
    let mut buf: Vec<u8> = Vec::new();
    run_cockpit(ctx, None, false, &mut reader, &mut buf).unwrap();
    String::from_utf8(buf).unwrap()
}

#[test]
fn a_different_cwd_repo_config_cannot_waive_proof_for_a_gone_task_repo() {
    let tmp = TempDir::new().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    // The task's own repository required proof, and is gone now.
    let task_repo = root.join("task-repo");
    repo_with_proof(&task_repo, true);
    // The reviewer stands in an unrelated repository that does not.
    let other = root.join("other-repo");
    repo_with_proof(&other, false);
    let ctx = ctx_in(&root, &other);
    insert_review_task(&ctx, "t-gone", &task_repo);
    fs::remove_dir_all(&task_repo).unwrap();

    let missing = handover_missing(&ctx, "t-gone");
    assert!(
        missing.iter().any(|m| m == PROOF_MISSING),
        "another repository's proof.required = false waived the task's proof: {missing:?}"
    );
    let out = cockpit_handover(&ctx, "t-gone");
    assert!(
        out.contains(PROOF_MISSING),
        "cockpit took another repository's config for the gone task repo:\n{out}"
    );
}

#[test]
fn the_same_repo_config_still_applies_while_the_task_repo_exists() {
    let tmp = TempDir::new().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let repo = root.join("repo");
    repo_with_proof(&repo, false);
    let ctx = ctx_in(&root, &repo);
    insert_review_task(&ctx, "t-here", &repo);

    let missing = handover_missing(&ctx, "t-here");
    assert!(
        !missing.iter().any(|m| m == PROOF_MISSING),
        "the task repo's own proof.required = false was ignored: {missing:?}"
    );
}
