//! Tier-3 On-Demand AI Decision Advice orchestrator:
//! - Hydrates cached `0600` sidecar advice into `GET /api/task/<id>` without
//!   ever spawning a worker on the GET path.
//! - Triggers async background read-only analysis via `POST /api/action`
//!   (`action: "analyze_decisions"`) with global concurrency limit 1,
//!   per-task in-flight dedup, 30s hammer cooldown, and zero-token deterministic
//!   fast-path for access/device blockers.

pub mod classify;
pub mod runner;
pub mod store;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::thread;

use aaf_domain::pipeline::PlanContract;
use aaf_domain::ports::{AccountStore, Clock, TaskStore};
use aaf_domain::task::{Task, TaskState};
use aaf_engine::db::Db;
use serde_json::{Value, json};

use self::classify::{compute_input_hash, try_deterministic_fast_path};
use self::runner::{AnalysisInput, run_ai_analysis};
use self::store::{ADVICE_SCHEMA_VERSION, StoredAdvice, read_valid_advice, write_advice_atomic};
use crate::cmd::ui::actions::{format_blocked_detail, s_err};
use crate::cmd::ui::extras::plan_detail_json;
use crate::ctx::{Ctx, SystemClock};

const TASK_COOLDOWN_SECS: i64 = 30;

#[derive(Default)]
struct JobTracker {
    active_task: Option<String>,
    last_trigger_by_task: HashMap<String, i64>,
}

fn tracker() -> &'static Mutex<JobTracker> {
    static TRACKER: OnceLock<Mutex<JobTracker>> = OnceLock::new();
    TRACKER.get_or_init(|| Mutex::new(JobTracker::default()))
}

struct ActiveGuard {
    task_id: String,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        let mut g = tracker().lock().unwrap_or_else(|e| e.into_inner());
        if g.active_task.as_deref() == Some(self.task_id.as_str()) {
            g.active_task = None;
        }
    }
}

fn extract_plan_contract(plan_json: &Value) -> Option<PlanContract> {
    plan_json
        .get("contract")
        .filter(|v| !v.is_null())
        .and_then(|v| serde_json::from_value::<PlanContract>(v.clone()).ok())
}

fn extract_blocked_text(task: &Task) -> String {
    task.blocked_reason
        .as_ref()
        .map(format_blocked_detail)
        .unwrap_or_default()
}

fn is_task_active(task_id: &str) -> bool {
    let g = tracker().lock().unwrap_or_else(|e| e.into_inner());
    g.active_task.as_deref() == Some(task_id)
}

/// Cheap GET-path hydration for `GET /api/task/<id>`.
/// Never spawns a worker or calls an LLM; reads & validates the `0600` sidecar.
pub(crate) fn read_for_task(ctx: &Ctx, task: &Task, plan_json: &Value) -> Value {
    let contract = extract_plan_contract(plan_json);
    let blocked_text = extract_blocked_text(task);
    let open_qs = contract
        .as_ref()
        .map(|c| c.open_questions.clone())
        .unwrap_or_default();
    if open_qs.is_empty() && (task.state != TaskState::Blocked || blocked_text.trim().is_empty()) {
        return Value::Null;
    }
    let input_hash = compute_input_hash(task, &blocked_text, &open_qs);
    let now = SystemClock.now();
    match read_valid_advice(&ctx.data_dir, &task.id, &input_hash, now) {
        Some(mut rec) => {
            if rec.status == "pending" && !is_task_active(&task.id) {
                rec.status = "error".into();
                rec.error = Some("previous analysis was interrupted; click retry".into());
            }
            serde_json::to_value(rec).unwrap_or(Value::Null)
        }
        None => Value::Null,
    }
}

