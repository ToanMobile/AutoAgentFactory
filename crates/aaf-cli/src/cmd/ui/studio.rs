//! Data behind the `/studio` page of `aaf ui`: the period report,
//! per-task duration/token/blocked breakdown, and doctor checks.

use std::collections::HashMap;
use std::path::Path;

use aaf_domain::ports::{Clock, RunStore, TaskStore};
use aaf_domain::run::{RunRecord, RunStatus};
use aaf_domain::task::{Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::telemetry::{CostPolicy, ReportWindow, collect_report, compute_factory_score};
use serde_json::{Value, json};

use super::actions::format_blocked_detail;
use super::studio_tokens::{DiagnosticsCtx, TaskTokenExtra, collect_token_diagnostics};
use crate::cmd::doctor::{DoctorArgs, run as run_doctor};
use crate::cmd::report::since_cutoff;
use crate::ctx::{Ctx, SystemClock};

type RunsByTask = HashMap<String, Vec<RunRecord>>;

/// The `/studio` link next to a pairing URL (`http://h:p/#token=…`).
pub(crate) fn studio_url(pairing_url: &str) -> String {
    pairing_url.replacen("/#token=", "/studio#token=", 1)
}

fn load_window_tasks_and_runs(
    db: &Db,
    repo: Option<&str>,
    since: Option<i64>,
) -> Result<(Vec<Task>, RunsByTask), String> {
    let mut tasks = db
        .list_tasks(repo, None)
        .map_err(|e| format!("tasks: {e}"))?;
    let mut runs_by_task: RunsByTask = HashMap::with_capacity(tasks.len());
    tasks.retain(|t| {
        let runs: Vec<_> = db
            .runs_for_task(&t.id)
            .unwrap_or_default()
            .into_iter()
            .filter(|r| since.is_none_or(|cutoff| r.started_at >= cutoff))
            .collect();
        let has_window_runs = !runs.is_empty();
        let keep = match since {
            None => true,
            Some(cutoff) => {
                t.created_at >= cutoff
                    || t.updated_at >= cutoff
                    || has_window_runs
                    || !matches!(t.state, TaskState::Done | TaskState::Cancelled)
            }
        };
        if keep && has_window_runs {
            runs_by_task.insert(t.id.clone(), runs);
        }
        keep
    });
    tasks.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.id.cmp(&b.id)));
    Ok((tasks, runs_by_task))
}

fn collect_task_breakdown(
    tasks: &[Task],
    runs_by_task: &RunsByTask,
    now: i64,
    extras: &HashMap<String, TaskTokenExtra>,
) -> Vec<Value> {
    tasks
        .iter()
        .map(|t| {
            let runs = runs_by_task.get(&t.id).map_or(&[][..], Vec::as_slice);
            let mut runs_ok = 0_u64;
            let mut runs_fail = 0_u64;
            let mut runs_live = 0_u64;
            let mut run_dur_secs = 0_i64;
            let mut tok_in = 0_u64;
            let mut tok_out = 0_u64;
            for r in runs {
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
            let ex = extras.get(&t.id);
            let tok_total = tok_in.saturating_add(tok_out);
            let wasted_tokens = ex.map_or(0, |e| e.wasted_tokens);
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
                "runs_timed_out": ex.map_or(0, |e| e.runs_timed_out),
                "runs_killed": ex.map_or(0, |e| e.runs_killed),
                "runs_limit_hit": ex.map_or(0, |e| e.runs_limit_hit),
                "zero_token_runs": ex.map_or(0, |e| e.zero_token_runs),
                "run_duration_secs": run_dur_secs,
                "lead_time_secs": lead_time_secs,
                "tokens_in": tok_in,
                "tokens_out": tok_out,
                "tokens_total": tok_total,
                "wasted_tokens": wasted_tokens,
                "wasted_runs": ex.map_or(0, |e| e.wasted_runs),
                "audit_tokens": ex.map_or(0, |e| e.audit_tokens),
                "audit_rejected_tokens": ex.map_or(0, |e| e.audit_rejected_tokens),
                "duplicate_audits": ex.map_or(0, |e| e.duplicate_audits),
                "budget_used": ex.and_then(|e| e.budget_used),
                "budget_limit": ex.and_then(|e| e.budget_limit),
                "budget_frozen": ex.is_some_and(|e| e.budget_frozen),
                "last_executor": last.map(|r| r.executor.as_str()).unwrap_or(""),
                "last_account": last.and_then(|r| r.account_id.as_deref()).unwrap_or(""),
                "last_run_status": last.map(|r| r.status.as_str()).unwrap_or(""),
                "last_branch": last.map(|r| r.branch.as_str()).unwrap_or(""),
                "blocked_kind": blocked_kind,
                "blocked_detail": blocked_detail,
            })
        })
        .collect()
}

