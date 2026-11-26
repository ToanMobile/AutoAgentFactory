//! Unified human task actions shared by `aaf board`, `aaf ui`, `aaf telegram`,
//! and `aaf cockpit` (`unblock_task`, `rework_task`, budget reset, plan approval,
//! stale-receipt cleanup, worktree resolution, and DAG critical-path).

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

mod budget;
mod dag;
pub(crate) use budget::{reset_budget_for_unblock, reset_exhausted_budget};
pub(crate) use dag::compute_critical_path;

use aaf_domain::blocked::BlockedReason;
use aaf_domain::config::RepoConfig;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::gate::{Approval, HeadLockReport};
use aaf_domain::pipeline::PlanContract;
use aaf_domain::ports::{EventSink, PortError, RunStore, TaskStore};
use aaf_domain::task::{Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::factory::prompt::PLAN_APPROVED_PREFIX;
use aaf_engine::gate::{GIT_TIMEOUT, GateError, base, head_lock};
use aaf_engine::git::worktree::{self, worktree_path};
#[allow(unused_imports)]
pub(crate) use aaf_engine::plan_debate::PLAN_DEBATE_STALLED_PREFIX;
use aaf_engine::plan_debate::schema::{canonical_plan_hash, format_plan_approved_note};
use serde_json::{Value, json};

use crate::ctx::{Ctx, SystemClock};

pub(crate) const PLAN_APPROVAL_PREFIX: &str = "plan approval required:";

pub(crate) struct LatestPlan {
    pub note_body: String,
    pub summary: String,
    pub plan_contract_hash: Option<String>,
    pub plan_contract: Option<PlanContract>,
}

#[derive(Debug)]
pub(crate) enum TaskOpError {
    WrongState {
        task_id: String,
        actual: TaskState,
        expected: TaskState,
    },
    Port(PortError),
}

impl From<PortError> for TaskOpError {
    fn from(err: PortError) -> Self {
        Self::Port(err)
    }
}

impl TaskOpError {
    pub(crate) fn to_refused_string(&self) -> String {
        match self {
            Self::WrongState {
                task_id,
                actual,
                expected,
            } => format!(
                "refused: {task_id} is {}, not {}",
                actual.as_str(),
                expected.as_str()
            ),
            Self::Port(err) => err.to_string(),
        }
    }
}

pub(crate) struct UnblockOutcome {
    pub(crate) budget_reset: bool,
    pub(crate) plan_approved: bool,
}

pub(crate) struct ReworkOutcome {
    pub(crate) target: TaskState,
    pub(crate) budget_reset: bool,
}

pub(crate) struct ApproveOutcome {
    pub(crate) approval: Approval,
    pub(crate) tree: PathBuf,
    pub(crate) transitioned_to: Option<TaskState>,
}

#[derive(Debug)]
pub(crate) enum ApproveOpError {
    Gate(GateError),
    Port(PortError),
    Config(String),
}

impl fmt::Display for ApproveOpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Gate(err) => write!(f, "{err}"),
            Self::Port(err) => write!(f, "{err}"),
            Self::Config(msg) => write!(f, "{msg}"),
        }
    }
}

/// Parses an optional leading `--rerun` flag from a free-text rework reason.
pub(crate) fn parse_rework_reason(raw: &str) -> (bool, &str) {
    match raw.strip_prefix("--rerun") {
        Some(after) if after.is_empty() || after.starts_with(char::is_whitespace) => {
            (true, after.trim())
        }
        _ => (false, raw.trim()),
    }
}

