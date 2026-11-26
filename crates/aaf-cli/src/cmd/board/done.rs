//! `aaf board done <id> --reason <text> [--pr <num>] [--commit <sha>] [--force]`:
//! atomically marks a non-active task as `Done` (human action, e.g. when its PR
//! or changes were merged outside `aaf merge`).

use aaf_domain::ports::Clock;
use aaf_domain::task::TaskState;
use aaf_engine::db::{DoneOptions, DoneRefusal};
use aaf_engine::factory::adapters::SystemClock;
use serde_json::json;

use super::store::{open_db, refuse_inside_agent_run};
use super::{clean, port};
use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput, CmdResult};

#[derive(Debug, clap::Args)]
pub struct DoneArgs {
    /// Task id to mark Done.
    pub id: String,
    /// Required audit reason recorded on the TaskStateChanged event.
    #[arg(long)]
    pub reason: String,
    /// Optional merged pull request number recorded on TaskStateChanged and MergeResult.
    #[arg(long)]
    pub pr: Option<u64>,
    /// Optional merged commit SHA (7..=40 hex chars) recorded on TaskStateChanged and MergeResult.
    #[arg(long)]
    pub commit: Option<String>,
    /// Allow marking an idle Gating task, reclassifying a Cancelled task, or
    /// closing a parent task with unfinished subtasks.
    #[arg(long)]
    pub force: bool,
}

fn validate_commit(raw: &str) -> Result<String, CmdError> {
    let trimmed = raw.trim();
    if !(7..=40).contains(&trimmed.len()) || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(CmdError::new(
            "--commit must be 7 to 40 hexadecimal characters",
        ));
    }
    Ok(trimmed.to_string())
}

pub fn run(args: &DoneArgs, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf board done") {
        return Ok(refused);
    }
    let reason = clean("--reason", &args.reason)?;
    if let Some(0) = args.pr {
        return Err(CmdError::new("--pr must be a positive pull request number"));
    }
    let commit = args.commit.as_deref().map(validate_commit).transpose()?;

    let db = open_db(ctx)?;
    let now = SystemClock.now();
    let opts = DoneOptions {
        reason: &reason,
        pr_number: args.pr,
        commit_sha: commit.as_deref(),
        force: args.force,
    };

    let outcome = match db
        .complete_task_manual_atomic(&args.id, &opts, now)
        .map_err(port)?
    {
        Ok(outcome) => outcome,
        Err(refusal) => return Ok(format_refusal(&args.id, refusal)),
    };

    let mut lines = vec![format!(
        "done {} (from {})",
        outcome.task_id,
        outcome.previous_state.as_str()
    )];
    if !outcome.unblocked_dependents.is_empty() {
        lines.push(format!(
            "dependents now satisfied on this edge: {}",
            outcome.unblocked_dependents.join(", ")
        ));
    }

    let mut out = CmdOutput::ok("done", lines.join("\n"))
        .field("task_id", json!(outcome.task_id))
        .field("previous_state", json!(outcome.previous_state))
        .field("state", json!(TaskState::Done))
        .field("reason", json!(reason))
        .field("unblocked_dependents", json!(outcome.unblocked_dependents));
    if let Some(pr) = args.pr {
        out = out.field("pr_number", json!(pr));
    }
    if let Some(sha) = commit {
        out = out.field("commit_sha", json!(sha));
    }
    if let Some(parent_id) = outcome.parent_id {
        out = out.field("parent_id", json!(parent_id));
    }
    Ok(out)
}

