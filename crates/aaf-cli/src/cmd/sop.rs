//! `aaf sop`: declarative SOP workflow inspection, scaffolding, validation, execution, and approvals.

use std::path::{Path, PathBuf};
use std::time::Duration;

use aaf_domain::gate::GateStatus;
use aaf_domain::ports::Clock;
use aaf_domain::sop::{
    DEFAULT_SCRIPT_TIMEOUT_SECS, SopCheckpoint, SopRunStatus, SopStage, StageKind, StageStatus,
};
use aaf_engine::process::{CommandRunner, SystemRunner};
use aaf_engine::sop::runner::{
    StageDriver, StageOutcome, approve_human_gate, checkpoint_path, list_checkpoints,
    load_checkpoint, ready_stages, run_sop_persisted, save_checkpoint, send_back,
};
use aaf_engine::sop::{
    init_sop_file, list_all_sops, load_repo_sops, load_sop_file, render_ascii_dag, resolve_sop,
};
use aaf_engine::throttle::Progress;
use serde_json::json;

use super::board::store::{refuse_inside_agent_run, repo_root};
use super::gate::progress_line;
use crate::ctx::{Ctx, SystemClock};
use crate::output::{CmdError, CmdOutput, CmdResult, notice};

#[derive(Debug, clap::Args)]
pub struct SopArgs {
    #[command(subcommand)]
    pub command: SopCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum SopCommand {
    /// List available SOP workflows (repo `.autofactory/sops/*.toml` and built-in blueprints).
    List,
    /// Show an SOP workflow's ASCII DAG and stage details.
    Show {
        /// SOP name to inspect.
        name: String,
    },
    /// Scaffold a built-in SOP blueprint into `.autofactory/sops/<name>.toml`.
    Init {
        /// Built-in blueprint name (`backend`, `frontend`, `mobile-android`, `unity-game`, `bugfix`).
        blueprint: String,
        /// Optional custom SOP name (defaults to the blueprint name).
        #[arg(long)]
        name: Option<String>,
        /// Overwrite an existing `.autofactory/sops/<name>.toml` file.
        #[arg(long)]
        force: bool,
    },
    /// Validate an SOP TOML file or all SOPs under `.autofactory/sops/`.
    Validate {
        /// Optional path to a specific SOP TOML file.
        path: Option<PathBuf>,
    },
    /// List saved SOP run checkpoints and their current status.
    Runs,
    /// Execute or resume an SOP workflow with crash-safe checkpointing.
    Run {
        /// SOP name to run.
        name: String,
        /// Existing checkpoint `run_id` to resume from.
        #[arg(long)]
        resume: Option<String>,
        /// Validate and print the execution plan without running stages or writing checkpoints.
        #[arg(long)]
        dry_run: bool,
    },
    /// Approve a paused `WaitingHuman` stage so the run can be resumed.
    Approve {
        /// Checkpoint `run_id` waiting for human approval.
        run_id: String,
        /// Reviewer name recorded in the checkpoint audit log.
        #[arg(long)]
        reviewer: String,
    },
    /// Send a run back to an earlier stage, resetting it and all downstream dependents.
    SendBack {
        /// Checkpoint `run_id` to rewind.
        run_id: String,
        /// Target stage id to restart from.
        #[arg(long)]
        to: String,
        /// Reason for sending the run back.
        #[arg(long)]
        reason: String,
    },
}

pub fn run(args: &SopArgs, ctx: &Ctx) -> CmdResult {
    match &args.command {
        SopCommand::List => cmd_list(ctx),
        SopCommand::Show { name } => cmd_show(name, ctx),
        SopCommand::Init {
            blueprint,
            name,
            force,
        } => cmd_init(blueprint, name.as_deref(), *force, ctx),
        SopCommand::Validate { path } => cmd_validate(path.as_deref(), ctx),
        SopCommand::Runs => cmd_runs(ctx),
        SopCommand::Run {
            name,
            resume,
            dry_run,
        } => cmd_run(name, resume.as_deref(), *dry_run, ctx),
        SopCommand::Approve { run_id, reviewer } => cmd_approve(run_id, reviewer, ctx),
        SopCommand::SendBack { run_id, to, reason } => cmd_send_back(run_id, to, reason, ctx),
    }
}

fn cmd_list(ctx: &Ctx) -> CmdResult {
    let repo = repo_root(ctx).ok();
    let sops = list_all_sops(repo.as_deref()).map_err(|e| CmdError::new(e.to_string()))?;
    let lines: Vec<String> = sops
        .iter()
        .map(|d| {
            format!(
                "{:<18} profile={:<10} stages={:<2} reworks={:<2} [{}] {}",
                d.spec.name,
                d.spec.profile,
                d.spec.stages.len(),
                d.spec.max_reworks,
                d.source.label(),
                d.spec.description
            )
        })
        .collect();
    let rows: Vec<_> = sops
        .iter()
        .map(|d| {
            json!({
                "name": d.spec.name,
                "version": d.spec.version,
                "profile": d.spec.profile,
                "stages": d.spec.stages.len(),
                "max_reworks": d.spec.max_reworks,
                "description": d.spec.description,
                "source": d.source.label(),
            })
        })
        .collect();
    Ok(CmdOutput::ok("ok", lines.join("\n"))
        .field("count", json!(sops.len()))
        .field("sops", json!(rows)))
}

fn cmd_show(name: &str, ctx: &Ctx) -> CmdResult {
    let repo = repo_root(ctx).ok();
    let discovered =
        resolve_sop(repo.as_deref(), name).map_err(|e| CmdError::new(e.to_string()))?;
    let order = discovered
        .spec
        .validate()
        .map_err(|e| CmdError::new(e.to_string()))?;
    let dag = render_ascii_dag(&discovered.spec).map_err(|e| CmdError::new(e.to_string()))?;
    let text = format!("{dag}\nsource: {}", discovered.source.label());
    let spec_json = serde_json::to_value(&discovered.spec)
        .map_err(|e| CmdError::new(format!("serialize spec: {e}")))?;
    Ok(CmdOutput::ok("ok", text)
        .field("name", json!(discovered.spec.name))
        .field("source", json!(discovered.source.label()))
        .field("order", json!(order))
        .field("dag", json!(dag))
        .field("spec", spec_json))
}

fn cmd_init(blueprint: &str, name: Option<&str>, force: bool, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf sop init") {
        return Ok(refused);
    }
    let root = repo_root(ctx)?;
    let path =
        init_sop_file(&root, blueprint, name, force).map_err(|e| CmdError::new(e.to_string()))?;
    let sop_name = name.unwrap_or(blueprint);
    let text = format!("initialized SOP '{sop_name}' at {}", path.display());
    Ok(CmdOutput::ok("ok", text)
        .field("blueprint", json!(blueprint))
        .field("name", json!(sop_name))
        .field("path", json!(path.to_string_lossy().into_owned())))
}

