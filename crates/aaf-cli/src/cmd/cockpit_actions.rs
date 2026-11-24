//! Mutating/approval actions and interactive sub-prompts for `aaf cockpit`.
//!
//! Every mutating action requires explicit `y/N` confirmation (default NO on
//! empty input or EOF) before opening a read-write DB handle or mutating state.

use std::io::{BufRead, ErrorKind, Write};
use std::path::Path;

use aaf_domain::ports::{Clock, RunStore, TaskStore};
use aaf_domain::task::{Task, TaskState};
use aaf_engine::db::{Db, RoDbCache, TurnSnapshot, private};
use aaf_engine::executors::stream_log::read_tail_lines;
use aaf_engine::factory::adapters::SystemClock;
use aaf_engine::factory::snapshots::{preview_latest_undo, steer_task, undo_latest_turn};
use aaf_engine::factory::unfreeze;
use aaf_engine::gate::GateError;
use aaf_engine::git::worktree::validate_task_id;
use aaf_engine::watch::sanitize_display as san;

use super::task_ops::{
    ApproveOpError, HeadLockEvalError, TaskOpError, approve_and_regate, eval_tree_head_lock,
    parse_rework_reason, resolve_task_tree, rework_task, unblock_task,
};
use crate::ctx::Ctx;
use crate::output::CmdError;

pub(crate) const DB_FILE: &str = "aaf.db";
const TAIL_MAX_LINES: usize = 50;

pub(crate) fn io_err(context: &str, err: impl std::fmt::Display) -> CmdError {
    CmdError::new(format!("{context}: {err}"))
}

pub(crate) fn wln(w: &mut dyn Write, line: &str) -> Result<(), CmdError> {
    writeln!(w, "{line}").map_err(|e| io_err("write", e))
}

pub(crate) fn open_db_ro(ro: &RoDbCache) -> Result<Option<Db>, CmdError> {
    ro.open().map_err(|e| io_err("database", e))
}

fn open_db_rw(ctx: &Ctx) -> Result<Db, CmdError> {
    private::create_dir(&ctx.data_dir)
        .map_err(|e| io_err(&format!("data dir {}", ctx.data_dir.display()), e))?;
    Db::open(&ctx.data_dir.join(DB_FILE)).map_err(|e| io_err("database", e))
}

/// Prompts for explicit confirmation, defaulting to `false` on empty input,
/// EOF, or anything other than `y` / `yes` (case-insensitive).
pub(crate) fn confirm(
    prompt: &str,
    reader: &mut dyn BufRead,
    writer: &mut dyn Write,
) -> Result<bool, CmdError> {
    write!(writer, "{prompt}").map_err(|e| io_err("write", e))?;
    writer.flush().map_err(|e| io_err("flush", e))?;
    let mut answer = String::new();
    if reader
        .read_line(&mut answer)
        .map_err(|e| io_err("read", e))?
        == 0
    {
        return Ok(false);
    }
    let t = answer.trim();
    Ok(t.eq_ignore_ascii_case("y") || t.eq_ignore_ascii_case("yes"))
}

pub(crate) fn load_ro_task(
    ro: &RoDbCache,
    id: &str,
    action: &str,
    w: &mut dyn Write,
) -> Result<Option<(Db, Task)>, CmdError> {
    if let Err(e) = validate_task_id(id) {
        wln(w, &format!("invalid task id: {}", san(&e.to_string())))?;
        return Ok(None);
    }
    let Some(db) = open_db_ro(ro)? else {
        wln(w, "database not found")?;
        return Ok(None);
    };
    match db.get_task(id) {
        Ok(task) => Ok(Some((db, task))),
        Err(e) => {
            wln(w, &format!("{action}: {}", san(&e.to_string())))?;
            Ok(None)
        }
    }
}

pub(crate) fn run_tail_view(
    ctx: &Ctx,
    ro: &RoDbCache,
    id: &str,
    reader: &mut dyn BufRead,
    w: &mut dyn Write,
) -> Result<(), CmdError> {
    if id.is_empty() {
        return wln(w, "usage: tail <id>");
    }
    let Some((db, _)) = load_ro_task(ro, id, "tail", w)? else {
        return Ok(());
    };
    let runs = match db.runs_for_task(id) {
        Ok(r) => r,
        Err(e) => return wln(w, &format!("tail: {}", san(&e.to_string()))),
    };
    drop(db);
    let Some(latest) = runs
        .iter()
        .max_by_key(|r| (r.started_at, r.id.as_str()))
        .map(|r| r.id.clone())
    else {
        return wln(w, &format!("no runs found for {}", san(id)));
    };
    let (safe_id, safe_run) = (san(id), san(&latest));
    loop {
        wln(w, &format!("== tail {safe_id} (run {safe_run}) =="))?;
        match read_tail_lines(&ctx.data_dir, &latest, TAIL_MAX_LINES) {
            Ok(lines) if lines.is_empty() => wln(w, "  (no stream lines)")?,
            Ok(lines) => {
                for l in &lines {
                    wln(w, &format!("  {}", san(l)))?;
                }
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                wln(w, &format!("  (no stream log for run {safe_run})"))?;
            }
            Err(e) => wln(w, &format!("  stream error: {}", san(&e.to_string())))?,
        }
        write!(w, "tail (Enter=refresh, q=back)> ").map_err(|e| io_err("write", e))?;
        w.flush().map_err(|e| io_err("flush", e))?;
        let mut line = String::new();
        let n = reader.read_line(&mut line).map_err(|e| io_err("read", e))?;
        if n == 0 || matches!(line.trim(), "q" | "quit") {
            break;
        }
    }
    Ok(())
}

