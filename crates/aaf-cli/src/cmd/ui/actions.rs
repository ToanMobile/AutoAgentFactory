//! Task mutation and formatting helpers for `aaf ui`.

use aaf_domain::blocked::BlockedReason;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{Clock, EventSink, RunStore, TaskStore};
use aaf_domain::task::TaskState;
use aaf_engine::db::{CancelRefusal, Db};
use aaf_engine::gate::GateError;
use serde_json::{Value, json};

use crate::cmd::task_ops::{
    ApproveOpError, PLAN_APPROVAL_PREFIX, PLAN_DEBATE_STALLED_PREFIX, approve_and_regate,
    emit_human_plan_converged, format_approved_plan_note, latest_plan_info, parse_rework_reason,
    reset_exhausted_budget, rework_task, unblock_task,
};
use crate::ctx::{Ctx, SystemClock};

const RECENT_RUNS_LIMIT: usize = 20;

pub(super) fn do_ready(db: &Db, task_id: &str) -> Result<Value, String> {
    let task = db.get_task(task_id).map_err(s_err)?;
    if task.state != TaskState::Backlog {
        return Err(format!(
            "refused: {task_id} is {}, not backlog",
            task.state.as_str()
        ));
    }
    let now = SystemClock.now();
    db.set_state(task_id, TaskState::Ready, None, now)
        .map_err(s_err)?;
    let payload = json!({
        "from": TaskState::Backlog.as_str(),
        "to": TaskState::Ready.as_str(),
        "why": "aaf ui ready",
        "by": "web-ui",
    });
    db.emit(
        &FactoryEvent::new(EventKind::TaskStateChanged, payload.to_string(), now).for_task(task_id),
    )
    .map_err(s_err)?;
    Ok(json!({ "action": "ready", "task_id": task_id, "state": "ready" }))
}

pub(super) fn do_ready_all(db: &Db, repo: Option<&str>) -> Result<Value, String> {
    let backlog = db
        .list_tasks(repo, Some(TaskState::Backlog))
        .map_err(s_err)?;
    let now = SystemClock.now();
    let mut ids = Vec::with_capacity(backlog.len());
    for t in &backlog {
        db.set_state(&t.id, TaskState::Ready, None, now)
            .map_err(s_err)?;
        let payload = json!({
            "from": TaskState::Backlog.as_str(),
            "to": TaskState::Ready.as_str(),
            "why": "aaf ui ready-all",
            "by": "web-ui",
        });
        db.emit(
            &FactoryEvent::new(EventKind::TaskStateChanged, payload.to_string(), now)
                .for_task(&t.id),
        )
        .map_err(s_err)?;
        ids.push(t.id.clone());
    }
    Ok(json!({ "action": "ready_all", "count": ids.len(), "task_ids": ids }))
}

pub(super) fn do_cancel(db: &Db, task_id: &str, note: &str) -> Result<Value, String> {
    let reason = (!note.trim().is_empty()).then_some(note.trim());
    match db
        .cancel_task_atomic(task_id, false, reason, SystemClock.now())
        .map_err(s_err)?
    {
        Ok(out) => Ok(json!({
            "action": "cancel",
            "task_id": out.task_id,
            "state": "cancelled",
            "cancelled_ids": out.cancelled_ids,
            "affected_dependents": out.affected_dependents,
        })),
        Err(CancelRefusal::ActiveState { state, .. }) => Err(format!(
            "refused: {task_id} is currently {}",
            state.as_str()
        )),
        Err(CancelRefusal::AlreadyTerminal { state, .. }) => {
            Err(format!("refused: {task_id} is already {}", state.as_str()))
        }
        Err(CancelRefusal::RunningRun { run_id, .. }) => {
            Err(format!("refused: {task_id} has active run {run_id}"))
        }
        Err(CancelRefusal::ActiveDescendant { child_id, state }) => Err(format!(
            "refused: child {child_id} is currently {}",
            state.as_str()
        )),
        Err(CancelRefusal::RunningDescendantRun { child_id, run_id }) => {
            Err(format!("refused: child {child_id} has active run {run_id}"))
        }
        Err(CancelRefusal::HasUnfinishedChildren { children }) => Err(format!(
            "refused: {task_id} has unfinished subtasks ({}); cancel via CLI with --cascade",
            children.join(", ")
        )),
    }
}

