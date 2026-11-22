#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
//! Phase 3 v3.2 tests for the synchronous interactive TTY cockpit (`aaf cockpit`).

#[path = "cli_support.rs"]
mod support;

#[path = "../src/ctx.rs"]
mod ctx;
#[path = "../src/output.rs"]
mod output;

#[path = "../src/cmd"]
mod cmd {
    pub mod cockpit;
    pub mod cockpit_actions;
}

use std::fs;
use std::io::Cursor;
use std::path::Path;
use std::process::{Command, Stdio};

use aaf_domain::blocked::BlockedReason;
use aaf_domain::config::RepoConfig;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::gate::LockStatus;
use aaf_domain::ports::{EventSink, KvStore, META_FACTORY_FROZEN, RunStore, TaskStore};
use aaf_domain::run::{RunRecord, RunStatus};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::executors::stream_log::StreamLog;
use aaf_engine::factory::is_frozen;
use aaf_engine::factory::snapshots::create_turn_snapshot_ref;
use aaf_engine::gate::{GIT_TIMEOUT, base, head_lock};
use aaf_engine::git::worktree;
use serde_json::json;
use support::Sandbox;
use tempfile::TempDir;

use crate::cmd::cockpit::run_cockpit;
use crate::ctx::Ctx;
use crate::output::Exit;

