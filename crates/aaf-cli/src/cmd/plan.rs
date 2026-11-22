//! `aaf plan "<goal>" [--file path]`: build a strictly read-only planning prompt.

use aaf_domain::ports::PortError;
use aaf_engine::plan_mode::build_plan_prompt;
use serde_json::json;

use super::board::store::{err, repo_root};
use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput, CmdResult};

#[derive(Debug, clap::Args)]
pub struct PlanArgs {
    /// Goal or problem statement to plan without editing code.
    pub goal: String,
    /// Relative context file inside the repository; repeat for more.
    #[arg(long = "file")]
    pub files: Vec<String>,
    /// Print the read-only plan prompt (always read-only; no agent is launched).
    #[arg(long)]
    pub dry_run: bool,
}

pub fn run(args: &PlanArgs, ctx: &Ctx) -> CmdResult {
    let repo = repo_root(ctx)?;
    let built = match build_plan_prompt(&repo, &args.goal, &args.files) {
        Ok(built) => built,
        Err(PortError::Invalid(msg)) => return Ok(CmdOutput::policy("refused", msg)),
        Err(other) => return Err(CmdError::new(other.to_string())),
    };

    let value = serde_json::to_value(&built).map_err(|e| err("plan", e))?;
    Ok(CmdOutput::ok("ok", built.prompt.clone())
        .field("read_only", json!(built.read_only))
        .field("dry_run", json!(args.dry_run))
        .field("plan", value)
        .field("prompt", json!(built.prompt)))
}