pub(crate) fn clear_stale_receipts(db: &impl RunStore, task: &Task, ctx: &Ctx) {
    let main_root = Path::new(&task.repo_root);
    let _ = fs::remove_file(aaf_fp::receipt_path(&ctx.data_dir, main_root));
    let _ = aaf_fp::clear_audits_for_repo(&ctx.data_dir, main_root);
    let main_id = aaf_fp::repo_id(main_root);
    let main_approvals_dir = ctx.data_dir.join("approvals").join(&main_id);

    let mut wt_roots = Vec::new();
    if let Ok(Some((wt, _))) =
        worktree::existing_registered(main_root, &ctx.worktrees_dir, &task.id)
    {
        wt_roots.push(wt.path);
    }
    if let Ok(wt) = worktree_path(main_root, &ctx.worktrees_dir, &task.id) {
        wt_roots.push(wt);
    }
    for r in db.runs_for_task(&task.id).unwrap_or_default() {
        if !r.worktree_path.is_empty() {
            wt_roots.push(PathBuf::from(r.worktree_path));
        }
    }
    for root in wt_roots {
        let _ = fs::remove_file(aaf_fp::receipt_path(&ctx.data_dir, &root));
        let _ = aaf_fp::clear_audits_for_repo(&ctx.data_dir, &root);
        let wt_id = aaf_fp::repo_id(&root);
        if wt_id == main_id {
            continue;
        }
        let wt_approvals = ctx.data_dir.join("approvals").join(&wt_id);
        // Drop mirrored main copies before the worktree dir so an interruption leaves them listed.
        if let Ok(entries) = fs::read_dir(&wt_approvals) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_str().is_some_and(|s| s.ends_with(".json")) {
                    let _ = fs::remove_file(main_approvals_dir.join(name));
                }
            }
        }
        let _ = fs::remove_dir_all(&wt_approvals);
    }
}

fn emit_task_event(
    db: &Db,
    kind: EventKind,
    payload: Value,
    task_id: &str,
    now: i64,
) -> Result<(), PortError> {
    db.emit(&FactoryEvent::new(kind, payload.to_string(), now).for_task(task_id))
        .map(|_| ())
}

pub(crate) fn latest_plan_info(db: &Db, task_id: &str) -> Result<Option<LatestPlan>, PortError> {
    let kinds = [
        EventKind::PlanConverged,
        EventKind::PlanDebateTurn,
        EventKind::PlanGenerated,
        EventKind::TaskStateChanged,
        EventKind::ReworkRequested,
    ];
    fn trimmed(v: &Value) -> Option<&str> {
        v.as_str().map(str::trim).filter(|s| !s.is_empty())
    }
    let mut legacy: Option<String> = None;
    for event in db.events_for_task_by_kinds(task_id, &kinds, u32::MAX)? {
        if event.kind == EventKind::ReworkRequested
            || aaf_engine::plan_debate::state::is_human_unblock_event(&event)
        {
            break;
        }
        if event.kind == EventKind::TaskStateChanged {
            continue;
        }
        let Ok(payload) = serde_json::from_str::<Value>(&event.payload_json) else {
            continue;
        };
        if event.kind == EventKind::PlanDebateTurn
            && (payload["role"].as_str() != Some("author") || payload["ok"].as_bool() != Some(true))
        {
            continue;
        }
        if let Some(val) = payload.get("plan_contract").filter(|v| !v.is_null())
            && let Ok(contract) = serde_json::from_value::<PlanContract>(val.clone())
        {
            let hash = trimmed(&payload["plan_contract_hash"])
                .map(str::to_string)
                .unwrap_or_else(|| canonical_plan_hash(&contract));
            let summary = trimmed(&payload["summary"])
                .unwrap_or(contract.summary.trim())
                .to_string();
            let full = format_plan_approved_note(&summary, &hash);
            let note_body = full
                .strip_prefix(PLAN_APPROVED_PREFIX)
                .map_or(full.as_str(), str::trim)
                .to_string();
            return Ok(Some(LatestPlan {
                note_body,
                summary,
                plan_contract_hash: Some(hash),
                plan_contract: Some(contract),
            }));
        }
        if event.kind == EventKind::PlanGenerated
            && legacy.is_none()
            && let Some(plan) = trimmed(&payload["plan"])
        {
            legacy = Some(plan.to_string());
        }
    }
    Ok(legacy.map(|plan| LatestPlan {
        summary: plan_summary_one_line(&plan),
        note_body: plan,
        plan_contract_hash: None,
        plan_contract: None,
    }))
}

fn is_plan_meta_preamble(line: &str) -> bool {
    let lower = line.to_lowercase();
    lower.starts_with("tôi không lưu được file plan")
        || lower.starts_with("tôi đã có kế hoạch")
        || lower.starts_with("tôi chỉ đọc")
        || lower.starts_with("i only read the repo")
        || lower.starts_with("i cannot write")
        || lower.starts_with("i couldn't save")
}

