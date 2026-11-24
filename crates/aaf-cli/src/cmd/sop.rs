//! `aaf sop`: declarative SOP workflow inspection, scaffolding, validation, execution, and approvals.

use std::path::{Path, PathBuf};

use aaf_domain::ports::Clock;
use aaf_domain::sop::{SopCheckpoint, SopRunStatus, StageStatus};
use aaf_engine::sop::runner::{
    approve_human_gate, checkpoint_path, list_checkpoints, load_checkpoint, ready_stages,
    run_sop_persisted, save_checkpoint, send_back,
};
use aaf_engine::sop::{
    init_sop_file, list_all_sops, load_repo_sops, load_sop_file, render_ascii_dag, resolve_sop,
};
use serde_json::json;

mod driver;
pub(crate) mod exec_support;
mod heavy;
mod trusted;

use super::board::store::{refuse_inside_agent_run, repo_root};
use crate::ctx::{Ctx, SystemClock};
use crate::output::{CmdError, CmdOutput, CmdResult};
use driver::CliStageDriver;
use trusted::{
    acquire_sop_run_lock, ensure_sop_worktree, resolve_trusted_sop, spec_needs_worktree,
};

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
        /// Auto-clear non-release human gates when preceded by a CLEAR gate receipt with clear
        /// or independent head-lock approval.
        #[arg(long)]
        auto: bool,
    },
    /// Approve a paused `WaitingHuman` stage so the run can be resumed.
    Approve {
        /// Checkpoint `run_id` waiting for human approval.
        run_id: String,
        /// Reviewer name recorded in the checkpoint audit log.
        #[arg(long)]
        reviewer: String,
        /// Immediately resume the SOP run after approving the waiting human gate.
        #[arg(long)]
        resume: bool,
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
            auto,
        } => {
            if *auto {
                cmd_run_opts(name, resume.as_deref(), *dry_run, true, ctx)
            } else {
                cmd_run(name, resume.as_deref(), *dry_run, ctx)
            }
        }
        SopCommand::Approve {
            run_id,
            reviewer,
            resume,
        } => {
            if *resume {
                cmd_approve_opts(run_id, reviewer, true, ctx)
            } else {
                cmd_approve(run_id, reviewer, ctx)
            }
        }
        SopCommand::SendBack { run_id, to, reason } => cmd_send_back(run_id, to, reason, ctx),
    }
}

fn sop_err(e: impl std::fmt::Display) -> CmdError {
    CmdError::new(e.to_string())
}

fn cmd_list(ctx: &Ctx) -> CmdResult {
    let repo = repo_root(ctx).ok();
    let sops = list_all_sops(repo.as_deref()).map_err(sop_err)?;
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
                "name": d.spec.name, "version": d.spec.version, "profile": d.spec.profile,
                "stages": d.spec.stages.len(), "max_reworks": d.spec.max_reworks,
                "description": d.spec.description, "source": d.source.label(),
            })
        })
        .collect();
    Ok(CmdOutput::ok("ok", lines.join("\n"))
        .field("count", json!(sops.len()))
        .field("sops", json!(rows)))
}

fn cmd_show(name: &str, ctx: &Ctx) -> CmdResult {
    let repo = repo_root(ctx).ok();
    let discovered = resolve_sop(repo.as_deref(), name).map_err(sop_err)?;
    let order = discovered.spec.validate().map_err(sop_err)?;
    let dag = render_ascii_dag(&discovered.spec).map_err(sop_err)?;
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
    let path = init_sop_file(&root, blueprint, name, force).map_err(sop_err)?;
    let sop_name = name.unwrap_or(blueprint);
    let text = format!("initialized SOP '{sop_name}' at {}", path.display());
    Ok(CmdOutput::ok("ok", text)
        .field("blueprint", json!(blueprint))
        .field("name", json!(sop_name))
        .field("path", json!(path_str(&path))))
}

fn cmd_validate(path: Option<&Path>, ctx: &Ctx) -> CmdResult {
    if let Some(file) = path {
        let spec = load_sop_file(file).map_err(sop_err)?;
        let order = spec.validate().map_err(sop_err)?;
        let text = format!(
            "valid: {} ({} stages: {})",
            file.display(),
            order.len(),
            order.join(" -> ")
        );
        return Ok(CmdOutput::ok("ok", text)
            .field("name", json!(spec.name))
            .field("path", json!(path_str(file)))
            .field("order", json!(order))
            .field("valid_count", json!(1)));
    }

    let root = repo_root(ctx)?;
    let repo_sops = load_repo_sops(&root).map_err(sop_err)?;
    if repo_sops.is_empty() {
        return Ok(
            CmdOutput::ok("ok", "no custom SOP files found under .autofactory/sops/")
                .field("valid_count", json!(0))
                .field("sops", json!([])),
        );
    }
    let mut lines = Vec::with_capacity(repo_sops.len());
    let mut items = Vec::with_capacity(repo_sops.len());
    for d in &repo_sops {
        let order = d.spec.validate().map_err(sop_err)?;
        let src = d.source.label();
        lines.push(format!(
            "valid: {} ({} stages) [{src}]",
            d.spec.name,
            order.len()
        ));
        items.push(json!({ "name": d.spec.name, "source": src, "order": order }));
    }
    Ok(CmdOutput::ok("ok", lines.join("\n"))
        .field("valid_count", json!(repo_sops.len()))
        .field("sops", json!(items)))
}