fn format_refusal(target_id: &str, refusal: DoneRefusal) -> CmdOutput {
    match refusal {
        DoneRefusal::ActiveState { task_id, state } => {
            let hint = if state == TaskState::Gating {
                "stop `aaf run` first, or pass --force if no run is active"
            } else {
                "stop `aaf run` first"
            };
            let text = format!(
                "{task_id} is {}; cannot mark an active task done ({hint})",
                state.as_str()
            );
            CmdOutput::policy("refused", text)
                .field("task_id", json!(task_id))
                .field("state", json!(state))
        }
        DoneRefusal::AlreadyDone { task_id } => {
            CmdOutput::policy("refused", format!("{task_id} is already done"))
                .field("task_id", json!(task_id))
                .field("state", json!(TaskState::Done))
        }
        DoneRefusal::CancelledRequiresForce { task_id } => CmdOutput::policy(
            "refused",
            format!("{task_id} is cancelled; pass --force to reclassify it as done"),
        )
        .field("task_id", json!(task_id))
        .field("state", json!(TaskState::Cancelled)),
        DoneRefusal::RunningRun { task_id, run_id } => CmdOutput::policy(
            "refused",
            format!(
                "{task_id} has a running run ({run_id}); cannot mark done while a run is active"
            ),
        )
        .field("task_id", json!(task_id))
        .field("run_id", json!(run_id)),
        DoneRefusal::ActiveDescendant { child_id, state } => CmdOutput::policy(
            "refused",
            format!(
                "cannot mark {target_id} done: subtask {child_id} is {}",
                state.as_str()
            ),
        )
        .field("task_id", json!(target_id))
        .field("child_id", json!(child_id))
        .field("state", json!(state)),
        DoneRefusal::RunningDescendantRun { child_id, run_id } => CmdOutput::policy(
            "refused",
            format!("cannot mark {target_id} done: subtask {child_id} has a running run ({run_id})"),
        )
        .field("task_id", json!(target_id))
        .field("child_id", json!(child_id))
        .field("run_id", json!(run_id)),
        DoneRefusal::HasUnfinishedChildren { children } => CmdOutput::policy(
            "refused",
            format!(
                "{target_id} has {} unfinished subtask(s) ({}); complete them first or pass --force",
                children.len(),
                children.join(", ")
            ),
        )
        .field("task_id", json!(target_id))
        .field("children", json!(children)),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use aaf_domain::ports::TaskStore;
    use aaf_domain::task::{ImmutableSpec, Priority, Task};
    use aaf_engine::db::Db;

    use super::*;

    fn test_ctx(tmp: &tempfile::TempDir, run_id: Option<&str>) -> Ctx {
        Ctx {
            cwd: tmp.path().to_path_buf(),
            home: tmp.path().join("home"),
            data_dir: tmp.path().join("data"),
            config_dir: tmp.path().join("config"),
            worktrees_dir: tmp.path().join("worktrees"),
            path_var: std::ffi::OsString::new(),
            run_id: run_id.map(str::to_string),
            stdin_is_tty: false,
            aaf_env: Vec::new(),
        }
    }

    #[test]
    fn board_done_refuses_inside_agent_run_and_validates_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_ctx = test_ctx(&tmp, Some("run-123"));
        let out = run(
            &DoneArgs {
                id: "t-1".into(),
                reason: "merged".into(),
                pr: None,
                commit: None,
                force: false,
            },
            &agent_ctx,
        )
        .unwrap();
        assert_eq!(out.status, "refused");

        let human_ctx = test_ctx(&tmp, None);
        let err = run(
            &DoneArgs {
                id: "t-1".into(),
                reason: "merged".into(),
                pr: None,
                commit: Some("not-a-sha!".into()),
                force: false,
            },
            &human_ctx,
        )
        .unwrap_err();
        assert!(err.message.contains("--commit"));
    }

    #[test]
    fn board_done_transitions_review_and_reclassifies_cancelled_with_force() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(&tmp, None);
        std::fs::create_dir_all(&ctx.data_dir).unwrap();
        let db = Db::open(&ctx.data_dir.join("aaf.db")).unwrap();
        let spec = ImmutableSpec {
            description: "spec".into(),
            acceptance: vec!["ok".into()],
        };
        let mut t1 = Task::new_root("t-1", "/repo", "task 1", spec.clone(), Priority::P2, 10);
        t1.state = TaskState::Review;
        let mut t2 = Task::new_root("t-2", "/repo", "task 2", spec, Priority::P2, 10);
        t2.state = TaskState::Cancelled;
        db.insert_task(&t1).unwrap();
        db.insert_task(&t2).unwrap();
        drop(db);

        let out = run(
            &DoneArgs {
                id: "t-1".into(),
                reason: "Squash-merged PR #455".into(),
                pr: Some(455),
                commit: Some("bcbd084e".into()),
                force: false,
            },
            &ctx,
        )
        .unwrap();
        assert_eq!(out.status, "done");

        let refused = run(
            &DoneArgs {
                id: "t-2".into(),
                reason: "Already merged".into(),
                pr: None,
                commit: None,
                force: false,
            },
            &ctx,
        )
        .unwrap();
        assert_eq!(refused.status, "refused");

        let forced = run(
            &DoneArgs {
                id: "t-2".into(),
                reason: "Already merged".into(),
                pr: Some(444),
                commit: Some("51f59617".into()),
                force: true,
            },
            &ctx,
        )
        .unwrap();
        assert_eq!(forced.status, "done");

        let db = Db::open(&ctx.data_dir.join("aaf.db")).unwrap();
        assert_eq!(db.get_task("t-1").unwrap().state, TaskState::Done);
        assert_eq!(db.get_task("t-2").unwrap().state, TaskState::Done);
    }
}