pub(super) fn do_unblock(db: &Db, task_id: &str, note: &str) -> Result<Value, String> {
    let trimmed = note.trim();
    if trimmed.len() > super::diff_parse::MAX_STEER_NOTE_BYTES {
        return Err(format!(
            "invalid unblock note: exceeds {}-byte limit ({} bytes)",
            super::diff_parse::MAX_STEER_NOTE_BYTES,
            trimmed.len()
        ));
    }
    let now = SystemClock.now();
    unblock_task(db, task_id, "aaf ui unblock", "web-ui", now)
        .map_err(|e| e.to_refused_string())?;
    let recorded_note = if !trimmed.is_empty() {
        Some(aaf_engine::factory::snapshots::steer_task(
            db, task_id, trimmed, now,
        )?)
    } else {
        None
    };
    Ok(json!({
        "action": "unblock",
        "task_id": task_id,
        "state": "ready",
        "note": recorded_note,
    }))
}

pub(super) fn do_unblock_all(db: &Db, repo: Option<&str>) -> Result<Value, String> {
    let blocked = db
        .list_tasks(repo, Some(TaskState::Blocked))
        .map_err(s_err)?;
    let now = SystemClock.now();
    let mut ids = Vec::with_capacity(blocked.len());
    for t in &blocked {
        unblock_task(db, &t.id, "aaf ui unblock-all", "web-ui", now)
            .map_err(|e| e.to_refused_string())?;
        ids.push(t.id.clone());
    }
    Ok(json!({ "action": "unblock_all", "count": ids.len(), "task_ids": ids }))
}

const MAX_DELETE_DONE_BATCH: usize = 500;

pub(super) fn do_delete_done(
    db: &Db,
    repo: Option<&str>,
    task_id: &str,
    note: &str,
) -> Result<Value, String> {
    if !task_id.trim().is_empty() {
        return Err(
            "invalid delete_done: pass task IDs as a JSON array in note, not task_id".into(),
        );
    }
    let trimmed = note.trim();
    if trimmed.is_empty() {
        return Err("invalid delete_done: note must be a non-empty JSON array of task IDs".into());
    }
    let parsed: Vec<String> = serde_json::from_str(trimmed)
        .map_err(|e| format!("invalid delete_done task_ids JSON array: {e}"))?;
    if parsed.is_empty() {
        return Err("invalid delete_done: task_ids array must not be empty".into());
    }
    if parsed.len() > MAX_DELETE_DONE_BATCH {
        return Err(format!(
            "invalid delete_done: batch size {} exceeds maximum {}",
            parsed.len(),
            MAX_DELETE_DONE_BATCH
        ));
    }
    for id in &parsed {
        aaf_engine::git::worktree::validate_task_id(id)
            .map_err(|e| format!("invalid task id '{id}': {e}"))?;
    }
    let out = db
        .archive_terminal_tasks_atomic(&parsed, repo, SystemClock.now())
        .map_err(s_err)?;
    Ok(json!({
        "action": "delete_done",
        "archived": true,
        "deleted_count": out.deleted_ids.len(),
        "deleted_ids": out.deleted_ids,
        "skipped_count": out.skipped_ids.len(),
        "skipped_ids": out.skipped_ids,
    }))
}

pub(super) fn do_approve_commit(
    db: &Db,
    ctx: &Ctx,
    id: &str,
    expected_fp: &str,
    reviewer: &str,
) -> Result<Value, String> {
    let by: String = reviewer
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
        .take(64)
        .collect();
    let who = if by.is_empty() { "web-ui" } else { &by };
    match approve_and_regate(
        db,
        ctx,
        id,
        who,
        Some(expected_fp),
        "aaf ui approve",
        who,
        SystemClock.now(),
    ) {
        Ok(out) => Ok(json!({
            "approved": true, "task_id": id, "reviewer": out.approval.reviewer,
            "lock_fp": out.approval.lock_fp, "state": out.transitioned_to.map(TaskState::as_str),
        })),
        Err(ApproveOpError::Gate(GateError::NothingLocked)) => {
            Err("refused: no locked file is changed; nothing approved".into())
        }
        Err(ApproveOpError::Gate(GateError::LockFpChanged)) => {
            Err("lock_fp mismatch: locked diff changed since preview".into())
        }
        Err(e) => Err(e.to_string()),
    }
}