/// Handles `POST /api/action` with `action == "analyze_decisions"`.
/// Returns immediately (`status: "ready"` or `"pending"`) without blocking the
/// HTTP worker thread during subprocess execution.
pub(crate) fn trigger_analysis(
    db: &Db,
    ctx: &Ctx,
    task_id: &str,
    note: &str,
) -> Result<Value, String> {
    let task = db.get_task(task_id).map_err(s_err)?;
    let plan_json = plan_detail_json(db, &task);
    let contract = extract_plan_contract(&plan_json);
    let blocked_text = extract_blocked_text(&task);
    let input =
        AnalysisInput::from_task_and_plan(task.clone(), blocked_text.clone(), contract.as_ref());
    let raw_items = input.raw_items();
    if raw_items.is_empty() {
        return Err(format!(
            "refused: task '{task_id}' has no open questions or blocked reason to analyze"
        ));
    }

    let input_hash = compute_input_hash(&task, &blocked_text, &input.open_questions);
    let force = note.trim().eq_ignore_ascii_case("force");
    let now = SystemClock.now();

    // 1. Return fresh ready/pending cache if available (and not force-refreshing).
    if let Some(existing) = read_valid_advice(&ctx.data_dir, &task.id, &input_hash, now) {
        if existing.status == "pending" && is_task_active(&task.id) {
            return Ok(json!({
                "action": "analyze_decisions",
                "task_id": task.id,
                "status": "pending",
                "cached": true,
            }));
        }
        if existing.status == "ready" && !force {
            return Ok(json!({
                "action": "analyze_decisions",
                "task_id": task.id,
                "status": "ready",
                "cached": true,
                "advice": existing,
            }));
        }
    }

    // 2. Zero-token deterministic fast path for pure access/device blockers.
    if let Some(items) = try_deterministic_fast_path(&raw_items) {
        let rec = StoredAdvice {
            schema_version: ADVICE_SCHEMA_VERSION,
            task_id: task.id.clone(),
            input_hash,
            status: "ready".into(),
            model: "deterministic-policy".into(),
            created_at: now,
            error: None,
            items,
        };
        write_advice_atomic(&ctx.data_dir, &rec)
            .map_err(|e| format!("write advice failed: {e}"))?;
        return Ok(json!({
            "action": "analyze_decisions",
            "task_id": task.id,
            "status": "ready",
            "cached": false,
            "advice": rec,
        }));
    }

    // 3. Enforce global concurrency limit = 1 + per-task 30s hammer cooldown.
    {
        let mut g = tracker().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(active) = g.active_task.as_deref() {
            if active == task.id {
                return Ok(json!({
                    "action": "analyze_decisions",
                    "task_id": task.id,
                    "status": "pending",
                    "cached": true,
                }));
            }
            return Err(format!(
                "refused: another AI decision analysis is currently running ({active}); retry in a moment"
            ));
        }
        if let Some(&last_ts) = g.last_trigger_by_task.get(&task.id)
            && now.saturating_sub(last_ts) < TASK_COOLDOWN_SECS
        {
            let wait = TASK_COOLDOWN_SECS - now.saturating_sub(last_ts);
            return Err(format!(
                "refused: please wait {wait}s before re-analyzing task '{task_id}'"
            ));
        }
        g.active_task = Some(task.id.clone());
        g.last_trigger_by_task
            .retain(|_, ts| now.saturating_sub(*ts) < TASK_COOLDOWN_SECS);
        g.last_trigger_by_task.insert(task.id.clone(), now);
    }

    let cooldown_ids: HashSet<String> = db
        .list_accounts()
        .unwrap_or_default()
        .into_iter()
        .filter(|a| a.in_cooldown(now))
        .map(|a| a.profile.id)
        .collect();

    let pending = StoredAdvice {
        schema_version: ADVICE_SCHEMA_VERSION,
        task_id: task.id.clone(),
        input_hash: input_hash.clone(),
        status: "pending".into(),
        model: String::new(),
        created_at: now,
        error: None,
        items: Vec::new(),
    };
    if let Err(e) = write_advice_atomic(&ctx.data_dir, &pending) {
        let mut g = tracker().lock().unwrap_or_else(|e| e.into_inner());
        if g.active_task.as_deref() == Some(task.id.as_str()) {
            g.active_task = None;
        }
        return Err(format!("write pending advice failed: {e}"));
    }

    let ctx_bg = ctx.clone();
    let tid_bg = task.id.clone();
    let hash_bg = input_hash;
    let spawn_res = thread::Builder::new()
        .name(format!("aaf-dec-{}", &tid_bg[..tid_bg.len().min(12)]))
        .spawn(move || {
            let _guard = ActiveGuard {
                task_id: tid_bg.clone(),
            };
            let finished = match run_ai_analysis(&ctx_bg, &input, &cooldown_ids) {
                Ok((model, items)) => StoredAdvice {
                    schema_version: ADVICE_SCHEMA_VERSION,
                    task_id: tid_bg,
                    input_hash: hash_bg,
                    status: "ready".into(),
                    model,
                    created_at: SystemClock.now(),
                    error: None,
                    items,
                },
                Err(err) => StoredAdvice {
                    schema_version: ADVICE_SCHEMA_VERSION,
                    task_id: tid_bg,
                    input_hash: hash_bg,
                    status: "error".into(),
                    model: String::new(),
                    created_at: SystemClock.now(),
                    error: Some(err),
                    items: Vec::new(),
                },
            };
            let _ = write_advice_atomic(&ctx_bg.data_dir, &finished);
        });

    if let Err(e) = spawn_res {
        let mut g = tracker().lock().unwrap_or_else(|e| e.into_inner());
        if g.active_task.as_deref() == Some(task.id.as_str()) {
            g.active_task = None;
        }
        return Err(format!("spawn decision analyzer thread failed: {e}"));
    }

    Ok(json!({
        "action": "analyze_decisions",
        "task_id": task.id,
        "status": "pending",
        "cached": false,
    }))
}