fn cmd_runs(ctx: &Ctx) -> CmdResult {
    let runs = list_checkpoints(&ctx.data_dir).map_err(sop_err)?;
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
        let (total, st) = (cp.stage_states.len(), run_status_str(cp.status));
        lines.push(format!(
            "{}  sop={:<16} status={st:<13} stages={passed}/{total}  reworks={}  updated={}",
            cp.run_id, cp.sop_name, cp.rework_count, cp.updated_at
        ));
        rows.push(json!({
            "run_id": cp.run_id, "sop_name": cp.sop_name, "repo_root": cp.repo_root,
            "status": cp.status, "passed_stages": passed, "total_stages": total,
            "rework_count": cp.rework_count, "worktree_path": cp.worktree_path,
            "branch": cp.branch, "created_at": cp.created_at, "updated_at": cp.updated_at,
        }));
    }
    Ok(CmdOutput::ok("ok", lines.join("\n"))
        .field("count", json!(runs.len()))
        .field("runs", json!(rows)))
}

fn cmd_run(name: &str, resume: Option<&str>, dry_run: bool, ctx: &Ctx) -> CmdResult {
    cmd_run_opts(name, resume, dry_run, false, ctx)
}

fn cmd_run_opts(
    name: &str,
    resume: Option<&str>,
    dry_run: bool,
    auto: bool,
    ctx: &Ctx,
) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf sop run") {
        return Ok(refused);
    }
    let repo_opt = repo_root(ctx).ok();
    let cfg_opt = repo_opt.as_deref().and_then(|r| ctx.load_config(r).ok());
    let base_ref = cfg_opt
        .as_ref()
        .map_or("auto", |c| c.repo.gate.base_ref.as_str());
    let (discovered, base_sha) = resolve_trusted_sop(repo_opt.as_deref(), base_ref, name)?;
    let spec = discovered.spec;
    let source_label = discovered.source.label();
    let order = spec.validate().map_err(sop_err)?;

    if dry_run {
        let dag = render_ascii_dag(&spec).map_err(sop_err)?;
        let text = format!(
            "DRY RUN (no stages executed, no worktree or checkpoint written)\n{dag}\nsource: {source_label}\nexecution order: {}",
            order.join(" -> ")
        );
        return Ok(CmdOutput::ok("dry_run", text)
            .field("sop_name", json!(spec.name))
            .field("source", json!(source_label))
            .field("dry_run", json!(true))
            .field("auto", json!(auto))
            .field("order", json!(order))
            .field("dag", json!(dag)));
    }

    let root = repo_root(ctx)?;
    let cfg = ctx.load_config(&root)?;
    let root_str = path_str(&root);
    let now = SystemClock.now();
    let mut cp = match resume {
        Some(run_id) => {
            let loaded = load_checkpoint(&ctx.data_dir, run_id).map_err(sop_err)?;
            if loaded.sop_name != spec.name {
                return Err(CmdError::new(format!(
                    "checkpoint {run_id:?} belongs to SOP {:?}, not {:?}",
                    loaded.sop_name, spec.name
                )));
            }
            check_checkpoint_repo(run_id, &loaded.repo_root, &root)?;
            loaded
        }
        None => {
            let run_id = next_run_id(&ctx.data_dir, &spec.name, now);
            SopCheckpoint::new(run_id, &spec, root_str, now)
        }
    };

    let Some(_lock) = acquire_sop_run_lock(ctx, &root)? else {
        return Ok(CmdOutput::policy(
            "refused",
            "another aaf run is already working on this repository",
        ));
    };

    let worktree_path = if spec_needs_worktree(&spec) {
        let wt = ensure_sop_worktree(ctx, &root, &cfg.repo.gate.base_ref, &mut cp)?;
        save_checkpoint(&ctx.data_dir, &cp).map_err(sop_err)?;
        Some(wt.path)
    } else {
        cp.worktree_path
            .as_deref()
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
    };

    let mut driver = CliStageDriver {
        ctx,
        repo_root: &root,
        worktree_path,
        base_sha,
        cfg,
        spec: &spec,
        auto,
        last_gate_receipt: None,
    };
    let final_status = run_sop_persisted(&spec, &mut cp, &ctx.data_dir, &mut driver, &SystemClock)
        .map_err(sop_err)?;
    let cp_path = checkpoint_path(&ctx.data_dir, &cp.run_id).map_err(sop_err)?;
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
    if let (Some(branch), Some(wt)) = (&cp.branch, &cp.worktree_path) {
        lines.push(format!("worktree: {wt} (branch {branch})"));
    }
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
            "next: run `aaf sop approve {} --reviewer <name> --resume`",
            cp.run_id
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
        .field("source", json!(source_label))
        .field("run_status", json!(final_status))
        .field("rework_count", json!(cp.rework_count))
        .field("worktree_path", json!(cp.worktree_path))
        .field("branch", json!(cp.branch))
        .field("auto", json!(auto))
        .field("next_ready", json!(next_ready))
        .field("checkpoint_path", json!(path_str(&cp_path)))
        .field("checkpoint", cp_json))
}