pub(super) fn do_rework(db: &Db, ctx: &Ctx, task_id: &str, reason: &str) -> Result<Value, String> {
    let (rerun, cleaned) = parse_rework_reason(reason);
    if cleaned.is_empty() {
        return Err("invalid rework reason: empty".into());
    }
    let out = rework_task(
        db,
        ctx,
        task_id,
        cleaned,
        rerun,
        "aaf ui rework",
        "web-ui",
        SystemClock.now(),
    )
    .map_err(|e| e.to_refused_string())?;
    Ok(json!({ "action": "rework", "task_id": task_id, "state": out.target.as_str() }))
}

pub(super) fn collect_recent_runs(db: &Db, repo_root: Option<&str>) -> Vec<Value> {
    db.recent_runs(repo_root, RECENT_RUNS_LIMIT)
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            json!({
                "id": r.id, "task_id": r.task_id, "executor": r.executor,
                "status": r.status.as_str(), "started_at": r.started_at, "ended_at": r.ended_at
            })
        })
        .collect()
}

pub(super) fn read_task_pipeline(db: &Db, task_id: &str) -> Result<Value, String> {
    let task = db.get_task(task_id).map_err(s_err)?;
    let runs = db.runs_for_task(&task.id).map_err(s_err)?;
    let events = db.events_for_task(&task.id, 500).map_err(s_err)?;
    let pipe = aaf_engine::telemetry::build_task_pipeline(&task, &runs, &events);
    serde_json::to_value(pipe).map_err(s_err)
}

pub(super) fn read_sop_runs(ctx: &Ctx) -> Result<Value, String> {
    let runs = aaf_engine::telemetry::list_sop_runs(&ctx.data_dir, 50);
    Ok(json!({ "runs": runs }))
}

pub(super) fn parse_and_format_comments(
    action: &str,
    base_note: &str,
    raw_comments: &[Value],
) -> Result<String, String> {
    let max_bytes = if action == "steer" {
        super::diff_parse::MAX_STEER_NOTE_BYTES
    } else {
        super::diff_parse::MAX_COMMENTS_TOTAL_BYTES
    };
    if raw_comments.is_empty() {
        if action == "steer" && base_note.trim().len() > max_bytes {
            return Err(format!(
                "invalid steer note: exceeds {max_bytes}-byte steer limit ({} bytes); shorten note or use rework (up to {} bytes)",
                base_note.trim().len(),
                super::diff_parse::MAX_COMMENTS_TOTAL_BYTES
            ));
        }
        return Ok(base_note.to_string());
    }
    let mut parsed = Vec::with_capacity(raw_comments.len());
    for item in raw_comments {
        let c: super::diff_parse::DiffComment =
            serde_json::from_value(item.clone()).map_err(|e| format!("invalid comment: {e}"))?;
        parsed.push(c);
    }
    super::diff_parse::format_inline_comments_bounded(base_note, &parsed, max_bytes)
}

pub(super) fn dispatch_db_action(
    db: &Db,
    ctx: &Ctx,
    config_base: &std::path::Path,
    repo: Option<&str>,
    action: &str,
    task_id: &str,
    note: &str,
) -> Result<Value, String> {
    match action {
        "ready" => do_ready(db, task_id),
        "ready_all" | "ready-all" => do_ready_all(db, repo),
        "cancel" => do_cancel(db, task_id, note),
        "unblock" => do_unblock(db, task_id, note),
        "unblock_all" | "unblock-all" => do_unblock_all(db, repo),
        "delete_done" | "delete-done" => do_delete_done(db, repo, task_id, note),
        "approve_plan" | "approve-plan" => do_approve_plan(db, task_id),
        "rework" => do_rework(db, ctx, task_id, note),
        "steer" => {
            if note.trim().len() > super::diff_parse::MAX_STEER_NOTE_BYTES {
                return Err(format!(
                    "invalid steer note: exceeds {}-byte steer limit ({} bytes)",
                    super::diff_parse::MAX_STEER_NOTE_BYTES,
                    note.trim().len()
                ));
            }
            let recorded =
                aaf_engine::factory::snapshots::steer_task(db, task_id, note, SystemClock.now())?;
            Ok(json!({ "action": "steer", "task_id": task_id, "note": recorded }))
        }
        "unfreeze" => {
            aaf_engine::factory::unfreeze(db, &SystemClock).map_err(s_err)?;
            Ok(json!({ "action": "unfreeze", "frozen": false }))
        }
        "stop_run" => super::proof_stop::do_stop_run(db, ctx, task_id),
        "account_cooldown" => {
            super::extras::do_account_cooldown(db, ctx, config_base, task_id, note, false)
        }
        "account_clear" => {
            super::extras::do_account_cooldown(db, ctx, config_base, task_id, "", true)
        }
        "account_clear_all" | "account-clear-all" => {
            super::accounts_panel::do_account_clear_all(db, ctx, config_base)
        }
        other => Err(format!("invalid action: {other}")),
    }
}