fn git(repo: &Path, args: &[&str]) -> String {
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
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn fixture_ctx() -> (TempDir, Ctx) {
    let tmp = TempDir::new().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let repo = root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("README.md"), "cockpit fixture\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
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
    fs::create_dir_all(&ctx.data_dir).unwrap();
    (tmp, ctx)
}

fn spec(desc: &str) -> ImmutableSpec {
    ImmutableSpec {
        description: desc.to_string(),
        acceptance: vec!["passes verification".to_string()],
    }
}

fn run_session(ctx: &Ctx, input: &str) -> (Exit, String) {
    let mut reader = Cursor::new(input.as_bytes());
    let mut out_buf: Vec<u8> = Vec::new();
    let res = run_cockpit(ctx, None, false, &mut reader, &mut out_buf).unwrap();
    let text = String::from_utf8(out_buf).unwrap();
    (res.exit, text)
}

#[test]
fn cockpit_binary_refuses_without_tty_and_inside_aaf_run_id() {
    let sb = Sandbox::new();

    // Non-TTY stdin (Stdio::null()) -> policy exit 2
    let (code, out) = sb.json(&["cockpit"]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["ok"], json!(false));
    assert_eq!(out["command"], json!("cockpit"));
    assert_eq!(out["status"], json!("refused"));

    let plain = sb.aaf(&["cockpit"]);
    assert_eq!(plain.status.code(), Some(2));
    let plain_out = String::from_utf8_lossy(&plain.stdout);
    assert!(plain_out.contains("interactive TTY"), "{plain_out}");

    // Inside agent run (AAF_RUN_ID=run-1) -> policy exit 2
    let agent = sb
        .command(&["cockpit", "--json"])
        .env("AAF_RUN_ID", "run-1")
        .output()
        .unwrap();
    assert_eq!(agent.status.code(), Some(2));
    let agent_json: serde_json::Value = serde_json::from_slice(&agent.stdout).unwrap();
    assert_eq!(agent_json["ok"], json!(false));
    assert_eq!(agent_json["status"], json!("refused"));
}

#[test]
fn cockpit_renders_four_panels_sanitizes_escapes_and_handles_readonly_commands() {
    let (_tmp, ctx) = fixture_ctx();
    let repo = ctx.cwd.to_string_lossy().into_owned();
    let now = 1_700_000_000;
    let db = Db::open(&ctx.data_dir.join("aaf.db")).unwrap();

    let mut root = Task::new_root(
        "t-root",
        &repo,
        "\x1b[31mEvil\x1b[0m \x1b]0;pwn\x07title",
        spec("Root task \x1b[33mspec\x1b[0m"),
        Priority::P0,
        now,
    );
    root.state = TaskState::Blocked;
    root.scope_paths = vec!["crates/app".to_string()];
    root.blocked_reason = Some(BlockedReason::NeedsHuman {
        why: "\x1bP1$r\x1b\\\x1b[2Jbad-reason\u{009b}31m".to_string(),
    });
    db.insert_task(&root).unwrap();
    db.append_context_note("t-root", "\x1b[32mctx-note\x1b[0m", now + 1)
        .unwrap();
    db.emit(
        &FactoryEvent::new(
            EventKind::GateResult,
            json!({
                "status": "CLEAR",
                "tree_fp": "git-tree:abc123",
                "commands_passed": 1,
                "commands_total": 1,
                "static_critical": 0,
                "static_warn": 0,
                "head_lock_status": "CLEAR"
            })
            .to_string(),
            now + 2,
        )
        .for_task("t-root"),
    )
    .unwrap();

    let mut child = Task::new_root(
        "t-child",
        &repo,
        "Child task",
        spec("Depends on root"),
        Priority::P1,
        now + 3,
    );
    child.state = TaskState::Running;
    db.insert_task(&child).unwrap();
    db.add_dep("t-child", "t-root").unwrap();
    db.insert_run(&RunRecord {
        id: "run-live-1".to_string(),
        task_id: "t-child".to_string(),
        account_id: None,
        executor: "claude".to_string(),
        worktree_path: repo,
        branch: "aaf/t-child".to_string(),
        status: RunStatus::Running,
        exit_code: None,
        tokens_in: 10,
        tokens_out: 20,
        started_at: now + 4,
        ended_at: None,
    })
    .unwrap();
    drop(db);

    let mut log = StreamLog::open(Some(&ctx.data_dir), "run-live-1", Vec::new()).unwrap();
    log.write_event(
        "tool_use",
        Some("Bash"),
        Some("\x1b[31mcargo test\x1b[0m \x1b]0;spoof\x07"),
    );
    drop(log);

    let script = "\nr\nd t-root\nt t-root\nh t-root\ng\ntail t-child\n\nq\nq\n";
    let (exit, out) = run_session(&ctx, script);
    assert_eq!(exit, Exit::Ok);

    // Zero ANSI / OSC / C1 escapes leaked anywhere in output.
    assert!(!out.contains('\x1b'), "ESC leaked: {out:?}");
    assert!(!out.contains('\x07'), "BEL leaked: {out:?}");
    assert!(
        !out.chars().any(|c| ('\u{0080}'..='\u{009F}').contains(&c)),
        "C1 control leaked: {out:?}"
    );

    // All 4 panels rendered, and initial + Enter + `r` rendered 3 times.
    assert_eq!(out.matches("== aaf cockpit").count(), 3, "{out}");
    assert!(out.contains("[1. Tasks / Board]"), "{out}");
    assert!(out.contains("[2. Active Runs & Heavy Slots]"), "{out}");
    assert!(out.contains("[3. Accounts & Cooldowns]"), "{out}");
    assert!(
        out.contains("[4. Recent Events / Alerts & Commands]"),
        "{out}"
    );
    assert!(out.contains("Evil title"), "{out}");
    assert!(out.contains("crates/app"), "{out}");
    assert!(out.contains("run run-live-1"), "{out}");

    // Read-only commands: d, t, h, g, tail
    assert!(out.contains("== task t-root"), "{out}");
    assert!(out.contains("ctx-note"), "{out}");
    assert!(out.contains("== events for t-root =="), "{out}");
    assert!(out.contains("gate_result"), "{out}");
    assert!(out.contains("CHƯA XONG") || out.contains("XONG"), "{out}");
    assert!(out.contains("== DAG =="), "{out}");
    assert!(out.contains("critical path: t-root -> t-child"), "{out}");
    assert!(out.contains("== tail t-child (run run-live-1) =="), "{out}");
    assert!(out.contains("cargo test"), "{out}");
    assert_eq!(
        out.matches("tail (Enter=refresh, q=back)> ").count(),
        2,
        "{out}"
    );
}

#[test]
fn cockpit_steer_unblock_unfreeze_rework_require_confirmation() {
    let (_tmp, ctx) = fixture_ctx();
    let repo = ctx.cwd.to_string_lossy().into_owned();
    let now = 1_700_000_000;
    {
        let db = Db::open(&ctx.data_dir.join("aaf.db")).unwrap();
        let mut blk = Task::new_root(
            "t-blk",
            &repo,
            "Plan gated task",
            spec("Implement feature"),
            Priority::P1,
            now,
        );
        blk.state = TaskState::Blocked;
        blk.blocked_reason = Some(BlockedReason::NeedsHuman {
            why: "plan approval required: review plan".to_string(),
        });
        db.insert_task(&blk).unwrap();
        db.emit(
            &FactoryEvent::new(
                EventKind::PlanGenerated,
                json!({ "plan": "Step 1: edit lib.rs" }).to_string(),
                now + 1,
            )
            .for_task("t-blk"),
        )
        .unwrap();

        let mut rev = Task::new_root(
            "t-rev",
            &repo,
            "Review task",
            spec("Verify review"),
            Priority::P2,
            now + 2,
        );
        rev.state = TaskState::Review;
        db.insert_task(&rev).unwrap();
        db.set_meta(META_FACTORY_FROZEN, "1").unwrap();
    }

    // Default N / empty Enter / explicit `n` -> ZERO DB changes.
    let decline =
        "steer t-blk use helper\n\nunblock t-blk\nn\nunfreeze\n\nrework t-rev fix edge\nn\nq\n";
    let (exit, out) = run_session(&ctx, decline);
    assert_eq!(exit, Exit::Ok);
    assert!(out.contains("Confirm steer t-blk [y/N]: "), "{out}");
    assert!(out.contains("Confirm unblock t-blk [y/N]: "), "{out}");
    assert!(out.contains("Confirm unfreeze factory [y/N]: "), "{out}");
    assert!(out.contains("Confirm rework t-rev [y/N]: "), "{out}");
    {
        let db = Db::open_read_only(&ctx.data_dir.join("aaf.db")).unwrap();
        let blk = db.get_task("t-blk").unwrap();
        assert_eq!(blk.state, TaskState::Blocked);
        assert!(blk.context_notes.is_empty());
        let rev = db.get_task("t-rev").unwrap();
        assert_eq!(rev.state, TaskState::Review);
        assert!(rev.context_notes.is_empty());
        assert!(is_frozen(&db).unwrap());
    }

    // Explicit `y` executes every mutation cleanly.
    let accept =
        "steer t-blk use helper\ny\nunblock t-blk\ny\nunfreeze\ny\nrework t-rev fix edge\ny\nq\n";
    let (exit, out) = run_session(&ctx, accept);
    assert_eq!(exit, Exit::Ok);
    assert!(out.contains("steered t-blk"), "{out}");
    assert!(out.contains("t-blk is ready"), "{out}");
    assert!(out.contains("factory is not frozen"), "{out}");
    assert!(out.contains("t-rev sent back to gating"), "{out}");
    {
        let db = Db::open_read_only(&ctx.data_dir.join("aaf.db")).unwrap();
        let blk = db.get_task("t-blk").unwrap();
        assert_eq!(blk.state, TaskState::Ready);
        assert!(blk.context_notes.iter().any(|n| n == "steer: use helper"));
        assert!(
            blk.context_notes
                .iter()
                .any(|n| n.starts_with("plan_approved:") && n.contains("Step 1: edit lib.rs"))
        );
        let rev = db.get_task("t-rev").unwrap();
        assert_eq!(rev.state, TaskState::Gating);
        assert!(rev.context_notes.iter().any(|n| n == "rework: fix edge"));
        assert!(!is_frozen(&db).unwrap());
    }
}

#[test]
fn cockpit_undo_previews_commit_oid_and_rolls_back_only_on_confirm() {
    let (_tmp, ctx) = fixture_ctx();
    let repo = ctx.cwd.clone();
    let wt = worktree::create(&repo, &ctx.worktrees_dir, "t-undo", "main").unwrap();

    fs::write(wt.path.join("app.txt"), "v1\n").unwrap();
    git(&wt.path, &["add", "-A"]);
    git(&wt.path, &["commit", "-q", "-m", "v1"]);
    let (ref_name, v1_oid) =
        create_turn_snapshot_ref(&repo, &wt.path, "t-undo", "run-u1", 1_700_000_010).unwrap();

    fs::write(wt.path.join("app.txt"), "v2\n").unwrap();
    git(&wt.path, &["add", "-A"]);
    git(&wt.path, &["commit", "-q", "-m", "v2"]);
    let v2_oid = git(&wt.path, &["rev-parse", "HEAD"]);
    fs::write(wt.path.join("untracked.tmp"), "junk\n").unwrap();

    {
        let db = Db::open(&ctx.data_dir.join("aaf.db")).unwrap();
        let mut task = Task::new_root(
            "t-undo",
            repo.to_string_lossy(),
            "Undoable task",
            spec("Test undo"),
            Priority::P1,
            1_700_000_000,
        );
        task.state = TaskState::Review;
        db.insert_task(&task).unwrap();
        db.insert_snapshot("t-undo", "run-u1", &ref_name, &v1_oid, 1_700_000_010)
            .unwrap();
    }

    // Empty Enter defaults to N -> no git reset and no DB change.
    let (exit, out) = run_session(&ctx, "undo t-undo\n\nq\n");
    assert_eq!(exit, Exit::Ok);
    assert!(out.contains(&ref_name), "{out}");
    assert!(
        out.contains(&format!("Confirm undo t-undo to {v1_oid} [y/N]: ")),
        "{out}"
    );
    assert_eq!(git(&wt.path, &["rev-parse", "HEAD"]), v2_oid);
    assert!(wt.path.join("untracked.tmp").exists());
    {
        let db = Db::open_read_only(&ctx.data_dir.join("aaf.db")).unwrap();
        assert_eq!(db.get_task("t-undo").unwrap().state, TaskState::Review);
        assert!(db.latest_snapshot("t-undo").unwrap().is_some());
    }

    // Confirm with `y` -> resets worktree, cleans untracked file, moves to Ready.
    let (exit, out) = run_session(&ctx, "undo t-undo\ny\nq\n");
    assert_eq!(exit, Exit::Ok);
    assert!(out.contains(&format!("undid t-undo to {v1_oid}")), "{out}");
    assert_eq!(git(&wt.path, &["rev-parse", "HEAD"]), v1_oid);
    assert!(!wt.path.join("untracked.tmp").exists());
    {
        let db = Db::open_read_only(&ctx.data_dir.join("aaf.db")).unwrap();
        assert_eq!(db.get_task("t-undo").unwrap().state, TaskState::Ready);
        assert!(db.latest_snapshot("t-undo").unwrap().is_none());
    }
}

#[test]
fn cockpit_approve_displays_locked_files_and_signs_only_on_confirm() {
    let (_tmp, ctx) = fixture_ctx();
    let repo = ctx.cwd.clone();
    fs::create_dir_all(repo.join("tests")).unwrap();
    fs::write(repo.join("tests/it.rs"), "// v1\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "add locked test"]);
    fs::write(repo.join("tests/it.rs"), "// v2\n").unwrap();

    {
        let db = Db::open(&ctx.data_dir.join("aaf.db")).unwrap();
        let mut task = Task::new_root(
            "t-app",
            repo.to_string_lossy(),
            "Approve locked change",
            spec("Approve test edit"),
            Priority::P1,
            1_700_000_000,
        );
        task.state = TaskState::Blocked;
        db.insert_task(&task).unwrap();
    }

    let cfg = RepoConfig::default();
    let base_info = base::resolve(&repo, &cfg.gate.base_ref, GIT_TIMEOUT).unwrap();
    assert_eq!(
        head_lock::evaluate(&ctx.data_dir, &repo, &cfg, &base_info.diff_target).status,
        LockStatus::Blocked
    );

    // Default N -> displays locked file, signs NOTHING.
    let (exit, out) = run_session(&ctx, "approve t-app\n\nq\n");
    assert_eq!(exit, Exit::Ok);
    assert!(out.contains("tests/it.rs"), "{out}");
    assert!(out.contains("Confirm approve t-app [y/N]: "), "{out}");
    assert_eq!(
        head_lock::evaluate(&ctx.data_dir, &repo, &cfg, &base_info.diff_target).status,
        LockStatus::Blocked
    );

    // Confirm with `y` -> signs approval and head-lock becomes Approved.
    let (exit, out) = run_session(&ctx, "approve t-app alice\ny\nq\n");
    assert_eq!(exit, Exit::Ok);
    assert!(out.contains("approved t-app by alice"), "{out}");
    assert_eq!(
        head_lock::evaluate(&ctx.data_dir, &repo, &cfg, &base_info.diff_target).status,
        LockStatus::Approved
    );
}