fn cmd_validate(path: Option<&Path>, ctx: &Ctx) -> CmdResult {
    if let Some(file) = path {
        let spec = load_sop_file(file).map_err(|e| CmdError::new(e.to_string()))?;
        let order = spec.validate().map_err(|e| CmdError::new(e.to_string()))?;
        let text = format!(
            "valid: {} ({} stages: {})",
            file.display(),
            order.len(),
            order.join(" -> ")
        );
        return Ok(CmdOutput::ok("ok", text)
            .field("name", json!(spec.name))
            .field("path", json!(file.to_string_lossy().into_owned()))
            .field("order", json!(order))
            .field("valid_count", json!(1)));
    }

    let root = repo_root(ctx)?;
    let repo_sops = load_repo_sops(&root).map_err(|e| CmdError::new(e.to_string()))?;
    if repo_sops.is_empty() {
        return Ok(
            CmdOutput::ok("ok", "no custom SOP files found under .autofactory/sops/")
                .field("valid_count", json!(0))
                .field("sops", json!([])),
        );
    }
    let mut lines = Vec::with_capacity(repo_sops.len());
    let mut items = Vec::with_capacity(repo_sops.len());
    for discovered in &repo_sops {
        let order = discovered
            .spec
            .validate()
            .map_err(|e| CmdError::new(e.to_string()))?;
        lines.push(format!(
            "valid: {} ({} stages) [{}]",
            discovered.spec.name,
            order.len(),
            discovered.source.label()
        ));
        items.push(json!({
            "name": discovered.spec.name,
            "source": discovered.source.label(),
            "order": order,
        }));
    }
    Ok(CmdOutput::ok("ok", lines.join("\n"))
        .field("valid_count", json!(repo_sops.len()))
        .field("sops", json!(items)))
}

