//! `aaf plan "<goal>" [--file path] [--dry-run] [--prompt-only] [--executor name] [--enqueue]`:
//! run a strictly read-only planning agent (or print the prompt on `--dry-run`/`--prompt-only`),
//! optionally enqueueing a `Ready` task with the generated plan attached as `plan_approved:`.

use std::path::Path;

use aaf_domain::account::ProviderKind;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{AgentRequest, Clock, EventSink, PortError, TaskStore};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::executors;
use aaf_engine::factory::AccountPool;
use aaf_engine::factory::adapters::provider_for;
use aaf_engine::factory::prompt::{PLAN_APPROVED_PREFIX, sandbox_policy_for};
use aaf_engine::hook_settings;
use aaf_engine::plan_mode::build_plan_prompt;
use serde_json::json;

use super::board::store::{err, open_db, refuse_inside_agent_run, repo_root};
use super::sop::exec_support::{
    acquire_stage_account, build_exec_env, build_stage_run_config, host_sandbox_available,
};
use crate::ctx::{Ctx, SystemClock};
use crate::output::{CmdError, CmdOutput, CmdResult, notice};

const PLAN_TIMEOUT_SECS: u64 = 600;
const MAX_TITLE_CHARS: usize = 200;

#[derive(Debug, clap::Args)]
pub struct PlanArgs {
    /// Goal or problem statement to plan without editing code.
    pub goal: String,
    /// Relative context file inside the repository; repeat for more.
    #[arg(long = "file")]
    pub files: Vec<String>,
    /// Print the read-only plan prompt without launching an agent or modifying the board.
    #[arg(long)]
    pub dry_run: bool,
    /// Print only the read-only plan prompt without launching an agent.
    #[arg(long)]
    pub prompt_only: bool,
    /// Override `[run].executor` for this read-only planning turn.
    #[arg(long)]
    pub executor: Option<String>,
    /// Enqueue a `Ready` board task with the generated plan attached as `plan_approved:`.
    #[arg(long)]
    pub enqueue: bool,
}

pub fn run(args: &PlanArgs, ctx: &Ctx) -> CmdResult {
    let repo = repo_root(ctx)?;
    let built = match build_plan_prompt(&repo, &args.goal, &args.files) {
        Ok(built) => built,
        Err(PortError::Invalid(msg)) => return Ok(CmdOutput::policy("refused", msg)),
        Err(other) => return Err(CmdError::new(other.to_string())),
    };
    let plan_value = serde_json::to_value(&built).map_err(|e| err("plan", e))?;

    if args.dry_run || args.prompt_only {
        return Ok(CmdOutput::ok("ok", built.prompt.clone())
            .field("read_only", json!(built.read_only))
            .field("dry_run", json!(args.dry_run))
            .field("prompt_only", json!(args.prompt_only))
            .field("enqueued", json!(false))
            .field("plan", plan_value)
            .field("prompt", json!(built.prompt)));
    }

    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf plan") {
        return Ok(refused);
    }
    if !host_sandbox_available() {
        return Ok(CmdOutput::policy(
            "refused",
            "no agent sandbox on this platform: pass --dry-run or --prompt-only",
        ));
    }

    let cfg = ctx.load_config(&repo)?;
    let effective_exec = args
        .executor
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| cfg.repo.run.executor.trim());
    let effective_exec = if effective_exec.is_empty() {
        "claude"
    } else {
        effective_exec
    };

    if effective_exec.eq_ignore_ascii_case("claude")
        && let Err(problem) = hook_settings::check_worktree(&repo, &ctx.path_var, None)
    {
        return Ok(CmdOutput::policy("refused", problem.clone()).field("reason", json!(problem)));
    }

    let stage_run_cfg = build_stage_run_config(
        effective_exec,
        &cfg.repo.run.model,
        true,
        &cfg.repo.run,
        ctx,
    )
    .map_err(|msg| CmdError::new(format!("executor {effective_exec:?}: {msg}")))?;
    let exec_env = build_exec_env(ctx, &cfg);
    let runner =
        executors::build_runner_with(&stage_run_cfg, exec_env).map_err(|e| err("executor", e))?;

    let now = SystemClock.now();
    let run_id = format!(
        "plan-{}",
        &aaf_fp::sha256_hex(format!("{}:{now}:{}", built.goal, std::process::id()).as_bytes())
            [..16]
    );
    let (grant, pool_opt, all_profiles) =
        acquire_stage_account(ctx, &repo, &cfg, effective_exec, &run_id).map_err(CmdError::new)?;

    let provider = provider_for(effective_exec).unwrap_or(ProviderKind::ClaudeCode);
    let own_account = grant.as_ref().and_then(|g| g.account.as_ref());
    let mut sandbox = sandbox_policy_for(
        true,
        &ctx.data_dir,
        &ctx.home,
        provider,
        own_account,
        &all_profiles,
    );
    sandbox.read_only = true;

    let timeout_secs = cfg.repo.run.run_timeout_secs.clamp(60, PLAN_TIMEOUT_SECS);
    notice(
        "plan",
        &format!("running read-only planner via {effective_exec}"),
    );
    let request = AgentRequest {
        run_id,
        prompt: built.prompt.clone(),
        cwd: repo.to_string_lossy().into_owned(),
        account: own_account.cloned(),
        timeout_secs,
        sandbox,
        json_schema: None,
    };
    let outcome = runner.run(&request).map_err(|e| err("plan runner", e))?;
    if let (Some(pool), Some(g), Some(hit)) = (
        pool_opt.as_ref(),
        grant.as_ref(),
        outcome.limit_hit.as_ref(),
    ) && let Some(account_id) = g.account_id()
    {
        let _ = pool.report_limit(account_id, hit);
    }
    drop(grant);

    if !outcome.succeeded() {
        let why = if outcome.timed_out {
            format!("read-only planner timed out after {timeout_secs}s")
        } else if let Some(hit) = &outcome.limit_hit {
            format!("read-only planner hit rate limit: {}", hit.raw)
        } else if let Some(infra) = &outcome.infra {
            format!(
                "read-only planner infra failure ({:?}): {}",
                infra.kind, infra.raw
            )
        } else {
            let code = outcome
                .exit_code
                .map_or_else(|| "signal".to_string(), |c| c.to_string());
            format!("read-only planner exited with {code}")
        };
        return Ok(CmdOutput::policy("failed", why)
            .field("read_only", json!(true))
            .field("dry_run", json!(false))
            .field("executor", json!(effective_exec))
            .field("exit_code", json!(outcome.exit_code))
            .field("timed_out", json!(outcome.timed_out))
            .field("log_tail", json!(outcome.log_tail)));
    }

    let generated = if !outcome.final_text.trim().is_empty() {
        outcome.final_text.trim().to_string()
    } else {
        outcome.log_tail.trim().to_string()
    };
    if generated.is_empty() {
        return Ok(CmdOutput::policy(
            "failed",
            "read-only planner produced an empty plan",
        ));
    }

    let mut out = CmdOutput::ok("ok", generated.clone())
        .field("read_only", json!(true))
        .field("dry_run", json!(false))
        .field("prompt_only", json!(false))
        .field("executor", json!(effective_exec))
        .field("plan", plan_value)
        .field("prompt", json!(built.prompt))
        .field("generated_plan", json!(generated))
        .field("tokens_in", json!(outcome.tokens_in))
        .field("tokens_out", json!(outcome.tokens_out));

    if args.enqueue {
        let task = enqueue_planned_task(ctx, &repo, &built.goal, &generated)?;
        let task_val = serde_json::to_value(&task).map_err(|e| err("task", e))?;
        out.text
            .push_str(&format!("\nenqueued {} (ready): {}", task.id, task.title));
        out = out
            .field("enqueued", json!(true))
            .field("task_id", json!(task.id))
            .field("task", task_val);
    } else {
        out = out.field("enqueued", json!(false));
    }
    Ok(out)
}

