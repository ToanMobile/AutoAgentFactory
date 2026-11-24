//! `aaf board undo <id>` and `aaf board steer <id> <note>`: human-only rollback
//! to the latest pre-turn snapshot and spec-preserving steering notes.

use std::io::{BufRead, Write};

use aaf_domain::ports::Clock;
use aaf_engine::factory::adapters::SystemClock;
use aaf_engine::factory::snapshots::{preview_latest_undo, steer_task, undo_latest_turn};
use serde_json::json;

use super::store::{err, open_db, refuse_inside_agent_run, repo_root};
use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput, CmdResult};

#[derive(Debug, clap::Args)]
pub struct UndoArgs {
    /// Task id whose worktree should be rolled back to its latest pre-turn snapshot.
    pub id: String,
    /// Skip the interactive `[y/N]` confirmation prompt (still refused inside agent runs).
    #[arg(short = 'y', long)]
    pub yes: bool,
}

pub fn undo_args(ctx: &Ctx, args: &UndoArgs) -> CmdResult {
    if args.yes {
        return undo_confirmed(ctx, &args.id);
    }
    undo(ctx, &args.id)
}

pub fn undo(ctx: &Ctx, id: &str) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf board undo") {
        return Ok(refused);
    }
    if !ctx.stdin_is_tty {
        return Ok(CmdOutput::policy(
            "refused",
            "refused: stdin is not a terminal: undo must be confirmed by a person (or pass --yes)",
        )
        .field("task_id", json!(id))
        .field("undone", json!(false)));
    }
    let stdin = std::io::stdin();
    let mut stderr = std::io::stderr();
    run_undo_with_io(ctx, id, &mut stdin.lock(), &mut stderr)
}

fn undo_confirmed(ctx: &Ctx, task_id: &str) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf board undo") {
        return Ok(refused);
    }
    let repo = repo_root(ctx)?;
    let db = open_db(ctx)?;
    let _preview = preview_latest_undo(&db, &repo, task_id).map_err(CmdError::new)?;
    let now = SystemClock.now();
    let snap = undo_latest_turn(&db, &ctx.data_dir, &repo, &ctx.worktrees_dir, task_id, now)
        .map_err(CmdError::new)?;

    Ok(CmdOutput::ok(
        "ok",
        format!(
            "{task_id} undone to {} (ref {}); state is ready",
            snap.commit_oid, snap.ref_name
        ),
    )
    .field("task_id", json!(task_id))
    .field("undone", json!(true))
    .field("commit_oid", json!(snap.commit_oid))
    .field("ref_name", json!(snap.ref_name))
    .field("state", json!("ready")))
}

pub fn run_undo_with_io<R: BufRead, W: Write>(
    ctx: &Ctx,
    task_id: &str,
    reader: &mut R,
    writer: &mut W,
) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf board undo") {
        return Ok(refused);
    }
    let repo = repo_root(ctx)?;
    let db = open_db(ctx)?;
    let preview = preview_latest_undo(&db, &repo, task_id).map_err(CmdError::new)?;

    writeln!(
        writer,
        "undo {task_id} -> commit {} (ref {})",
        preview.commit_oid, preview.ref_name
    )
    .map_err(|e| err("stderr", e))?;
    write!(writer, "Confirm undo [y/N]: ").map_err(|e| err("stderr", e))?;
    writer.flush().map_err(|e| err("stderr", e))?;

    let mut answer = String::new();
    reader.read_line(&mut answer).map_err(|e| err("stdin", e))?;
    let confirmed = matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes");
    if !confirmed {
        return Ok(
            CmdOutput::policy("aborted", format!("undo aborted for {task_id}"))
                .field("task_id", json!(task_id))
                .field("undone", json!(false)),
        );
    }

    let now = SystemClock.now();
    let snap = undo_latest_turn(&db, &ctx.data_dir, &repo, &ctx.worktrees_dir, task_id, now)
        .map_err(CmdError::new)?;

    Ok(CmdOutput::ok(
        "ok",
        format!(
            "{task_id} undone to {} (ref {}); state is ready",
            snap.commit_oid, snap.ref_name
        ),
    )
    .field("task_id", json!(task_id))
    .field("undone", json!(true))
    .field("commit_oid", json!(snap.commit_oid))
    .field("ref_name", json!(snap.ref_name))
    .field("state", json!("ready")))
}