fn cmd_runs(ctx: &Ctx) -> CmdResult {
    let runs = list_checkpoints(&ctx.data_dir).map_err(|e| CmdError::new(e.to_string()))?;
    if runs.is_empty() {
        return Ok(CmdOutput::ok("ok", "no SOP runs recorded")
            .field("count", json!(0))
            .field("runs", json!([])));
    }
    let mut lines = Vec::with_capacity(runs.len());
    let mut rows = Vec::with_capacity(runs.len());
    for cp in &runs {
        let passed = cp
            .stage_states
            .values()
            .filter(|r| r.status == StageStatus::Passed)
            .count();
        let total = cp.stage_states.len();
        lines.push(format!(
            "{}  sop={:<16} status={:<13} stages={}/{}  reworks={}  updated={}",
            cp.run_id,
            cp.sop_name,
            run_status_str(cp.status),
            passed,
            total,
            cp.rework_count,
            cp.updated_at
        ));
        rows.push(json!({
            "run_id": cp.run_id,
            "sop_name": cp.sop_name,
            "repo_root": cp.repo_root,
            "status": cp.status,
            "passed_stages": passed,
            "total_stages": total,
            "rework_count": cp.rework_count,
            "created_at": cp.created_at,
            "updated_at": cp.updated_at,
        }));
    }
    Ok(CmdOutput::ok("ok", lines.join("\n"))
        .field("count", json!(runs.len()))
        .field("runs", json!(rows)))
}

fn cmd_run(name: &str, resume: Option<&str>, dry_run: bool, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf sop run") {
        return Ok(refused);
    }
    let repo_opt = repo_root(ctx).ok();
    let discovered =
        resolve_sop(repo_opt.as_deref(), name).map_err(|e| CmdError::new(e.to_string()))?;
    let spec = discovered.spec;
    let order = spec.validate().map_err(|e| CmdError::new(e.to_string()))?;

    if dry_run {
        let dag = render_ascii_dag(&spec).map_err(|e| CmdError::new(e.to_string()))?;
        let text = format!(
            "DRY RUN (no stages executed, no checkpoint written)\n{dag}\nexecution order: {}",
            order.join(" -> ")
        );
        return Ok(CmdOutput::ok("dry_run", text)
            .field("sop_name", json!(spec.name))
            .field("dry_run", json!(true))
            .field("order", json!(order))
            .field("dag", json!(dag)));
    }

    let root = repo_root(ctx)?;
    let root_str = root.to_string_lossy().into_owned();
    let now = SystemClock.now();
    let mut cp = match resume {
        Some(run_id) => {
            let loaded =
                load_checkpoint(&ctx.data_dir, run_id).map_err(|e| CmdError::new(e.to_string()))?;
            if loaded.sop_name != spec.name {
                return Err(CmdError::new(format!(
                    "checkpoint {run_id:?} belongs to SOP {:?}, not {:?}",
                    loaded.sop_name, spec.name
                )));
            }
            loaded
        }
        None => {
            let run_id = next_run_id(&ctx.data_dir, &spec.name, now);
            SopCheckpoint::new(run_id, &spec, root_str, now)
        }
    };

    let mut driver = CliStageDriver {
        ctx,
        repo_root: &root,
    };
    let final_status = run_sop_persisted(&spec, &mut cp, &ctx.data_dir, &mut driver, &SystemClock)
        .map_err(|e| CmdError::new(e.to_string()))?;
    let cp_path =
        checkpoint_path(&ctx.data_dir, &cp.run_id).map_err(|e| CmdError::new(e.to_string()))?;
    let next_ready: Vec<String> = ready_stages(&spec, &cp)
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.id.clone())
        .collect();

    let status_word = run_status_str(final_status);
    let mut lines = vec![format!(
        "SOP '{}' run {} -> {} (reworks={})",
        cp.sop_name, cp.run_id, status_word, cp.rework_count
    )];
    for stage_id in &order {
        if let Some(rec) = cp.stage_states.get(stage_id) {
            let detail = rec.summary.as_deref().unwrap_or("");
            lines.push(format!(
                "  {:<18} {:?} (attempts={}) {}",
                stage_id, rec.status, rec.attempts, detail
            ));
        }
    }
    if final_status == SopRunStatus::WaitingHuman {
        lines.push(format!(
            "next: run `aaf sop approve {} --reviewer <name>` then `aaf sop run {} --resume {}`",
            cp.run_id, cp.sop_name, cp.run_id
        ));
    }
    lines.push(format!("checkpoint: {}", cp_path.display()));

    let cp_json = serde_json::to_value(&cp)
        .map_err(|e| CmdError::new(format!("serialize checkpoint: {e}")))?;
    let out = if final_status == SopRunStatus::Failed {
        CmdOutput::policy(status_word, lines.join("\n"))
    } else {
        CmdOutput::ok(status_word, lines.join("\n"))
    };
    Ok(out
        .field("run_id", json!(cp.run_id))
        .field("sop_name", json!(cp.sop_name))
        .field("run_status", json!(final_status))
        .field("rework_count", json!(cp.rework_count))
        .field("next_ready", json!(next_ready))
        .field(
            "checkpoint_path",
            json!(cp_path.to_string_lossy().into_owned()),
        )
        .field("checkpoint", cp_json))
}