/// `cost` prices API-billed accounts only; subscription runs report tokens.
#[cfg(test)]
pub(crate) fn report_json(
    db: Option<&Db>,
    repo: Option<&str>,
    since_hours: Option<u64>,
    cost: &CostPolicy,
) -> Result<Value, String> {
    report_json_with_diag(db, repo, since_hours, cost, DiagnosticsCtx::default())
}

pub(crate) fn report_json_for_ctx(
    ctx: &Ctx,
    config_base: &Path,
    db: Option<&Db>,
    repo: Option<&str>,
    since_hours: Option<u64>,
) -> Result<Value, String> {
    let cost = db.map(|d| ctx.cost_policy(d)).unwrap_or_default();
    let cfg_res = ctx.load_config(config_base);
    let (cg, run_timeout_secs, cfg_err) = match &cfg_res {
        Ok(c) => (
            c.repo.unblocker.cost_guard,
            c.repo.run.run_timeout_secs,
            None,
        ),
        Err(e) => (Default::default(), 0, Some(e.message.as_str())),
    };
    let dctx = DiagnosticsCtx {
        data_dir: Some(&ctx.data_dir),
        cost_guard: cg,
        run_timeout_secs,
        config_error: cfg_err,
    };
    report_json_with_diag(db, repo, since_hours, &cost, dctx)
}

pub(crate) fn report_json_with_diag(
    db: Option<&Db>,
    repo: Option<&str>,
    since_hours: Option<u64>,
    cost: &CostPolicy,
    dctx: DiagnosticsCtx<'_>,
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
    let (breakdown, token_diagnostics) = match db {
        Some(db) => {
            let (tasks, runs_by_task) = load_window_tasks_and_runs(db, repo, window.since)?;
            let (diag, extras) =
                collect_token_diagnostics(db, &tasks, &runs_by_task, window.since, now, dctx);
            let tb = collect_task_breakdown(&tasks, &runs_by_task, now, &extras);
            (tb, diag)
        }
        None => (
            Vec::new(),
            json!({
                "summary": { "total_tokens": 0, "wasted_tokens": 0, "wasted_pct": 0.0, "wasted_level": "ok", "audit_tokens": 0, "audit_pct": 0.0 },
                "burn_rate": { "tokens_last_1h": 0, "tokens_last_24h": 0, "daily_token_cap": 0, "spike_limit_1h": 0, "cost_paused_until": null, "cost_paused_stale": false, "factory_frozen": false, "config_error": dctx.config_error },
                "alerts": [],
                "alerts_truncated": false,
                "by_executor": [],
                "by_account": [],
                "top_runs": [],
                "duplicate_audits": [],
                "budget_watch": [],
            }),
        ),
    };
    let score = compute_factory_score(&report);
    let mut value = serde_json::to_value(&report).map_err(|e| format!("report: {e}"))?;
    let score_val = serde_json::to_value(&score).map_err(|e| format!("score: {e}"))?;
    if let Value::Object(ref mut map) = value {
        map.insert("score".into(), score_val.clone());
    }
    Ok(json!({
        "ok": true,
        "scope": repo.unwrap_or("all"),
        "since_hours": since_hours,
        "report": value,
        "score": score_val,
        "task_breakdown": breakdown,
        "token_diagnostics": token_diagnostics,
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

    #[test]
    fn open_task_older_than_window_with_recent_run_matches_between_diag_and_breakdown() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("aaf.db")).unwrap();
        let now = SystemClock.now();
        let old = now - 48 * 3600;
        let task = Task::new_root(
            "t-old-open",
            "/repo/demo",
            "Old open task with fresh run",
            aaf_domain::task::ImmutableSpec {
                description: "d".into(),
                acceptance: vec!["a".into()],
            },
            aaf_domain::task::Priority::P1,
            old,
        );
        db.insert_task(&task).unwrap();
        db.insert_run(&aaf_domain::run::RunRecord {
            id: "run-fresh".into(),
            task_id: "t-old-open".into(),
            account_id: None,
            executor: "claude".into(),
            worktree_path: "/tmp/wt".into(),
            branch: "aaf/t-old-open".into(),
            status: RunStatus::Succeeded,
            exit_code: Some(0),
            tokens_in: 40_000,
            tokens_out: 5_000,
            started_at: now - 3600,
            ended_at: Some(now - 3500),
        })
        .unwrap();
        let v = report_json(Some(&db), None, Some(24), &CostPolicy::default()).unwrap();
        let diag_tot = v["token_diagnostics"]["summary"]["total_tokens"]
            .as_u64()
            .unwrap();
        let tb_tot: u64 = v["task_breakdown"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["tokens_total"].as_u64())
            .sum();
        assert_eq!(diag_tot, 45_000);
        assert_eq!(tb_tot, 45_000);
    }
}