pub fn steer(ctx: &Ctx, id: &str, note: &str) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf board steer") {
        return Ok(refused);
    }
    let db = open_db(ctx)?;
    let now = SystemClock.now();
    let stored = steer_task(&db, id, note, now).map_err(CmdError::new)?;
    Ok(CmdOutput::ok("ok", format!("{id}: {stored}"))
        .field("task_id", json!(id))
        .field("note", json!(stored)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::io::Cursor;
    use std::path::Path;
    use std::process::Command;

    use aaf_domain::ports::TaskStore;
    use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
    use aaf_engine::factory::snapshots::create_turn_snapshot_ref;
    use aaf_engine::git::worktree;

    use super::*;
    use crate::output::Exit;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn setup_ctx(stdin_is_tty: bool, run_id: Option<&str>) -> (tempfile::TempDir, Ctx, String) {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let repo = root.join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        for (k, v) in [
            ("user.name", "aaf test"),
            ("user.email", "aaf@example.invalid"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", "/dev/null"),
        ] {
            git(&repo, &["config", k, v]);
        }
        fs::write(repo.join("a.txt"), "v1\n").unwrap();
        git(&repo, &["add", "a.txt"]);
        git(&repo, &["commit", "-q", "-m", "init"]);

        let data_dir = root.join("data");
        let worktrees_dir = root.join("worktrees");
        let ctx = Ctx {
            cwd: repo.clone(),
            home: root.join("home"),
            data_dir: data_dir.clone(),
            config_dir: root.join("config"),
            worktrees_dir: worktrees_dir.clone(),
            path_var: OsString::new(),
            run_id: run_id.map(str::to_string),
            stdin_is_tty,
            aaf_env: Vec::new(),
        };

        let db = open_db(&ctx).unwrap();
        let mut task = Task::new_root(
            "t-undo1",
            repo.to_string_lossy(),
            "Undo test",
            ImmutableSpec {
                description: "keep spec".to_string(),
                acceptance: vec!["ok".to_string()],
            },
            Priority::P2,
            100,
        );
        task.state = TaskState::Review;
        db.insert_task(&task).unwrap();

        let wt = worktree::create(&repo, &worktrees_dir, "t-undo1", "main").unwrap();
        let (ref_name, oid) =
            create_turn_snapshot_ref(&repo, &wt.path, "t-undo1", "run-1", 200).unwrap();
        db.insert_snapshot("t-undo1", "run-1", &ref_name, &oid, 200)
            .unwrap();
        fs::write(wt.path.join("a.txt"), "v2\n").unwrap();
        git(&wt.path, &["add", "a.txt"]);
        git(&wt.path, &["commit", "-q", "-m", "turn 1"]);
        (tmp, ctx, oid)
    }

    #[test]
    fn undo_refuses_non_tty_and_agent_run() {
        let (_tmp, ctx, _) = setup_ctx(false, None);
        let out = undo(&ctx, "t-undo1").unwrap();
        assert_eq!(out.exit, Exit::Policy);

        let (_tmp2, ctx_agent, _) = setup_ctx(true, Some("run-99"));
        let out_agent = undo(&ctx_agent, "t-undo1").unwrap();
        assert_eq!(out_agent.exit, Exit::Policy);
    }

    #[test]
    fn run_undo_with_io_aborts_on_no_and_succeeds_on_yes() {
        let (_tmp, ctx, oid) = setup_ctx(true, None);
        let mut out_buf = Vec::new();
        let mut no_in = Cursor::new("n\n");
        let aborted = run_undo_with_io(&ctx, "t-undo1", &mut no_in, &mut out_buf).unwrap();
        assert_eq!(aborted.exit, Exit::Policy);
        assert_eq!(aborted.fields["undone"], json!(false));
        let prompt = String::from_utf8(out_buf).unwrap();
        assert!(prompt.contains(&oid));
        assert!(prompt.contains("Confirm undo [y/N]: "));

        let mut out_buf2 = Vec::new();
        let mut yes_in = Cursor::new("yes\n");
        let ok = run_undo_with_io(&ctx, "t-undo1", &mut yes_in, &mut out_buf2).unwrap();
        assert_eq!(ok.exit, Exit::Ok);
        assert_eq!(ok.fields["undone"], json!(true));
        assert_eq!(ok.fields["commit_oid"], json!(oid));
        assert_eq!(ok.fields["state"], json!("ready"));
    }

    #[test]
    fn steer_appends_note_and_refuses_agent_run() {
        let (_tmp, ctx, _) = setup_ctx(false, None);
        let out = steer(&ctx, "t-undo1", "avoid allocations").unwrap();
        assert_eq!(out.exit, Exit::Ok);
        assert_eq!(out.fields["note"], json!("steer: avoid allocations"));

        let (_tmp2, ctx_agent, _) = setup_ctx(true, Some("run-99"));
        let refused = steer(&ctx_agent, "t-undo1", "sneak").unwrap();
        assert_eq!(refused.exit, Exit::Policy);
    }
}
