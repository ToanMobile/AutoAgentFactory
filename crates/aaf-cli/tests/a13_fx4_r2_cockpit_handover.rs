#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
//! Audit a13 round 2 (O13-7): `aaf cockpit` `h <id>` loads the repository
//! config the same way `aaf handover` does, including the current-repository
//! fallback once the task's own `repo_root` is gone.

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

#[test]
fn cockpit_handover_uses_the_cwd_repo_config_when_the_task_repo_is_gone() {
    let tmp = TempDir::new().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let repo = root.join("repo");
    fs::create_dir_all(repo.join(".autofactory")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(
        repo.join(".autofactory/config.toml"),
        "[proof]\nrequired = true\n",
    )
    .unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    let ctx = Ctx {
        cwd: repo.clone(),
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
    {
        let db = Db::open(&ctx.data_dir.join("aaf.db")).unwrap();
        let mut task = Task::new_root(
            "t-gone",
            root.join("removed-repo").to_string_lossy(),
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

    let args = HandoverArgs {
        task_id: Some("t-gone".to_string()),
        format: Some(HandoverFormat::Json),
        out: None,
        open: false,
        since: None,
        summary: false,
    };
    let cli = cmd::handover::run(&args, &ctx).unwrap();
    let missing: Vec<String> = cli.fields["handover"]["missing"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(
        missing.iter().any(|m| m == PROOF_MISSING),
        "aaf handover ignored the cwd repo config: {missing:?}"
    );

    let mut reader = Cursor::new("h t-gone\nq\n".as_bytes());
    let mut buf: Vec<u8> = Vec::new();
    run_cockpit(&ctx, None, false, &mut reader, &mut buf).unwrap();
    let out = String::from_utf8(buf).unwrap();
    for item in &missing {
        assert!(
            out.contains(item.as_str()),
            "cockpit readiness differs from aaf handover (missing {item:?}):\n{out}"
        );
    }
}