/// The cockpit's wording of a failed head-lock evaluation.
fn eval_error_line(e: &HeadLockEvalError) -> String {
    match e {
        HeadLockEvalError::Config(m) => format!("approve config: {}", san(m)),
        HeadLockEvalError::Base(m) => format!("approve base: {}", san(m)),
        HeadLockEvalError::Task(m) => format!("approve: {}", san(m)),
    }
}

pub(crate) fn action_approve(
    ctx: &Ctx,
    ro: &RoDbCache,
    id: &str,
    reviewer_arg: Option<&str>,
    reader: &mut dyn BufRead,
    w: &mut dyn Write,
) -> Result<(), CmdError> {
    let Some((db, task)) = load_ro_task(ro, id, "approve", w)? else {
        return Ok(());
    };
    let target_tree = resolve_task_tree(&db, &task, ctx);
    drop(db);

    let report = match eval_tree_head_lock(ctx, &target_tree) {
        Ok((_, report)) => report,
        Err(e) => return wln(w, &eval_error_line(&e)),
    };
    let safe_id = san(id);
    if report.locked_changes.is_empty() {
        return wln(
            w,
            &format!("no locked file is changed for {safe_id}; nothing to approve"),
        );
    }
    wln(
        w,
        &format!(
            "locked changes for {safe_id} (lock_fp={}):",
            san(&report.lock_fp)
        ),
    )?;
    for ch in &report.locked_changes {
        wln(w, &format!("  {} {}", ch.change.letter(), san(&ch.path)))?;
    }
    if !confirm(&format!("Confirm approve {safe_id} [y/N]: "), reader, w)? {
        return wln(w, "cancelled");
    }
    let reviewer = reviewer_arg
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("cockpit");
    let db = open_db_rw(ctx)?;
    match approve_and_regate(
        &db,
        ctx,
        id,
        reviewer,
        Some(&report.lock_fp),
        "aaf cockpit approve",
        "human",
        SystemClock.now(),
    ) {
        Ok(out) => {
            let suffix = match out.transitioned_to {
                Some(state) => format!(" -> {}", state.as_str()),
                None => String::new(),
            };
            wln(
                w,
                &format!(
                    "approved {safe_id} by {} (lock_fp={}){suffix}",
                    san(&out.approval.reviewer),
                    san(&out.approval.lock_fp)
                ),
            )
        }
        Err(ApproveOpError::Gate(GateError::NothingLocked)) => wln(
            w,
            &format!("refused: no locked file is changed for {safe_id}; nothing approved"),
        ),
        Err(ApproveOpError::Gate(GateError::LockFpChanged)) => wln(
            w,
            &format!(
                "refused: lock_fp mismatch for {safe_id}: locked diff changed since preview; run approve again"
            ),
        ),
        Err(e) => wln(w, &format!("approve failed: {}", san(&e.to_string()))),
    }
}

pub(crate) fn action_rework(
    ctx: &Ctx,
    ro: &RoDbCache,
    id: &str,
    rest: &str,
    reader: &mut dyn BufRead,
    w: &mut dyn Write,
) -> Result<(), CmdError> {
    let (rerun, reason) = parse_rework_reason(rest);
    if reason.is_empty() {
        return wln(w, "usage: rework <id> [--rerun] <reason>");
    }
    let Some((db_ro, task)) = load_ro_task(ro, id, "rework", w)? else {
        return Ok(());
    };
    drop(db_ro);
    let safe_id = san(id);
    if task.state != TaskState::Review {
        return wln(
            w,
            &format!("refused: {safe_id} is {}, not review", task.state.as_str()),
        );
    }
    let target = if rerun {
        TaskState::Ready
    } else {
        TaskState::Gating
    };
    wln(
        w,
        &format!("rework {safe_id} -> {}: {}", target.as_str(), san(reason)),
    )?;
    if !confirm(&format!("Confirm rework {safe_id} [y/N]: "), reader, w)? {
        return wln(w, "cancelled");
    }
    let db = open_db_rw(ctx)?;
    match rework_task(
        &db,
        ctx,
        id,
        reason,
        rerun,
        "aaf cockpit rework",
        "human",
        SystemClock.now(),
    ) {
        Ok(out) => wln(
            w,
            &format!("{safe_id} sent back to {}", out.target.as_str()),
        ),
        Err(TaskOpError::WrongState { actual, .. }) => wln(
            w,
            &format!("refused: {safe_id} is {}, not review", actual.as_str()),
        ),
        Err(TaskOpError::Port(e)) => Err(io_err("rework", e)),
    }
}