/// Extracts a concise single-line summary from free-form markdown plan text.
pub(crate) fn plan_summary_one_line(plan: &str) -> String {
    fn cleaned(l: &str) -> &str {
        l.trim()
            .trim_start_matches(['#', '-', '*', '•', ' '])
            .trim()
    }
    let first = plan
        .lines()
        .map(cleaned)
        .find(|l| !l.is_empty() && !is_plan_meta_preamble(l))
        .or_else(|| plan.lines().map(cleaned).find(|l| !l.is_empty()))
        .unwrap_or("");
    let mut chars = first.chars();
    let short: String = chars.by_ref().take(140).collect();
    if chars.next().is_some() {
        format!("{short}…")
    } else {
        short
    }
}

/// Fallback for Cockpit/UI inspection when a task already carries an approved
/// plan in `context_notes` across a human unblock (which breaks event-epoch lookup).
pub(crate) fn plan_from_task_notes(task: &Task) -> Option<LatestPlan> {
    let body = task
        .context_notes
        .iter()
        .rev()
        .find_map(|n| n.strip_prefix(PLAN_APPROVED_PREFIX))
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    Some(LatestPlan {
        summary: plan_summary_one_line(body),
        note_body: body.to_string(),
        plan_contract_hash: None,
        plan_contract: None,
    })
}

pub(crate) fn format_approved_plan_note(plan: &LatestPlan) -> String {
    match &plan.plan_contract_hash {
        Some(hash) => format_plan_approved_note(&plan.summary, hash),
        None => format!("{PLAN_APPROVED_PREFIX} {}", plan.note_body),
    }
}

pub(crate) fn emit_human_plan_converged(
    db: &Db,
    task_id: &str,
    plan: &LatestPlan,
    now: i64,
) -> Result<(), PortError> {
    let payload = match (&plan.plan_contract, &plan.plan_contract_hash) {
        (Some(contract), Some(hash)) => json!({
            "forced_by": "human",
            "summary": plan.summary,
            "plan_contract_hash": hash,
            "plan_contract": contract,
        }),
        _ => json!({ "forced_by": "human", "summary": plan.summary }),
    };
    emit_task_event(db, EventKind::PlanConverged, payload, task_id, now)
}

fn maybe_approve_on_unblock(
    db: &Db,
    task: &Task,
    now: i64,
) -> Result<(bool, Option<LatestPlan>), PortError> {
    let Some(BlockedReason::NeedsHuman { why }) = &task.blocked_reason else {
        return Ok((false, None));
    };
    // Only auto-approve `PLAN_APPROVAL_PREFIX`, never `PLAN_DEBATE_STALLED_PREFIX`.
    let Some(rest) = why.strip_prefix(PLAN_APPROVAL_PREFIX) else {
        return Ok((false, None));
    };
    if let Some(info) = latest_plan_info(db, &task.id)? {
        db.append_context_note(&task.id, &format_approved_plan_note(&info), now)?;
        let converged = info.plan_contract.is_some().then_some(info);
        return Ok((true, converged));
    }
    let trimmed = rest.trim();
    let plan = if trimmed.is_empty() { why } else { trimmed };
    db.append_context_note(&task.id, &format!("{PLAN_APPROVED_PREFIX} {plan}"), now)?;
    Ok((true, None))
}

pub(crate) fn resolve_task_tree(db: &Db, task: &Task, ctx: &Ctx) -> PathBuf {
    if let Ok(Some((wt, _))) =
        worktree::existing_registered(Path::new(&task.repo_root), &ctx.worktrees_dir, &task.id)
    {
        return wt.path;
    }
    if let Ok(wt) = worktree_path(Path::new(&task.repo_root), &ctx.worktrees_dir, &task.id)
        && wt.is_dir()
    {
        return wt;
    }
    db.runs_for_task(&task.id)
        .ok()
        .and_then(|runs| {
            runs.into_iter()
                .rev()
                .map(|r| PathBuf::from(r.worktree_path))
                .find(|p| !p.as_os_str().is_empty() && p.is_dir())
        })
        .unwrap_or_else(|| PathBuf::from(&task.repo_root))
}

#[derive(Debug)]
pub(crate) enum HeadLockEvalError {
    Task(String),
    Config(String),
    Base(String),
}

impl HeadLockEvalError {
    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Task(m) | Self::Config(m) | Self::Base(m) => m,
        }
    }
}

fn tree_config_base(ctx: &Ctx, tree: &Path) -> Result<(RepoConfig, base::Base), HeadLockEvalError> {
    let eff = ctx
        .load_config(tree)
        .map_err(|e| HeadLockEvalError::Config(e.message))?;
    let base = base::resolve(tree, &eff.repo.gate.base_ref, GIT_TIMEOUT)
        .map_err(|e| HeadLockEvalError::Base(e.to_string()))?;
    Ok((eff.repo, base))
}

