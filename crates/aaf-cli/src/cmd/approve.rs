//! `aaf approve --reviewer <name>`: sign an approval for the locked files as
//! they are changed right now.
//!
//! This is the human's key to the head lock, so an agent must not be able to
//! turn it: it refuses inside an agent run (`AAF_RUN_ID`) and when stdin is
//! not a terminal.

#[cfg(test)]
mod tests;

use aaf_engine::gate::{GateError, head_lock};
use serde_json::json;

use crate::ctx::{Ctx, SystemClock};
use crate::output::{CmdError, CmdOutput, CmdResult};

#[derive(Debug, clap::Args)]
pub struct ApproveArgs {
    /// Who is signing off; recorded in the approval.
    #[arg(long)]
    pub reviewer: String,
}

fn refused(reason: String) -> CmdResult {
    Ok(CmdOutput::policy("refused", format!("refused: {reason}")).field("reason", json!(reason)))
}

pub fn run(args: &ApproveArgs, ctx: &Ctx) -> CmdResult {
    if let Some(run_id) = &ctx.run_id {
        return refused(format!(
            "AAF_RUN_ID is set ({run_id}): an agent run cannot approve its own changes"
        ));
    }
    if !ctx.stdin_is_tty {
        return refused("stdin is not a terminal: approvals are signed by a person".to_string());
    }
    let reviewer = args.reviewer.trim();
    if reviewer.is_empty() {
        return refused("the reviewer name is empty".to_string());
    }

    let repo_root = ctx.repo_root()?;
    let cfg = ctx.load_config(&repo_root)?;
    // Computes `lock_fp` for the current locked changes and stores the
    // approval through `aaf_fp::write_approval`.
    let approval = match head_lock::approve(
        &ctx.data_dir,
        &repo_root,
        &cfg.repo,
        reviewer,
        false,
        &SystemClock,
    ) {
        Ok(approval) => approval,
        Err(GateError::NothingLocked) => {
            return Ok(CmdOutput::policy(
                "nothing_to_approve",
                "no locked file is changed; there is nothing to approve",
            ));
        }
        Err(err) => return Err(CmdError::new(format!("approve: {err}"))),
    };
    let path = aaf_fp::approval_path(&ctx.data_dir, &repo_root, &approval.lock_fp);
    Ok(CmdOutput::ok(
        "approved",
        format!(
            "approved by {} for lock {}\n  approval: {}",
            approval.reviewer,
            approval.lock_fp,
            path.display()
        ),
    )
    .field("lock_fp", json!(approval.lock_fp))
    .field("reviewer", json!(approval.reviewer))
    .field("created_at", json!(approval.created_at))
    .field("approval_path", json!(path)))
}