fn cmd_approve(run_id: &str, reviewer: &str, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf sop approve") {
        return Ok(refused);
    }
    let mut cp =
        load_checkpoint(&ctx.data_dir, run_id).map_err(|e| CmdError::new(e.to_string()))?;
    let stage_id = approve_human_gate(&mut cp, reviewer, &SystemClock)
        .map_err(|e| CmdError::new(e.to_string()))?;
    let cp_path = save_checkpoint(&ctx.data_dir, &cp).map_err(|e| CmdError::new(e.to_string()))?;
    let text = format!(
        "approved stage '{stage_id}' on run '{}' by {reviewer}\nresume with: aaf sop run {} --resume {}",
        cp.run_id, cp.sop_name, cp.run_id
    );
    Ok(CmdOutput::ok("ok", text)
        .field("run_id", json!(cp.run_id))
        .field("sop_name", json!(cp.sop_name))
        .field("approved_stage", json!(stage_id))
        .field("reviewer", json!(reviewer))
        .field(
            "checkpoint_path",
            json!(cp_path.to_string_lossy().into_owned()),
        ))
}

fn cmd_send_back(run_id: &str, to: &str, reason: &str, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf sop send-back") {
        return Ok(refused);
    }
    let mut cp =
        load_checkpoint(&ctx.data_dir, run_id).map_err(|e| CmdError::new(e.to_string()))?;
    let cp_repo = (!cp.repo_root.is_empty()).then(|| PathBuf::from(&cp.repo_root));
    let repo_opt = cp_repo.or_else(|| repo_root(ctx).ok());
    let discovered =
        resolve_sop(repo_opt.as_deref(), &cp.sop_name).map_err(|e| CmdError::new(e.to_string()))?;
    send_back(&discovered.spec, &mut cp, to, reason, &SystemClock)
        .map_err(|e| CmdError::new(e.to_string()))?;
    let cp_path = save_checkpoint(&ctx.data_dir, &cp).map_err(|e| CmdError::new(e.to_string()))?;
    let text = format!(
        "sent run '{}' back to stage '{to}' (reworks={})\nresume with: aaf sop run {} --resume {}",
        cp.run_id, cp.rework_count, cp.sop_name, cp.run_id
    );
    Ok(CmdOutput::ok("ok", text)
        .field("run_id", json!(cp.run_id))
        .field("sop_name", json!(cp.sop_name))
        .field("to_stage", json!(to))
        .field("reason", json!(reason))
        .field("rework_count", json!(cp.rework_count))
        .field(
            "checkpoint_path",
            json!(cp_path.to_string_lossy().into_owned()),
        ))
}

fn run_status_str(status: SopRunStatus) -> &'static str {
    match status {
        SopRunStatus::Running => "running",
        SopRunStatus::WaitingHuman => "waiting_human",
        SopRunStatus::Completed => "completed",
        SopRunStatus::Failed => "failed",
    }
}