pub(crate) fn eval_tree_head_lock(
    ctx: &Ctx,
    tree: &Path,
) -> Result<(RepoConfig, HeadLockReport), HeadLockEvalError> {
    let (repo, base) = tree_config_base(ctx, tree)?;
    let report = head_lock::evaluate(&ctx.data_dir, tree, &repo, &base.diff_target);
    Ok((repo, report))
}

/// Consumes `db` so the read-only handle is released before git work on the tree.
pub(crate) fn task_tree(db: Db, task_id: &str, ctx: &Ctx) -> Result<PathBuf, HeadLockEvalError> {
    let task = db
        .get_task(task_id)
        .map_err(|e| HeadLockEvalError::Task(e.to_string()))?;
    Ok(resolve_task_tree(&db, &task, ctx))
}

pub(crate) fn eval_task_head_lock(
    db: Db,
    task_id: &str,
    ctx: &Ctx,
) -> Result<(PathBuf, HeadLockReport), HeadLockEvalError> {
    let tree = task_tree(db, task_id, ctx)?;
    Ok((tree.clone(), eval_tree_head_lock(ctx, &tree)?.1))
}

#[derive(Debug)]
pub(crate) enum ApproveRefusal {
    Eval(HeadLockEvalError),
    NothingLocked,
    FpChanged,
    Sign(GateError),
}

/// Signs a head-lock approval only if `lock_fp` still equals `expected_fp` (audit a13 L8-R3-1).
pub(crate) fn approve_if_unchanged(
    ctx: &Ctx,
    tree: &Path,
    expected_fp: &str,
    reviewer: &str,
) -> Result<Approval, ApproveRefusal> {
    let (repo, base) = tree_config_base(ctx, tree).map_err(ApproveRefusal::Eval)?;
    let (data, who) = (&ctx.data_dir, (reviewer, false));
    head_lock::approve_expecting(data, tree, &repo, &base, who, &SystemClock, expected_fp).map_err(
        |e| match e {
            GateError::NothingLocked => ApproveRefusal::NothingLocked,
            GateError::LockFpChanged => ApproveRefusal::FpChanged,
            e => ApproveRefusal::Sign(e),
        },
    )
}

/// Moves `Blocked -> Ready`, resets exhausted budget, approves pending plan, and records events.
pub(crate) fn unblock_task(
    db: &Db,
    task_id: &str,
    why: &str,
    by: &str,
    now: i64,
) -> Result<UnblockOutcome, TaskOpError> {
    let task = db.get_task(task_id)?;
    if task.state != TaskState::Blocked {
        return Err(TaskOpError::WrongState {
            task_id: task_id.to_string(),
            actual: task.state,
            expected: TaskState::Blocked,
        });
    }
    let budget_reset = reset_budget_for_unblock(db, &task.root_id, &task.repo_root)?;
    let (plan_approved, converged_plan) = maybe_approve_on_unblock(db, &task, now)?;
    db.append_context_note(task_id, &format!("unblocked: {why}"), now)?;
    db.set_state(task_id, TaskState::Ready, None, now)?;
    let payload = json!({
        "to": "ready", "why": why, "by": by,
        "was": task.blocked_reason, "budget_reset": budget_reset, "plan_approved": plan_approved,
    });
    emit_task_event(db, EventKind::TaskStateChanged, payload, task_id, now)?;
    if let Some(info) = converged_plan {
        emit_human_plan_converged(db, task_id, &info, now)?;
    }
    Ok(UnblockOutcome {
        budget_reset,
        plan_approved,
    })
}

