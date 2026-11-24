//! Unified human task actions shared by `aaf board`, `aaf ui`, `aaf telegram`,
//! and `aaf cockpit`.
//!
//! Centralizing `unblock_task`, `rework_task`, budget reset, plan approval,
//! stale-receipt cleanup, and worktree resolution guarantees all channels keep
//! identical invariants — notably appending an `"unblocked: <source>"` context
//! note on every unblock so the unblocker ladder resets its failure streak.
//! Also hosts the DAG critical-path helper shared by `aaf board graph` and
//! `aaf cockpit`.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use aaf_domain::blocked::BlockedReason;
use aaf_domain::budget::{BudgetCheck, RootBudget};
use aaf_domain::config::RepoConfig;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::gate::{Approval, HeadLockReport};
use aaf_domain::ports::{BudgetStore, EventSink, PortError, RunStore, TaskStore};
use aaf_domain::task::{Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::factory::prompt::PLAN_APPROVED_PREFIX;
use aaf_engine::gate::{GIT_TIMEOUT, GateError, base, head_lock};
use aaf_engine::git::worktree::{self, worktree_path};
use serde_json::{Value, json};

use crate::ctx::{Ctx, SystemClock};

pub(crate) const PLAN_APPROVAL_PREFIX: &str = "plan approval required:";
const PLAN_EVENT_LOOKBACK: u32 = 100;

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

pub(crate) fn reset_exhausted_budget(db: &Db, root_id: &str) -> Result<bool, PortError> {
    match db.get_budget(root_id) {
        Ok(budget) if budget.check() != BudgetCheck::Ok => {
            db.put_budget(&RootBudget {
                tokens_used: 0,
                wall_secs_used: 0,
                cycles_used: 0,
                frozen: false,
                ..budget
            })?;
            Ok(true)
        }
        Ok(_) | Err(PortError::NotFound(_)) => Ok(false),
        Err(other) => Err(other),
    }
}

pub(crate) fn clear_stale_receipts(db: &impl RunStore, task: &Task, ctx: &Ctx) {
    let main_root = Path::new(&task.repo_root);
    let _ = fs::remove_file(aaf_fp::receipt_path(&ctx.data_dir, main_root));
    let main_id = aaf_fp::repo_id(main_root);
    let main_approvals_dir = ctx.data_dir.join("approvals").join(&main_id);

    let mut wt_roots = Vec::new();
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
        let wt_id = aaf_fp::repo_id(&root);
        if wt_id == main_id {
            continue;
        }
        let wt_approvals = ctx.data_dir.join("approvals").join(&wt_id);
        // Drop the mirrored copies first and the worktree dir last: if this is
        // interrupted, the dir still lists what to clean on the next rework
        // instead of leaving orphaned (still valid) main-repo approvals.
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

pub(crate) fn latest_generated_plan(db: &Db, task_id: &str) -> Result<Option<String>, PortError> {
    for event in db.events_for_task(task_id, PLAN_EVENT_LOOKBACK)? {
        if event.kind != EventKind::PlanGenerated {
            continue;
        }
        let Ok(payload) = serde_json::from_str::<Value>(&event.payload_json) else {
            continue;
        };
        if let Some(plan) = payload["plan"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Ok(Some(plan.to_string()));
        }
    }
    Ok(None)
}

pub(crate) fn maybe_approve_on_unblock(db: &Db, task: &Task, now: i64) -> Result<bool, PortError> {
    let Some(BlockedReason::NeedsHuman { why }) = &task.blocked_reason else {
        return Ok(false);
    };
    let Some(rest) = why.strip_prefix(PLAN_APPROVAL_PREFIX) else {
        return Ok(false);
    };
    let generated = latest_generated_plan(db, &task.id)?;
    let plan = generated
        .as_deref()
        .or_else(|| (!rest.trim().is_empty()).then_some(rest.trim()))
        .unwrap_or(why);
    db.append_context_note(&task.id, &format!("{PLAN_APPROVED_PREFIX} {plan}"), now)?;
    Ok(true)
}

pub(crate) fn resolve_task_tree(db: &Db, task: &Task, ctx: &Ctx) -> PathBuf {
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

/// The step at which a head-lock evaluation failed, so each channel keeps
/// its own wording.
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

/// The config of `tree` and its resolved configured base.
fn tree_config_base(ctx: &Ctx, tree: &Path) -> Result<(RepoConfig, base::Base), HeadLockEvalError> {
    let eff = ctx
        .load_config(tree)
        .map_err(|e| HeadLockEvalError::Config(e.message))?;
    let base = base::resolve(tree, &eff.repo.gate.base_ref, GIT_TIMEOUT)
        .map_err(|e| HeadLockEvalError::Base(e.to_string()))?;
    Ok((eff.repo, base))
}

/// Head-lock state of `tree` against its configured base, with the config used.
pub(crate) fn eval_tree_head_lock(
    ctx: &Ctx,
    tree: &Path,
) -> Result<(RepoConfig, HeadLockReport), HeadLockEvalError> {
    let (repo, base) = tree_config_base(ctx, tree)?;
    let report = head_lock::evaluate(&ctx.data_dir, tree, &repo, &base.diff_target);
    Ok((repo, report))
}

/// The task's work tree; consumes `db` so the read-only handle is released
/// before any git work on the tree.
pub(crate) fn task_tree(db: Db, task_id: &str, ctx: &Ctx) -> Result<PathBuf, HeadLockEvalError> {
    let task = db
        .get_task(task_id)
        .map_err(|e| HeadLockEvalError::Task(e.to_string()))?;
    Ok(resolve_task_tree(&db, &task, ctx))
}

/// Resolves the task's work tree and evaluates its head-lock.
pub(crate) fn eval_task_head_lock(
    db: Db,
    task_id: &str,
    ctx: &Ctx,
) -> Result<(PathBuf, HeadLockReport), HeadLockEvalError> {
    let tree = task_tree(db, task_id, ctx)?;
    let (_, report) = eval_tree_head_lock(ctx, &tree)?;
    Ok((tree, report))
}

/// Why [`approve_if_unchanged`] signed nothing.
#[derive(Debug)]
pub(crate) enum ApproveRefusal {
    /// Re-evaluating the head-lock failed.
    Eval(HeadLockEvalError),
    /// No locked file is changed any more.
    NothingLocked,
    /// The locked diff is not the one the reviewer was shown.
    FpChanged,
    /// Signing itself failed.
    Sign(GateError),
}

/// Signs a head-lock approval only for the locked diff the reviewer saw:
/// refuses unless locked files are still changed and their `lock_fp` is
/// still `expected_fp`, checked on the very evaluation that is signed
/// ([`head_lock::approve_expecting`], audit a13 L8-R3-1). Shared by cockpit,
/// web UI and Telegram so every channel binds approvals the same way.
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

/// Moves a `Blocked` task back to `Ready`, resets any exhausted root budget,
/// approves a pending generated plan if applicable, appends an `"unblocked: <why>"`
/// context note, and records `TaskStateChanged`.
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
    let budget_reset = reset_exhausted_budget(db, &task.root_id)?;
    let plan_approved = maybe_approve_on_unblock(db, &task, now)?;
    db.append_context_note(task_id, &format!("unblocked: {why}"), now)?;
    db.set_state(task_id, TaskState::Ready, None, now)?;
    let payload = json!({
        "to": "ready",
        "why": why,
        "by": by,
        "was": task.blocked_reason,
        "budget_reset": budget_reset,
        "plan_approved": plan_approved,
    });
    db.emit(
        &FactoryEvent::new(EventKind::TaskStateChanged, payload.to_string(), now).for_task(task_id),
    )?;
    Ok(UnblockOutcome {
        budget_reset,
        plan_approved,
    })
}

/// Moves a `Review` task back to `Gating` (or `Ready` when `rerun == true`),
/// resets any exhausted root budget, clears stale receipts/approvals, appends
/// a `"rework: ..."` note, and emits `ReworkRequested` + `TaskStateChanged`.
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
    let budget_reset = reset_exhausted_budget(db, &task.root_id)?;
    clear_stale_receipts(db, &task, ctx);
    let note = format!(
        "rework: {}",
        reason.split_whitespace().collect::<Vec<_>>().join(" ")
    );
    db.append_context_note(task_id, &note, now)?;
    db.set_state(task_id, target, None, now)?;
    let rework_payload = json!({
        "reason": reason,
        "to": target.as_str(),
        "rerun": rerun,
        "budget_reset": budget_reset,
        "by": by,
    });
    db.emit(
        &FactoryEvent::new(EventKind::ReworkRequested, rework_payload.to_string(), now)
            .for_task(task_id),
    )?;
    let move_payload = json!({
        "from": TaskState::Review.as_str(),
        "to": target.as_str(),
        "why": why,
        "by": by,
    });
    db.emit(
        &FactoryEvent::new(EventKind::TaskStateChanged, move_payload.to_string(), now)
            .for_task(task_id),
    )?;
    Ok(ReworkOutcome {
        target,
        budget_reset,
    })
}

/// Longest directed chain (`blocker -> ... -> dependent`) in the DAG.
/// Returns an empty path when there are no dependency edges.
pub(crate) fn compute_critical_path(topo: &[String], edges: &[(String, String)]) -> Vec<String> {
    if edges.is_empty() {
        return Vec::new();
    }
    let mut blockers: HashMap<&str, Vec<&str>> = HashMap::new();
    for (task, blocked_by) in edges {
        blockers
            .entry(task.as_str())
            .or_default()
            .push(blocked_by.as_str());
    }
    let mut best_len: HashMap<&str, usize> = HashMap::with_capacity(topo.len());
    let mut pred: HashMap<&str, &str> = HashMap::new();
    let mut best_end: Option<&str> = None;
    let mut global_max = 0usize;

    for node in topo {
        let mut len = 1usize;
        let mut best_prev: Option<&str> = None;
        if let Some(deps) = blockers.get(node.as_str()) {
            for &dep in deps {
                let dep_len = best_len.get(dep).copied().unwrap_or(0);
                let cand = dep_len.saturating_add(1);
                if cand > len {
                    len = cand;
                    best_prev = Some(dep);
                }
            }
        }
        best_len.insert(node.as_str(), len);
        if let Some(prev) = best_prev {
            pred.insert(node.as_str(), prev);
        }
        if len > global_max {
            global_max = len;
            best_end = Some(node.as_str());
        }
    }

    if global_max < 2 {
        return Vec::new();
    }
    let mut path = Vec::with_capacity(global_max);
    let mut seen: HashSet<&str> = HashSet::new();
    let mut cur = best_end;
    while let Some(node) = cur {
        if !seen.insert(node) {
            break;
        }
        path.push(node.to_string());
        cur = pred.get(node).copied();
    }
    path.reverse();
    path
}

/// Signs a human head-lock approval for `task_id` and, when the task is
/// `Blocked` and its registered worktree directory exists on disk
/// ([`worktree::existing_registered`]), resets any exhausted root budget,
/// appends `"unblocked: head-lock approved"`, and transitions
/// `Blocked -> Gating`.
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
    let target_tree = match &registered_wt {
        Some(wt) => wt.clone(),
        None => resolve_task_tree(db, &task, ctx),
    };
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
        let budget_reset =
            reset_exhausted_budget(db, &task.root_id).map_err(ApproveOpError::Port)?;
        db.append_context_note(&task.id, "unblocked: head-lock approved", now)
            .map_err(ApproveOpError::Port)?;
        db.set_state(&task.id, TaskState::Gating, None, now)
            .map_err(ApproveOpError::Port)?;
        let payload = json!({
            "from": TaskState::Blocked.as_str(),
            "to": TaskState::Gating.as_str(),
            "why": why,
            "by": by,
            "lock_fp": approval.lock_fp,
            "budget_reset": budget_reset,
        });
        db.emit(
            &FactoryEvent::new(EventKind::TaskStateChanged, payload.to_string(), now)
                .for_task(&task.id),
        )
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