fn next_run_id(data_dir: &Path, sop_name: &str, now: i64) -> String {
    let slug: String = sop_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .take(32)
        .collect();
    let base = format!("sop-{slug}-{now}-{}", std::process::id());
    if checkpoint_path(data_dir, &base).is_ok_and(|p| !p.exists()) {
        return base;
    }
    for n in 1..1000u32 {
        let candidate = format!("{base}-{n}");
        if checkpoint_path(data_dir, &candidate).is_ok_and(|p| !p.exists()) {
            return candidate;
        }
    }
    base
}

struct CliStageDriver<'a> {
    ctx: &'a Ctx,
    repo_root: &'a Path,
}

impl StageDriver for CliStageDriver<'_> {
    fn run_stage(&mut self, stage: &SopStage, _cp: &SopCheckpoint) -> StageOutcome {
        match &stage.kind {
            StageKind::Script {
                command,
                heavy,
                timeout_secs,
            } => {
                let secs = if *timeout_secs == 0 {
                    DEFAULT_SCRIPT_TIMEOUT_SECS
                } else {
                    *timeout_secs
                };
                if *heavy {
                    notice(
                        "sop",
                        &format!("running heavy script stage '{}': {command}", stage.id),
                    );
                } else {
                    notice("sop", &format!("running script stage '{}'", stage.id));
                }
                let runner = SystemRunner {
                    timeout: Duration::from_secs(secs),
                };
                match runner.run("sh", &["-c", command], self.repo_root) {
                    Ok(out) if out.success() => StageOutcome::Passed {
                        summary: format!("script passed (exit 0): {}", tail_line(&out.stdout)),
                        artifacts: stage.artifacts.clone(),
                    },
                    Ok(out) => {
                        let code = out
                            .exit_code
                            .map_or_else(|| "signal".to_string(), |c| c.to_string());
                        let detail = if out.stderr.trim().is_empty() {
                            tail_line(&out.stdout)
                        } else {
                            tail_line(&out.stderr)
                        };
                        StageOutcome::Failed {
                            reason: format!("script exit {code}: {detail}"),
                        }
                    }
                    Err(e) => StageOutcome::Failed {
                        reason: format!("script execution error: {e}"),
                    },
                }
            }
            StageKind::Gate { mode } => {
                notice(
                    "sop",
                    &format!("running gate stage '{}' ({mode:?})", stage.id),
                );
                let cfg = match self.ctx.load_config(self.repo_root) {
                    Ok(c) => c,
                    Err(e) => {
                        return StageOutcome::Failed {
                            reason: format!("load config: {}", e.message),
                        };
                    }
                };
                let progress = |event: &Progress<'_>| notice("sop", &progress_line(event));
                match aaf_engine::gate::run_gate_observed_reporting(
                    &self.ctx.data_dir,
                    self.repo_root,
                    &cfg.repo,
                    *mode,
                    &SystemClock,
                    &cfg.global.machine,
                    &progress,
                ) {
                    Ok(run) if run.receipt.status == GateStatus::Clear => StageOutcome::Passed {
                        summary: format!("gate {:?} CLEAR (tree={})", mode, run.receipt.tree_fp),
                        artifacts: stage.artifacts.clone(),
                    },
                    Ok(run) => StageOutcome::Failed {
                        reason: format!(
                            "gate {:?} {:?}: {}",
                            mode,
                            run.receipt.status,
                            run.receipt.reasons.join("; ")
                        ),
                    },
                    Err(e) => StageOutcome::Failed {
                        reason: format!("gate error: {e}"),
                    },
                }
            }
            StageKind::HumanGate { prompt } => StageOutcome::WaitHuman {
                prompt: prompt.clone(),
            },
            StageKind::Agent {
                role,
                executor,
                model,
                read_only,
                prompt,
            } => {
                let exec_str = executor.as_deref().unwrap_or("default");
                let model_str = model.as_deref().unwrap_or("default");
                StageOutcome::WaitHuman {
                    prompt: format!(
                        "agent stage '{}' (role={role}, read_only={read_only}, executor={exec_str}, model={model_str}): {prompt}",
                        stage.id
                    ),
                }
            }
        }
    }
}

fn tail_line(text: &str) -> String {
    text.lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map_or_else(String::new, |line| line.trim().chars().take(160).collect())
}