fn check_checkpoint_repo(run_id: &str, cp_repo: &str, current: &Path) -> Result<(), CmdError> {
    let cur = path_str(current);
    if !cp_repo.is_empty() && cp_repo != cur {
        return Err(CmdError::new(format!(
            "checkpoint {run_id:?} belongs to repo {cp_repo:?}, not {cur:?}"
        )));
    }
    Ok(())
}

fn cmd_approve(run_id: &str, reviewer: &str, ctx: &Ctx) -> CmdResult {
    cmd_approve_opts(run_id, reviewer, false, ctx)
}

fn cmd_approve_opts(run_id: &str, reviewer: &str, resume: bool, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf sop approve") {
        return Ok(refused);
    }
    let mut cp = load_checkpoint(&ctx.data_dir, run_id).map_err(sop_err)?;
    if let Ok(root) = repo_root(ctx) {
        check_checkpoint_repo(run_id, &cp.repo_root, &root)?;
    }
    let stage_id = approve_human_gate(&mut cp, reviewer, &SystemClock).map_err(sop_err)?;
    let cp_path = save_checkpoint(&ctx.data_dir, &cp).map_err(sop_err)?;
    let approve_line = format!(
        "approved stage '{stage_id}' on run '{}' by {reviewer}",
        cp.run_id
    );

    if resume {
        let mut run_ctx = ctx.clone();
        if repo_root(&run_ctx).is_err() && !cp.repo_root.is_empty() {
            run_ctx.cwd = PathBuf::from(&cp.repo_root);
        }
        let mut resumed = cmd_run_opts(&cp.sop_name, Some(&cp.run_id), false, false, &run_ctx)?;
        resumed.text = format!("{approve_line}\n{}", resumed.text);
        return Ok(resumed
            .field("approved_stage", json!(stage_id))
            .field("reviewer", json!(reviewer))
            .field("resumed", json!(true)));
    }

    let text = format!(
        "{approve_line}\nresume with: aaf sop run {} --resume {}",
        cp.sop_name, cp.run_id
    );
    Ok(CmdOutput::ok("ok", text)
        .field("run_id", json!(cp.run_id))
        .field("sop_name", json!(cp.sop_name))
        .field("approved_stage", json!(stage_id))
        .field("reviewer", json!(reviewer))
        .field("resumed", json!(false))
        .field("checkpoint_path", json!(path_str(&cp_path))))
}

fn cmd_send_back(run_id: &str, to: &str, reason: &str, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf sop send-back") {
        return Ok(refused);
    }
    let mut cp = load_checkpoint(&ctx.data_dir, run_id).map_err(sop_err)?;
    if let Ok(root) = repo_root(ctx) {
        check_checkpoint_repo(run_id, &cp.repo_root, &root)?;
    }
    let cp_repo = (!cp.repo_root.is_empty()).then(|| PathBuf::from(&cp.repo_root));
    let repo_opt = cp_repo.or_else(|| repo_root(ctx).ok());
    let base_ref = repo_opt
        .as_deref()
        .and_then(|r| ctx.load_config(r).ok())
        .map_or_else(|| "auto".to_string(), |c| c.repo.gate.base_ref);
    let (discovered, _) = resolve_trusted_sop(repo_opt.as_deref(), &base_ref, &cp.sop_name)?;
    send_back(&discovered.spec, &mut cp, to, reason, &SystemClock).map_err(sop_err)?;
    let cp_path = save_checkpoint(&ctx.data_dir, &cp).map_err(sop_err)?;
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
        .field("checkpoint_path", json!(path_str(&cp_path))))
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
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .take(24)
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

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}