pub(super) fn s_err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

pub(super) fn format_blocked_detail(reason: &BlockedReason) -> String {
    match reason {
        BlockedReason::NeedsHuman { why } => why.clone(),
        BlockedReason::GateFailed { summary } => summary.clone(),
        BlockedReason::Dependency { task_id } => format!("waiting on {task_id}"),
        BlockedReason::MergeConflict { detail }
        | BlockedReason::FlakyTest { detail }
        | BlockedReason::Stuck { detail }
        | BlockedReason::BudgetExceeded { detail }
        | BlockedReason::Infra { detail }
        | BlockedReason::AuditRejected { detail } => detail.clone(),
        BlockedReason::Quota { reset_at } => match reset_at {
            Some(ts) => format!("quota exhausted (reset_at={ts})"),
            None => "quota exhausted".to_string(),
        },
    }
}

pub(super) fn do_approve_plan(db: &Db, task_id: &str) -> Result<Value, String> {
    let task = db.get_task(task_id).map_err(s_err)?;
    if task.state != TaskState::Blocked {
        return Err(format!(
            "refused: {task_id} is {}, not blocked",
            task.state.as_str()
        ));
    }
    let waiting = matches!(
        &task.blocked_reason,
        Some(BlockedReason::NeedsHuman { why })
            if why.starts_with(PLAN_APPROVAL_PREFIX)
                || why.starts_with(PLAN_DEBATE_STALLED_PREFIX)
    );
    if !waiting {
        return Err(format!(
            "refused: {task_id} is not waiting for plan approval"
        ));
    }
    let Some(plan) = latest_plan_info(db, task_id).map_err(s_err)? else {
        return Err(format!(
            "refused: {task_id} has no generated plan event to approve"
        ));
    };
    let now = SystemClock.now();
    let note = format_approved_plan_note(&plan);
    db.append_context_note(task_id, &note, now).map_err(s_err)?;
    emit_human_plan_converged(db, task_id, &plan, now).map_err(s_err)?;
    let budget_reset = reset_exhausted_budget(db, &task.root_id, &task.repo_root).map_err(s_err)?;
    db.set_state(task_id, TaskState::Ready, None, now)
        .map_err(s_err)?;
    let payload = json!({
        "from": "blocked", "to": "ready", "why": "plan_approved",
        "by": "web-ui", "budget_reset": budget_reset,
    });
    let ev = FactoryEvent::new(EventKind::TaskStateChanged, payload.to_string(), now);
    db.emit(&ev.for_task(task_id)).map_err(s_err)?;
    Ok(json!({
        "action": "approve_plan", "task_id": task_id, "state": "ready",
        "approved": true, "budget_reset": budget_reset,
    }))
}

pub(super) fn resolve_explicit_key(cli_key: Option<&str>, ctx: &Ctx) -> Option<String> {
    let clean = |v: &str| (!v.trim().is_empty()).then(|| v.trim().to_string());
    let env_val = ctx
        .aaf_env
        .iter()
        .find(|(k, _)| k == "AAF_UI_TOKEN")
        .map(|(_, v)| v.as_str());
    cli_key
        .and_then(clean)
        .or_else(|| env_val.and_then(clean))
        .or_else(|| std::env::var("AAF_UI_TOKEN").ok().and_then(|v| clean(&v)))
}