/// Moves `Review -> Gating` (or `Ready` when `rerun`), resets budget, clears receipts/audits.
#[allow(clippy::too_many_arguments)]
pub(crate) fn rework_task(
    db: &Db,
    ctx: &Ctx,
    task_id: &str,
    reason: &str,
    rerun: bool,
    why: &str,
    by: &str,
    now: i64,
) -> Result<ReworkOutcome, TaskOpError> {
    let task = db.get_task(task_id)?;
    if task.state != TaskState::Review {
        return Err(TaskOpError::WrongState {
            task_id: task_id.to_string(),
            actual: task.state,
            expected: TaskState::Review,
        });
    }
    let target = if rerun {
        TaskState::Ready
    } else {
        TaskState::Gating
    };
    // Atomic CAS `Review -> target` BEFORE mutating budget, receipts, or notes.
    if !db.compare_and_set_state(task_id, TaskState::Review, target, None, now)? {
        let actual = db.get_task(task_id)?.state;
        return Err(TaskOpError::WrongState {
            task_id: task_id.to_string(),
            actual,
            expected: TaskState::Review,
        });
    }
    let budget_reset = reset_budget_for_unblock(db, &task.root_id, &task.repo_root)?;
    clear_stale_receipts(db, &task, ctx);
    let flat = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    db.append_context_note(task_id, &format!("rework: {flat}"), now)?;
    let rework_payload = json!({
        "reason": reason, "to": target.as_str(), "rerun": rerun,
        "budget_reset": budget_reset, "by": by,
    });
    emit_task_event(db, EventKind::ReworkRequested, rework_payload, task_id, now)?;
    let move_payload = json!({
        "from": TaskState::Review.as_str(), "to": target.as_str(), "why": why, "by": by,
    });
    emit_task_event(db, EventKind::TaskStateChanged, move_payload, task_id, now)?;
    Ok(ReworkOutcome {
        target,
        budget_reset,
    })
}

/// Signs a human head-lock approval and, when `Blocked` with a registered worktree,
/// resets exhausted root budget, appends `"unblocked: head-lock approved"`, and moves `Blocked -> Gating`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn approve_and_regate(
    db: &Db,
    ctx: &Ctx,
    task_id: &str,
    reviewer: &str,
    expected_lock_fp: Option<&str>,
    why: &str,
    by: &str,
    now: i64,
) -> Result<ApproveOutcome, ApproveOpError> {
    let task = db.get_task(task_id).map_err(ApproveOpError::Port)?;
    let main_root = Path::new(&task.repo_root);
    let registered_wt = worktree::existing_registered(main_root, &ctx.worktrees_dir, &task.id)
        .ok()
        .flatten()
        .map(|(wt, _)| wt.path);
    let target_tree = registered_wt
        .clone()
        .unwrap_or_else(|| resolve_task_tree(db, &task, ctx));
    let approval = match expected_lock_fp {
        Some(fp) => {
            approve_if_unchanged(ctx, &target_tree, fp, reviewer).map_err(|err| match err {
                ApproveRefusal::Eval(HeadLockEvalError::Config(msg)) => ApproveOpError::Config(msg),
                ApproveRefusal::Eval(
                    HeadLockEvalError::Base(msg) | HeadLockEvalError::Task(msg),
                ) => ApproveOpError::Gate(GateError::Git(msg)),
                ApproveRefusal::NothingLocked => ApproveOpError::Gate(GateError::NothingLocked),
                ApproveRefusal::FpChanged => ApproveOpError::Gate(GateError::LockFpChanged),
                ApproveRefusal::Sign(e) => ApproveOpError::Gate(e),
            })?
        }
        None => {
            let eff = ctx
                .load_config(&target_tree)
                .map_err(|e| ApproveOpError::Config(e.message))?;
            head_lock::approve(
                &ctx.data_dir,
                &target_tree,
                &eff.repo,
                reviewer,
                false,
                &SystemClock,
            )
            .map_err(ApproveOpError::Gate)?
        }
    };

    let mut transitioned_to = None;
    if task.state == TaskState::Blocked && registered_wt.is_some() {
        let budget_reset = reset_exhausted_budget(db, &task.root_id, &task.repo_root)
            .map_err(ApproveOpError::Port)?;
        db.append_context_note(&task.id, "unblocked: head-lock approved", now)
            .map_err(ApproveOpError::Port)?;
        db.set_state(&task.id, TaskState::Gating, None, now)
            .map_err(ApproveOpError::Port)?;
        let payload = json!({
            "from": TaskState::Blocked.as_str(), "to": TaskState::Gating.as_str(),
            "why": why, "by": by, "lock_fp": approval.lock_fp, "budget_reset": budget_reset,
        });
        emit_task_event(db, EventKind::TaskStateChanged, payload, &task.id, now)
            .map_err(ApproveOpError::Port)?;
        transitioned_to = Some(TaskState::Gating);
    }

    Ok(ApproveOutcome {
        approval,
        tree: target_tree,
        transitioned_to,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
