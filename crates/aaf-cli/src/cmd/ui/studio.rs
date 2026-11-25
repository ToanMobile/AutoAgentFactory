//! Data behind the `/studio` page of `aaf ui`: the period report,
//! per-task duration/token/blocked breakdown, and doctor checks.

use std::path::Path;

use aaf_domain::ports::{Clock, RunStore, TaskStore};
use aaf_domain::run::RunStatus;
use aaf_domain::task::TaskState;
use aaf_engine::db::Db;
use aaf_engine::telemetry::{CostPolicy, ReportWindow, collect_report};
use serde_json::{Value, json};

use super::actions::format_blocked_detail;
use crate::cmd::doctor::{DoctorArgs, run as run_doctor};
use crate::cmd::report::since_cutoff;
use crate::ctx::{Ctx, SystemClock};

/// The `/studio` link next to a pairing URL (`http://h:p/#token=…`).
pub(crate) fn studio_url(pairing_url: &str) -> String {
    pairing_url.replacen("/#token=", "/studio#token=", 1)
}

fn collect_task_breakdown(
    db: &Db,
    repo: Option<&str>,
    since: Option<i64>,
    now: i64,
) -> Result<Vec<Value>, String> {
    let mut tasks = db
        .list_tasks(repo, None)
        .map_err(|e| format!("tasks: {e}"))?;
    if let Some(cutoff) = since {
        tasks.retain(|t| {
            t.updated_at >= cutoff || !matches!(t.state, TaskState::Done | TaskState::Cancelled)
        });
    }
    tasks.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.id.cmp(&b.id)));
    Ok(tasks
        .into_iter()
        .map(|t| {
            let runs = db.runs_for_task(&t.id).unwrap_or_default();
            let mut runs_ok = 0_u64;
            let mut runs_fail = 0_u64;
            let mut runs_live = 0_u64;
            let mut run_dur_secs = 0_i64;
            let mut tok_in = 0_u64;
            let mut tok_out = 0_u64;
            for r in &runs {
                match r.status {
                    RunStatus::Succeeded => runs_ok += 1,
                    RunStatus::Running => runs_live += 1,
                    RunStatus::Failed
                    | RunStatus::TimedOut
                    | RunStatus::LimitHit
                    | RunStatus::Killed => runs_fail += 1,
                }
                let end = r.ended_at.unwrap_or(now);
                if end >= r.started_at {
                    run_dur_secs = run_dur_secs.saturating_add(end.saturating_sub(r.started_at));
                }
                tok_in = tok_in.saturating_add(r.tokens_in);
                tok_out = tok_out.saturating_add(r.tokens_out);
            }
            let last = runs.last();
            let repo_name = Path::new(&t.repo_root)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let lead_end = if matches!(t.state, TaskState::Done | TaskState::Cancelled) {
                t.updated_at
            } else {
                now
            };
            let lead_time_secs = lead_end.saturating_sub(t.created_at).max(0);
            let blocked_kind = t.blocked_reason.as_ref().map(|b| b.kind());
            let blocked_detail = t
                .blocked_reason
                .as_ref()
                .map(format_blocked_detail)
                .unwrap_or_default();
            json!({
                "id": t.id,
                "title": t.title,
                "repo_name": repo_name,
                "state": t.state.as_str(),
                "priority": t.priority.as_str(),
                "created_at": t.created_at,
                "updated_at": t.updated_at,
                "runs_total": runs.len(),
                "runs_succeeded": runs_ok,
                "runs_failed": runs_fail,
                "runs_running": runs_live,
                "run_duration_secs": run_dur_secs,
                "lead_time_secs": lead_time_secs,
                "tokens_in": tok_in,
                "tokens_out": tok_out,
                "tokens_total": tok_in.saturating_add(tok_out),
                "last_executor": last.map(|r| r.executor.as_str()).unwrap_or(""),
                "last_account": last.and_then(|r| r.account_id.as_deref()).unwrap_or(""),
                "last_run_status": last.map(|r| r.status.as_str()).unwrap_or(""),
                "last_branch": last.map(|r| r.branch.as_str()).unwrap_or(""),
                "blocked_kind": blocked_kind,
                "blocked_detail": blocked_detail,
            })
        })
        .collect())
}

/// `cost` prices API-billed accounts only; subscription runs report tokens.
pub(crate) fn report_json(
    db: Option<&Db>,
    repo: Option<&str>,
    since_hours: Option<u64>,
    cost: &CostPolicy,
) -> Result<Value, String> {
    let now = SystemClock.now();
    let window = ReportWindow {
        since: since_hours.map(|h| since_cutoff(now, h)),
        now,
    };
    let report = match db {
        Some(db) => collect_report(db, repo, window, cost).map_err(|e| format!("report: {e}"))?,
        None => crate::cmd::report::empty_report(window),
    };
    let breakdown = match db {
        Some(db) => collect_task_breakdown(db, repo, window.since, now)?,
        None => Vec::new(),
    };
    let value = serde_json::to_value(&report).map_err(|e| format!("report: {e}"))?;
    Ok(json!({
        "ok": true,
        "scope": repo.unwrap_or("all"),
        "since_hours": since_hours,
        "report": value,
        "task_breakdown": breakdown,
    }))
}

pub(crate) fn doctor_json(ctx: &Ctx) -> Result<Value, String> {
    let out = run_doctor(&DoctorArgs {}, ctx).map_err(|e| e.message)?;
    let mut object = out.fields;
    object.insert("ok".into(), json!(true));
    object.insert("healthy".into(), json!(out.status == "ok"));
    object.insert("status".into(), json!(out.status));
    Ok(Value::Object(object))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn studio_url_keeps_the_token_fragment() {
        assert_eq!(
            studio_url("http://127.0.0.1:4317/#token=abc"),
            "http://127.0.0.1:4317/studio#token=abc"
        );
    }

    #[test]
    fn report_without_a_database_is_empty() {
        let v = report_json(None, Some("/repo"), Some(24), &CostPolicy::default()).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["scope"], "/repo");
        assert_eq!(v["since_hours"], 24);
        assert_eq!(v["report"]["counts"]["total"], 0);
        assert!(v["report"]["window"]["since"].is_i64());
        assert_eq!(v["task_breakdown"], json!([]));
    }
}
