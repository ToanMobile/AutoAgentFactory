//! JSON projection helpers for `/studio` token diagnostics (`GET /api/report`).

use std::collections::BTreeMap;

use aaf_domain::budget::RootBudget;
use aaf_engine::executors::stream_log::stream_exists;
use serde_json::{Value, json};

use super::alerts::TokenAlert;
use super::{
    AccountAgg, DiagnosticsCtx, DupAuditRow, ExecAgg, RunTotals, TopRunRow, pct, wasted_level,
};

pub(super) struct BurnSnapshot {
    pub(super) tokens_last_1h: u64,
    pub(super) tokens_last_24h: u64,
    pub(super) daily_cap: u64,
    pub(super) spike_limit_1h: u64,
    pub(super) cost_paused_until: Option<i64>,
    pub(super) cost_paused_stale: bool,
    pub(super) factory_frozen: bool,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_diagnostics_payload(
    t: &RunTotals,
    burn: &BurnSnapshot,
    dctx: DiagnosticsCtx<'_>,
    alerts: &[TokenAlert],
    alerts_truncated: bool,
    by_exec: BTreeMap<String, ExecAgg>,
    by_account: BTreeMap<String, AccountAgg>,
    top_runs: Vec<TopRunRow<'_>>,
    dup_audits: &[DupAuditRow],
    budget_rows: &[(&RootBudget, String, f64, bool)],
) -> Value {
    let total_tokens = t.total_in.saturating_add(t.total_out);
    let wasted_pct = pct(t.wasted_tok, total_tokens);
    let audit_pct = pct(t.audit_tok, total_tokens);
    let avg_audit_tokens = t.audit_tok.checked_div(t.audit_runs).unwrap_or(0);
    let avg_coding_tokens = t.coding_tok.checked_div(t.coding_runs).unwrap_or(0);

    json!({
        "summary": {
            "total_tokens": total_tokens,
            "tokens_in": t.total_in,
            "tokens_out": t.total_out,
            "total_runs": t.total_runs,
            "succeeded_tokens": t.succeeded_tok,
            "running_tokens": t.running_tok,
            "wasted_tokens": t.wasted_tok,
            "wasted_runs": t.wasted_runs,
            "wasted_pct": wasted_pct,
            "wasted_level": wasted_level(t.wasted_tok, wasted_pct),
            "failed_tokens": t.failed_tok,
            "failed_runs": t.failed_runs,
            "audit_rejected_tokens": t.audit_rejected_tok,
            "audit_rejected_runs": t.audit_rejected_runs,
            "timed_out_tokens": t.timed_out_tok,
            "timed_out_runs": t.timed_out_runs,
            "killed_tokens": t.killed_tok,
            "killed_runs": t.killed_runs,
            "limit_hit_tokens": t.limit_hit_tok,
            "limit_hit_runs": t.limit_hit_runs,
            "zero_token_runs": t.zero_token_runs,
            "zero_token_failed_runs": t.zero_token_failed_runs,
            "stale_running_runs": t.stale_running_runs,
            "audit_tokens": t.audit_tok,
            "audit_runs": t.audit_runs,
            "audit_pct": audit_pct,
            "avg_audit_tokens": avg_audit_tokens,
            "coding_tokens": t.coding_tok,
            "coding_runs": t.coding_runs,
            "avg_coding_tokens": avg_coding_tokens,
            "other_tokens": t.other_tok,
            "other_runs": t.other_runs,
            "duplicate_audit_groups": dup_audits.len(),
            "duplicate_audit_extra_runs": dup_audits.iter().map(|d| d.count.saturating_sub(1)).sum::<u64>(),
        },
        "burn_rate": {
            "tokens_last_1h": burn.tokens_last_1h,
            "tokens_last_24h": burn.tokens_last_24h,
            "daily_token_cap": burn.daily_cap,
            "spike_limit_1h": burn.spike_limit_1h,
            "cost_paused_until": burn.cost_paused_until,
            "cost_paused_stale": burn.cost_paused_stale,
            "factory_frozen": burn.factory_frozen,
            "config_error": dctx.config_error,
        },
        "alerts": alerts.iter().map(|a| json!({
            "severity": a.severity, "code": a.code, "title": a.title,
            "detail": a.detail, "recommendation": a.recommendation, "task_ids": a.task_ids,
        })).collect::<Vec<_>>(),
        "alerts_truncated": alerts_truncated,
        "by_executor": by_exec.into_iter().map(|(exec, a)| {
            let tot = a.tokens_in.saturating_add(a.tokens_out);
            json!({
                "executor": exec, "runs": a.runs, "succeeded": a.succeeded,
                "failed": a.failed, "audit_rejected": a.audit_rejected,
                "timed_out": a.timed_out, "killed": a.killed,
                "limit_hit": a.limit_hit, "running": a.running,
                "zero_token_runs": a.zero_token_runs, "tokens_in": a.tokens_in,
                "tokens_out": a.tokens_out, "tokens_total": tot,
                "wasted_tokens": a.wasted_tokens, "wasted_pct": pct(a.wasted_tokens, tot),
                "avg_tokens_per_run": tot.checked_div(a.runs).unwrap_or(0), "wall_secs": a.wall_secs,
            })
        }).collect::<Vec<_>>(),
        "by_account": by_account.into_iter().map(|(acct, a)| {
            let tot = a.tokens_in.saturating_add(a.tokens_out);
            json!({
                "account_id": acct, "account_name": a.name, "runs": a.runs,
                "wasted_runs": a.wasted_runs, "tokens_in": a.tokens_in,
                "tokens_out": a.tokens_out, "tokens_total": tot,
                "wasted_tokens": a.wasted_tokens, "wasted_pct": pct(a.wasted_tokens, tot),
            })
        }).collect::<Vec<_>>(),
        "top_runs": top_runs.into_iter().map(|tr| json!({
            "run_id": tr.run.id, "task_id": tr.run.task_id, "task_title": tr.task_title,
            "repo_name": tr.repo_name, "executor": tr.run.executor,
            "account_id": tr.run.account_id, "account_name": tr.account_name,
            "status": tr.run.status.as_str(),
            "tokens_in": tr.run.tokens_in, "tokens_out": tr.run.tokens_out,
            "tokens_total": tr.total_tokens, "duration_secs": tr.duration_secs,
            "started_at": tr.run.started_at, "wasted": tr.wasted,
            "audit_rejected": tr.audit_rejected,
            "has_stream_log": dctx.data_dir.is_some_and(|dir| stream_exists(dir, &tr.run.id)),
        })).collect::<Vec<_>>(),
        "duplicate_audits": dup_audits.iter().map(|d| json!({
            "task_id": d.task_id, "task_title": d.task_title, "tree_fp": d.tree_fp,
            "config_hash": d.config_hash, "count": d.count, "fail_count": d.fail_count,
            "inconclusive_count": d.inconclusive_count, "verdicts": d.verdicts,
        })).collect::<Vec<_>>(),
        "budget_watch": budget_rows.iter().take(8).map(|(b, title, used_pct, has_open)| json!({
            "root_id": b.root_id, "title": title, "tokens_used": b.tokens_used,
            "token_limit": b.token_limit, "used_pct": used_pct,
            "wall_secs_used": b.wall_secs_used, "wall_secs_limit": b.wall_secs_limit,
            "cycles_used": b.cycles_used, "cycles_limit": b.cycles_limit,
            "frozen": b.frozen, "has_open": has_open,
        })).collect::<Vec<_>>(),
    })
}
