//! Output assembly and fail-soft automatic DevKit handover report export for
//! `aaf run`.

use std::collections::HashSet;

use aaf_domain::config::RepoConfig;
use aaf_engine::db::Db;
use aaf_engine::factory::{BindingView, RunSummary};
use aaf_engine::handover::write_auto_handover_reports;
use serde_json::{Value, json};

use super::super::board::store::{err, open_db_read_only};
use super::{drain_stop_reason, waiting_line};
use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput};

pub(super) fn report(
    summary: &RunSummary,
    warnings: &[String],
    binding: &BindingView,
) -> Result<CmdOutput, CmdError> {
    let mut text = format!(
        "{} agent run(s), {} gate run(s): {} done, {} in review, {} blocked, {} back to ready, \
         {} waiting for an account{}",
        summary.agent_runs,
        summary.gate_runs,
        summary.done.len(),
        summary.review.len(),
        summary.blocked.len(),
        summary.requeued.len(),
        summary.waiting_for_account,
        if summary.frozen {
            "; factory is FROZEN: aaf board unfreeze"
        } else {
            ""
        }
    );
    if summary.waiting_for_review_cap > 0 {
        text.push_str(&format!(
            "\nwaiting for review cap: {} (run.max_open_review reached)",
            summary.waiting_for_review_cap
        ));
    }
    if !summary.blocked_upstream.is_empty() {
        let pairs: Vec<String> = summary
            .blocked_upstream
            .iter()
            .map(|(task, blocker)| format!("{task} <- {blocker}"))
            .collect();
        text.push_str(&format!("\nblocked upstream: {}", pairs.join(", ")));
    }
    if let Some(line) = waiting_line(summary.waiting_for_account, binding) {
        text.push('\n');
        text.push_str(&line);
    }
    // PLAN C.10: 2 = Blocked. A task that ended Blocked needs a human, and
    // every blocking error blocks its task; what is left in `errors` beside
    // that (a pull request that could not be opened, a leftover worktree)
    // stopped nothing and keeps exit 0.
    // A halted run needs a person too (a login, the network), and its task
    // is in Ready, not Blocked: said apart so it is not taken for an idle
    // board.
    let mut out = if summary.frozen {
        CmdOutput::policy("frozen", text)
    } else if let Some(why) = &summary.halted {
        CmdOutput::policy("halted", format!("{text}\nstopped: {why}"))
    } else if !summary.blocked.is_empty() {
        let ids = summary.blocked.join(", ");
        CmdOutput::policy(
            "blocked",
            format!("{text}\nblocked: {ids} (aaf board show <id> says why)"),
        )
    } else {
        CmdOutput::ok("ok", text)
    };
    let Value::Object(fields) = serde_json::to_value(summary).map_err(|e| err("summary", e))?
    else {
        return Err(CmdError::new("summary is not an object"));
    };
    out.fields.extend(fields);
    Ok(out
        .field("drain_stop_reason", json!(drain_stop_reason(summary)))
        .field("warnings", json!(warnings))
        .field("account_binding", json!(binding)))
}

/// Automatically writes DevKit HTML + Markdown handover reports for every task
/// that reached `done` or `review` in `summary` (deduped, fail-soft).
pub(super) fn attach_auto_handover_reports(
    mut out: CmdOutput,
    summary: &RunSummary,
    ctx: &Ctx,
    db: &Db,
    repo_cfg: &RepoConfig,
) -> CmdOutput {
    let mut seen = HashSet::new();
    let mut reports = Vec::new();
    for task_id in summary.done.iter().chain(summary.review.iter()) {
        if !seen.insert(task_id.as_str()) {
            continue;
        }
        match write_auto_handover_reports(&ctx.data_dir, db, task_id, repo_cfg) {
            Ok(paths) => {
                out.text.push_str(&format!(
                    "\nhandover ({task_id}, {}): html={} md={}",
                    paths.status,
                    paths.html.display(),
                    paths.markdown.display()
                ));
                reports.push(json!({
                    "task_id": task_id,
                    "status": paths.status,
                    "html": paths.html.to_string_lossy(),
                    "markdown": paths.markdown.to_string_lossy(),
                }));
            }
            Err(err) => {
                eprintln!("aaf: handover report skipped for {task_id}: {err}");
            }
        }
    }
    out.field("handover_reports", json!(reports))
}

/// Fail-soft incremental export for long-running `aaf run --auto`: writes
/// DevKit handover reports immediately when a task transitions to `Review` or
/// `Done` (or updates) instead of waiting for process exit.
pub(crate) fn flush_completed_reports_for_repo(
    ctx: &Ctx,
    db: &Db,
    repo_root: &str,
    repo_cfg: &RepoConfig,
    exported_ts: &mut std::collections::HashMap<String, i64>,
) {
    use aaf_domain::ports::TaskStore as _;
    use aaf_domain::task::TaskState;

    for state in [TaskState::Done, TaskState::Review] {
        let Ok(tasks) = db.list_tasks(Some(repo_root), Some(state)) else {
            continue;
        };
        for task in tasks {
            if exported_ts.get(&task.id) == Some(&task.updated_at) {
                continue;
            }
            if let Err(err) = write_auto_handover_reports(&ctx.data_dir, db, &task.id, repo_cfg) {
                eprintln!("aaf: handover report skipped for {}: {err}", task.id);
            } else {
                exported_ts.insert(task.id, task.updated_at);
            }
        }
    }
}

/// Fail-soft single-task DevKit handover report export for `aaf merge`.
/// Opens `aaf.db` read-only (never creates a new database file).
pub(crate) fn attach_single_handover_report(
    mut out: CmdOutput,
    ctx: &Ctx,
    task_id: Option<&str>,
    repo_cfg: &RepoConfig,
) -> CmdOutput {
    let Some(task_id) = task_id else {
        return out;
    };
    let db = match open_db_read_only(ctx) {
        Ok(Some(db)) => db,
        Ok(None) => return out,
        Err(e) => {
            eprintln!("aaf: handover report skipped for {task_id}: {e:?}");
            return out;
        }
    };
    match write_auto_handover_reports(&ctx.data_dir, &db, task_id, repo_cfg) {
        Ok(paths) => {
            out.text.push_str(&format!(
                "\n  handover ({}): html={} md={}",
                paths.status,
                paths.html.display(),
                paths.markdown.display()
            ));
            out.field(
                "handover_report",
                json!({
                    "task_id": task_id,
                    "status": paths.status,
                    "html": paths.html.to_string_lossy(),
                    "markdown": paths.markdown.to_string_lossy(),
                }),
            )
        }
        Err(e) => {
            eprintln!("aaf: handover report skipped for {task_id}: {e}");
            out
        }
    }
}