pub(crate) fn action_steer(
    ctx: &Ctx,
    id: &str,
    note: &str,
    reader: &mut dyn BufRead,
    w: &mut dyn Write,
) -> Result<(), CmdError> {
    if let Err(e) = validate_task_id(id) {
        return wln(w, &format!("invalid task id: {}", san(&e.to_string())));
    }
    let note = note.trim();
    if note.is_empty() {
        return wln(w, "usage: steer <id> <note>");
    }
    let safe_id = san(id);
    wln(w, &format!("steer {safe_id}: {}", san(note)))?;
    if !confirm(&format!("Confirm steer {safe_id} [y/N]: "), reader, w)? {
        return wln(w, "cancelled");
    }
    let db = open_db_rw(ctx)?;
    match steer_task(&db, id, note, SystemClock.now()) {
        Ok(recorded) => wln(w, &format!("steered {safe_id}: {}", san(&recorded))),
        Err(e) => wln(w, &format!("steer failed: {}", san(&e))),
    }
}

pub(crate) fn action_undo(
    ctx: &Ctx,
    ro: &RoDbCache,
    id: &str,
    reader: &mut dyn BufRead,
    w: &mut dyn Write,
) -> Result<(), CmdError> {
    let Some((db_ro, task)) = load_ro_task(ro, id, "undo", w)? else {
        return Ok(());
    };
    let snap: TurnSnapshot = match preview_latest_undo(&db_ro, Path::new(&task.repo_root), id) {
        Ok(s) => s,
        Err(e) => return wln(w, &format!("undo preview failed: {}", san(&e))),
    };
    drop(db_ro);

    let (safe_id, safe_oid, safe_ref) = (san(id), san(&snap.commit_oid), san(&snap.ref_name));
    wln(
        w,
        &format!("undo preview for {safe_id}:\n  ref: {safe_ref}\n  commit: {safe_oid}"),
    )?;
    if !confirm(
        &format!("Confirm undo {safe_id} to {safe_oid} [y/N]: "),
        reader,
        w,
    )? {
        return wln(w, "cancelled");
    }
    let db = open_db_rw(ctx)?;
    match undo_latest_turn(
        &db,
        &ctx.data_dir,
        Path::new(&task.repo_root),
        &ctx.worktrees_dir,
        id,
        SystemClock.now(),
    ) {
        Ok(u) => wln(
            w,
            &format!(
                "undid {safe_id} to {} ({})",
                san(&u.commit_oid),
                san(&u.ref_name)
            ),
        ),
        Err(e) => wln(w, &format!("undo failed: {}", san(&e))),
    }
}

pub(crate) fn action_unblock(
    ctx: &Ctx,
    ro: &RoDbCache,
    id: &str,
    reader: &mut dyn BufRead,
    w: &mut dyn Write,
) -> Result<(), CmdError> {
    let Some((db_ro, task)) = load_ro_task(ro, id, "unblock", w)? else {
        return Ok(());
    };
    drop(db_ro);
    let safe_id = san(id);
    if task.state != TaskState::Blocked {
        return wln(
            w,
            &format!("refused: {safe_id} is {}, not blocked", task.state.as_str()),
        );
    }
    if !confirm(&format!("Confirm unblock {safe_id} [y/N]: "), reader, w)? {
        return wln(w, "cancelled");
    }
    let db = open_db_rw(ctx)?;
    match unblock_task(&db, id, "aaf cockpit unblock", "human", SystemClock.now()) {
        Ok(_) => wln(w, &format!("{safe_id} is ready")),
        Err(TaskOpError::WrongState { actual, .. }) => wln(
            w,
            &format!("refused: {safe_id} is {}, not blocked", actual.as_str()),
        ),
        Err(TaskOpError::Port(e)) => Err(io_err("unblock", e)),
    }
}

pub(crate) fn action_unfreeze(
    ctx: &Ctx,
    reader: &mut dyn BufRead,
    w: &mut dyn Write,
) -> Result<(), CmdError> {
    if !confirm("Confirm unfreeze factory [y/N]: ", reader, w)? {
        return wln(w, "cancelled");
    }
    let db = open_db_rw(ctx)?;
    unfreeze(&db, &SystemClock).map_err(|e| io_err("unfreeze", e))?;
    wln(w, "factory is not frozen")
}