pub(super) fn build_handover_json(db: &Db, ctx: &Ctx, task_id: &str) -> Result<Value, String> {
    let task = db.get_task(task_id).map_err(s_err)?;
    let repo_cfg =
        crate::cmd::handover::load_repo_config(ctx, std::path::Path::new(&task.repo_root));
    let data = aaf_engine::handover::load_handover(db, &ctx.data_dir, task_id, &repo_cfg)
        .map_err(s_err)?;
    let mut val = serde_json::to_value(&data).map_err(s_err)?;
    if let Value::Object(ref mut map) = val {
        map.insert(
            "markdown".into(),
            json!(aaf_engine::handover::render_markdown(&data)),
        );
    }
    Ok(val)
}

pub(super) fn build_approve_preview_json(
    db: Db,
    ctx: &Ctx,
    task_id: &str,
) -> Result<Value, String> {
    let (_, report) = crate::cmd::task_ops::eval_task_head_lock(db, task_id, ctx)
        .map_err(|e| e.message().to_string())?;
    let changes: Vec<Value> = report
        .locked_changes
        .iter()
        .map(|ch| json!({ "change": ch.change.letter(), "path": ch.path }))
        .collect();
    Ok(json!({
        "task_id": task_id,
        "lock_fp": report.lock_fp,
        "can_approve": !changes.is_empty(),
        "locked_changes": changes,
    }))
}

pub(super) fn build_board_json(
    db: Option<&Db>,
    scope: &str,
    repo: Option<&str>,
    state: Option<&str>,
) -> Result<Value, String> {
    let parsed = state
        .map(|s| {
            TaskState::parse(&s.to_ascii_lowercase()).ok_or_else(|| format!("invalid state: {s}"))
        })
        .transpose()?;
    let Some(db) = db else {
        return Ok(json!({ "scope": scope, "frozen": false, "tasks": [] }));
    };
    let tasks = db
        .list_board_tasks(repo, parsed)
        .map_err(|e| e.to_string())?;
    let mut enrichment = db.board_enrichment(repo).map_err(|e| e.to_string())?;
    let items: Vec<Value> = tasks
        .into_iter()
        .map(|t| {
            let deps = enrichment.deps_by_task.remove(&t.id).unwrap_or_default();
            let latest_run_id = enrichment.latest_run_by_task.remove(&t.id);
            let run_summary = enrichment.run_summary_by_task.remove(&t.id);
            let latest_event = enrichment.latest_event_by_task.remove(&t.id);
            let repo_name = std::path::Path::new(&t.repo_root)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            let blocked_detail = t.blocked_reason.as_ref().map(format_blocked_detail);
            let needs_plan = super::extras::task_needs_plan_approval(&t);
            let latest_run_json = run_summary.as_ref().map(|r| {
                json!({
                    "id": r.id,
                    "status": r.status,
                    "executor": r.executor,
                    "account": r.account,
                    "started_at": r.started_at,
                    "ended_at": r.ended_at,
                })
            });
            let latest_event_json = latest_event
                .as_ref()
                .map(|(kind, at)| json!({ "kind": kind, "at": at }));
            json!({
                "id": t.id, "title": t.title, "state": t.state.as_str(), "priority": t.priority.as_str(),
                "blocked_reason": t.blocked_reason.as_ref().map(|b| b.kind()), "blocked_detail": blocked_detail,
                "needs_plan_approval": needs_plan, "spec_description": t.spec.description,
                "acceptance": t.spec.acceptance, "fingerprint": t.fingerprint, "repo_name": repo_name,
                "unblock_cycles": t.unblock_cycles, "deps": deps, "repo_root": t.repo_root,
                "created_at": t.created_at, "updated_at": t.updated_at, "latest_run_id": latest_run_id,
                "latest_run": latest_run_json,
                "run_count": run_summary.as_ref().map_or(0, |r| r.run_count),
                "total_duration_secs": run_summary.as_ref().map_or(0, |r| r.total_duration_secs),
                "latest_event": latest_event_json,
            })
        })
        .collect();
    Ok(json!({ "scope": scope, "frozen": enrichment.frozen, "tasks": items }))
}
