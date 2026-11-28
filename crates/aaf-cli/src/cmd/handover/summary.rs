//! Multi-task overnight digest and summary rendering (`--since` / `--summary`)
//! for `aaf handover`.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use aaf_domain::blocked::BlockedReason;
use aaf_domain::event::EventKind;
use aaf_domain::ports::{Clock, EventSink, RunStore, TaskStore};
use aaf_domain::task::TaskState;
use aaf_engine::handover::CSP_META;
use aaf_engine::telemetry::{
    AutonomyMetrics, CostBasis, CostPolicy, TaskCounts, collect_autonomy_metrics_for_tasks,
};
use serde_json::{Value, json};

use super::super::board::store::{err, open_db_read_only};
use super::{HandoverArgs, HandoverFormat, OutError, emit_output, open_html_file, resolve_format};
use crate::ctx::{Ctx, SystemClock};
use crate::output::{CmdOutput, CmdResult};

fn blocked_summary(reason: &BlockedReason) -> String {
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

pub(super) fn run_digest(args: &HandoverArgs, ctx: &Ctx) -> CmdResult {
    let format = match resolve_format(args, ctx) {
        Ok(fmt) => fmt,
        Err(policy) => return Ok(CmdOutput::policy("refused", policy)),
    };
    let repo = ctx
        .repo_root()
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    let now = SystemClock.now();
    let cutoff = args
        .since
        .map(|h| now.saturating_sub(i64::try_from(h.saturating_mul(3600)).unwrap_or(i64::MAX)));
    let mut counts = TaskCounts::default();
    let mut autonomy = AutonomyMetrics::default();
    let mut completed = Vec::new();
    let mut blocked = Vec::new();
    let (mut tokens_in, mut tokens_out) = (0u64, 0u64);
    let mut per_account: BTreeMap<Option<String>, (u64, u64)> = BTreeMap::new();
    let mut cost_policy = CostPolicy::default();

    if let Some(db) = open_db_read_only(ctx)? {
        cost_policy = ctx.cost_policy(&db);
        let tasks = db
            .list_tasks(repo.as_deref(), None)
            .map_err(|e| err("tasks", e))?;
        let mut window_tasks = Vec::new();
        for t in tasks
            .into_iter()
            .filter(|t| cutoff.is_none_or(|c| t.updated_at >= c || t.created_at >= c))
        {
            window_tasks.push((t.id.clone(), t.state));
            counts.total += 1;
            match t.state {
                TaskState::Backlog => counts.backlog += 1,
                TaskState::Ready => counts.ready += 1,
                TaskState::Running => counts.running += 1,
                TaskState::Gating => counts.gating += 1,
                TaskState::Review => counts.review += 1,
                TaskState::Blocked => counts.blocked += 1,
                TaskState::Done => counts.done += 1,
                TaskState::Cancelled => counts.cancelled += 1,
                // Own counter: an adopted PR is neither blocked nor coding.
                TaskState::Adopting => counts.adopting += 1,
            }
            for r in db.runs_for_task(&t.id).unwrap_or_default() {
                tokens_in = tokens_in.saturating_add(r.tokens_in);
                tokens_out = tokens_out.saturating_add(r.tokens_out);
                let entry = per_account.entry(r.account_id).or_insert((0, 0));
                entry.0 = entry.0.saturating_add(r.tokens_in);
                entry.1 = entry.1.saturating_add(r.tokens_out);
            }
            if matches!(t.state, TaskState::Done | TaskState::Review) {
                let pr_url = db
                    .events_for_task(&t.id, 50)
                    .unwrap_or_default()
                    .into_iter()
                    .find(|e| e.kind == EventKind::PrOpened)
                    .and_then(|e| serde_json::from_str::<Value>(&e.payload_json).ok())
                    .and_then(|v| {
                        v["url"]
                            .as_str()
                            .or_else(|| v["pr_url"].as_str())
                            .map(str::to_string)
                    });
                completed.push(json!({
                    "id": t.id, "title": t.title, "state": t.state.as_str(), "pr_url": pr_url
                }));
            } else if t.state == TaskState::Blocked {
                let kind = t
                    .blocked_reason
                    .as_ref()
                    .map(BlockedReason::kind)
                    .unwrap_or("unknown");
                let reason = t
                    .blocked_reason
                    .as_ref()
                    .map_or_else(String::new, blocked_summary);
                blocked
                    .push(json!({ "id": t.id, "title": t.title, "kind": kind, "reason": reason }));
            }
        }
        autonomy = collect_autonomy_metrics_for_tasks(&db, repo.as_deref(), &window_tasks)
            .map_err(|e| err("autonomy", e))?;
    }
    let cost = cost_policy.estimate(
        per_account
            .iter()
            .map(|(acct, &(t_in, t_out))| (acct.as_deref(), t_in, t_out)),
    );
    let cost_basis = CostBasis::from_estimate(cost);
    let digest_json = json!({
        "mode": "digest", "since_hours": args.since, "counts": counts,
        "autonomy": autonomy,
        "completed": completed, "blocked": blocked,
        "tokens_in": tokens_in, "tokens_out": tokens_out,
        "estimated_cost_usd": cost, "cost_basis": cost_basis,
    });
    let rendered = match format {
        HandoverFormat::Json => {
            serde_json::to_string_pretty(&digest_json).map_err(|e| err("json", e))?
        }
        HandoverFormat::Md => render_digest_md(
            args.since, &counts, &autonomy, &completed, &blocked, tokens_in, tokens_out, cost,
        ),
        HandoverFormat::Html => {
            let md = render_digest_md(
                args.since, &counts, &autonomy, &completed, &blocked, tokens_in, tokens_out, cost,
            );
            let esc = md
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            format!(
                "<!DOCTYPE html><html><head><meta charset=\"utf-8\">{CSP_META}<title>AAF Overnight Digest</title></head><body><pre>{esc}</pre></body></html>"
            )
        }
    };
    let written = match emit_output(args, ctx, "handover-digest.html", &rendered) {
        Ok(p) => p,
        Err(OutError::Policy(why)) => return Ok(CmdOutput::policy("refused", why)),
        Err(OutError::Runtime(e)) => return Err(e),
    };
    if args.open
        && let Some(ref html_path) = written
    {
        open_html_file(html_path)?;
    }
    let text = match &written {
        Some(path) => format!("wrote {} ({} task(s))", path.display(), counts.total),
        None => rendered,
    };
    let mut out = CmdOutput::ok("ok", text)
        .field("mode", json!("digest"))
        .field("format", json!(format.as_str()))
        .field("digest", digest_json);
    if let Some(path) = written {
        out = out.field("out", json!(path.to_string_lossy().into_owned()));
    }
    Ok(out)
}

fn fmt_pct(pct: Option<f64>) -> String {
    pct.map_or_else(|| "n/a".to_string(), |v| format!("{v:.1}%"))
}

#[allow(clippy::too_many_arguments)]
fn render_digest_md(
    since: Option<u64>,
    c: &TaskCounts,
    a: &AutonomyMetrics,
    completed: &[Value],
    blocked: &[Value],
    tokens_in: u64,
    tokens_out: u64,
    cost: Option<f64>,
) -> String {
    let mut out = match since {
        Some(h) => format!("# AAF Overnight Digest (last {h}h)\n\n"),
        None => "# AAF Handover Summary\n\n".to_string(),
    };
    let _ = writeln!(
        out,
        "Counts: {} total (done={}, review={}, blocked={}, running={}, gating={}, ready={}, backlog={}, cancelled={})",
        c.total, c.done, c.review, c.blocked, c.running, c.gating, c.ready, c.backlog, c.cancelled
    );
    if c.adopting > 0 {
        // Shown only when present so the totals still add up (adopting is its own state).
        let _ = writeln!(out, "Adopting: {}", c.adopting);
    }
    let dur = a
        .gate_avg_duration_ms
        .map_or_else(String::new, |ms| format!(", avg {ms}ms"));
    let _ = writeln!(
        out,
        "Autonomy: {}/{} done without human ({}), gate first-pass: {}/{} ({}{dur}), cross-audit fail: {}/{} ({}), escalations: {}",
        a.done_without_human,
        a.done_tasks,
        fmt_pct(a.autonomy_rate_pct),
        a.gate_first_pass_clear,
        a.gate_tasks_evaluated,
        fmt_pct(a.gate_first_pass_rate_pct),
        a.cross_audit_failed,
        a.cross_audit_total,
        fmt_pct(a.cross_audit_fail_rate_pct),
        a.implementer_escalations,
    );
    match cost {
        Some(usd) => {
            let _ = writeln!(
                out,
                "Usage: {tokens_in} in + {tokens_out} out (~${usd:.4})\n"
            );
        }
        None => {
            let _ = writeln!(out, "Usage: {tokens_in} in + {tokens_out} out\n");
        }
    }
    out.push_str("## Completed / Review\n");
    if completed.is_empty() {
        out.push_str("- none\n");
    } else {
        for item in completed {
            let pr = item["pr_url"]
                .as_str()
                .map_or(String::new(), |u| format!(" — {u}"));
            let _ = writeln!(
                out,
                "- {} [{}] {}{pr}",
                item["id"].as_str().unwrap_or(""),
                item["state"].as_str().unwrap_or(""),
                item["title"].as_str().unwrap_or("")
            );
        }
    }
    out.push_str("\n## Blocked\n");
    if blocked.is_empty() {
        out.push_str("- none\n");
    } else {
        for item in blocked {
            let _ = writeln!(
                out,
                "- {} [{}] {}: {}",
                item["id"].as_str().unwrap_or(""),
                item["kind"].as_str().unwrap_or(""),
                item["title"].as_str().unwrap_or(""),
                item["reason"].as_str().unwrap_or("")
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::render_digest_md;
    use aaf_engine::telemetry::{AutonomyMetrics, TaskCounts};

    #[test]
    fn digest_markdown_renders_autonomy_and_gate_metrics() {
        let a = AutonomyMetrics {
            done_tasks: 2,
            done_without_human: 1,
            autonomy_rate_pct: Some(50.0),
            gate_tasks_evaluated: 2,
            gate_first_pass_clear: 1,
            gate_first_pass_rate_pct: Some(50.0),
            gate_avg_duration_ms: Some(1800),
            cross_audit_total: 2,
            cross_audit_failed: 1,
            cross_audit_fail_rate_pct: Some(50.0),
            implementer_escalations: 1,
        };
        let md = render_digest_md(Some(12), &TaskCounts::default(), &a, &[], &[], 10, 5, None);
        assert!(
            md.contains("Autonomy: 1/2 done without human (50.0%)"),
            "{md}"
        );
        assert!(
            md.contains("gate first-pass: 1/2 (50.0%, avg 1800ms)"),
            "{md}"
        );
        assert!(md.contains("cross-audit fail: 1/2 (50.0%)"), "{md}");
        assert!(md.contains("escalations: 1"), "{md}");
        assert!(md.contains("Usage: 10 in + 5 out\n"), "{md}");
        assert!(!md.contains('$'), "{md}");
    }
}