fn enqueue_planned_task(
    ctx: &Ctx,
    repo: &Path,
    goal: &str,
    generated_plan: &str,
) -> Result<Task, CmdError> {
    let db = open_db(ctx)?;
    let repo_str = repo.to_string_lossy().into_owned();
    let title = sanitize_title(goal);
    let now = SystemClock.now();
    let id = next_task_id(&db, &repo_str, &title, now)?;
    let spec = ImmutableSpec {
        description: goal.to_string(),
        acceptance: vec![
            "Implementation matches the approved plan and passes aaf gate".to_string(),
        ],
    };
    let mut task = Task::new_root(id, repo_str, title, spec, Priority::P2, now);
    task.state = TaskState::Ready;
    let note = format!("{PLAN_APPROVED_PREFIX} {generated_plan}");
    task.context_notes.push(note);
    db.insert_task(&task).map_err(|e| err("store", e))?;

    let created_payload = json!({
        "title": task.title,
        "source": "aaf plan --enqueue",
    });
    db.emit(
        &FactoryEvent::new(EventKind::TaskCreated, created_payload.to_string(), now)
            .for_task(&task.id),
    )
    .map_err(|e| err("store", e))?;
    let plan_payload = json!({
        "plan": generated_plan,
        "approved": true,
    });
    db.emit(
        &FactoryEvent::new(EventKind::PlanGenerated, plan_payload.to_string(), now)
            .for_task(&task.id),
    )
    .map_err(|e| err("store", e))?;
    Ok(task)
}

fn sanitize_title(goal: &str) -> String {
    let first_line = goal.lines().find(|l| !l.trim().is_empty()).unwrap_or(goal);
    let cleaned: String = first_line
        .trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_TITLE_CHARS)
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "Planned task".to_string()
    } else {
        trimmed.to_string()
    }
}

fn next_task_id(
    store: &dyn TaskStore,
    repo: &str,
    title: &str,
    now: i64,
) -> Result<String, CmdError> {
    for attempt in 0_u32..100 {
        let seed = format!("{repo}\n{title}\n{now}\n{}\n{attempt}", std::process::id());
        let id = format!("t-{}", &aaf_fp::sha256_hex(seed.as_bytes())[..8]);
        match store.get_task(&id) {
            Err(PortError::NotFound(_)) => return Ok(id),
            Ok(_) => {}
            Err(other) => return Err(err("store", other)),
        }
    }
    Err(CmdError::new("could not find a free task id"))
}
