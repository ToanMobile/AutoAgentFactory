//! `aaf proof record|verify|show`: verify and sign mobile MCP capture artifacts.

use std::path::PathBuf;

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{Clock, EventSink};
use aaf_engine::factory::adapters::SystemClock;
use aaf_engine::proof::{ProofError, ProofInput, record_proof, show_proof, verify_proof};
use serde_json::json;

use super::board::store::{err, open_db, refuse_inside_agent_run, repo_root};
use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput, CmdResult};

#[derive(Debug, clap::Args)]
pub struct ProofArgs {
    #[command(subcommand)]
    pub command: ProofCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum ProofCommand {
    /// Validate a captured PNG screenshot and bind an HMAC-signed proof artifact to tree_fp.
    Record(RecordArgs),
    /// Verify the signed proof artifact matches the repository, tree_fp, policy and live image.
    Verify(TreeArgs),
    /// Print the signed proof artifact for the current or given tree_fp.
    Show(TreeArgs),
}

#[derive(Debug, clap::Args)]
pub struct RecordArgs {
    #[arg(long)]
    pub image: PathBuf,
    #[arg(long, default_value = "mobile-mcp:mobile_save_screenshot")]
    pub tool: String,
    #[arg(long)]
    pub device: String,
    #[arg(long)]
    pub task_id: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct TreeArgs {
    #[arg(long)]
    pub tree_fp: Option<String>,
}

pub fn run(args: &ProofArgs, ctx: &Ctx) -> CmdResult {
    match &args.command {
        ProofCommand::Record(rec) => record(rec, ctx),
        ProofCommand::Verify(tree) => verify(tree, ctx),
        ProofCommand::Show(tree) => show(tree, ctx),
    }
}

fn proof_err(error: ProofError) -> CmdError {
    CmdError::new(error.to_string())
}

fn record(args: &RecordArgs, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf proof record") {
        return Ok(refused);
    }
    let repo = repo_root(ctx)?;
    let cfg = ctx.load_config(&repo)?;
    let now = SystemClock.now();
    let recorded = match record_proof(
        &repo,
        &ctx.data_dir,
        &cfg.repo.proof,
        &ProofInput {
            image_path: args.image.clone(),
            source_tool: args.tool.clone(),
            device_serial: args.device.clone(),
            task_id: args.task_id.clone(),
            now,
        },
    ) {
        Ok(recorded) => recorded,
        Err(ProofError::Invalid(reason) | ProofError::DeniedDevice(reason)) => {
            return Ok(CmdOutput::policy(
                "BLOCKED",
                format!("proof BLOCKED: {reason}"),
            ));
        }
        Err(other) => return Err(proof_err(other)),
    };

    if let Some(task_id) = &args.task_id {
        let db = open_db(ctx)?;
        let payload = json!({
            "tree_fp": recorded.record.tree_fp,
            "image_sha256": recorded.record.image_sha256,
            "device_serial": recorded.record.device_serial,
            "source_tool": recorded.record.source_tool,
        });
        db.emit(
            &FactoryEvent::new(EventKind::ProofRecorded, payload.to_string(), now)
                .for_task(task_id),
        )
        .map_err(|e| CmdError::new(e.to_string()))?;
    }

    Ok(CmdOutput::ok(
        "ok",
        format!(
            "recorded proof {}x{} sha256={} tree_fp={}",
            recorded.record.width,
            recorded.record.height,
            recorded.record.image_sha256,
            recorded.record.tree_fp
        ),
    )
    .field(
        "proof_path",
        json!(recorded.proof_path.to_string_lossy().into_owned()),
    )
    .field(
        "proof",
        serde_json::to_value(&recorded.record).map_err(|e| err("proof", e))?,
    ))
}

fn verify(args: &TreeArgs, ctx: &Ctx) -> CmdResult {
    let repo = repo_root(ctx)?;
    let cfg = ctx.load_config(&repo)?;
    match verify_proof(
        &repo,
        &ctx.data_dir,
        &cfg.repo.proof,
        args.tree_fp.as_deref(),
    ) {
        Ok(record) => Ok(CmdOutput::ok(
            "CLEAR",
            format!(
                "proof CLEAR {}x{} sha256={}",
                record.width, record.height, record.image_sha256
            ),
        )
        .field(
            "proof",
            serde_json::to_value(&record).map_err(|e| err("proof", e))?,
        )),
        Err(ProofError::Invalid(reason) | ProofError::DeniedDevice(reason)) => Ok(
            CmdOutput::policy("BLOCKED", format!("proof BLOCKED: {reason}")),
        ),
        Err(ProofError::Fingerprint(fp_err)) => Ok(CmdOutput::policy(
            "BLOCKED",
            format!("proof BLOCKED: {fp_err}"),
        )),
        Err(other) => Err(proof_err(other)),
    }
}

fn show(args: &TreeArgs, ctx: &Ctx) -> CmdResult {
    let repo = repo_root(ctx)?;
    let record = show_proof(&repo, &ctx.data_dir, args.tree_fp.as_deref()).map_err(proof_err)?;
    Ok(CmdOutput::ok(
        "ok",
        format!(
            "proof {}x{} device={} tool={} tree_fp={}",
            record.width, record.height, record.device_serial, record.source_tool, record.tree_fp
        ),
    )
    .field(
        "proof",
        serde_json::to_value(&record).map_err(|e| err("proof", e))?,
    ))
}
