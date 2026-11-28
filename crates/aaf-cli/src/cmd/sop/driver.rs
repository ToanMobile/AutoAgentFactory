//! Isolated stage execution driver for `aaf sop run` (`ORCH-03`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use aaf_domain::config::RunConfig;
use aaf_domain::gate::{GateMode, GateStatus, LockStatus, Receipt};
use aaf_domain::ports::{AgentRequest, Clock};
use aaf_domain::sop::{
    DEFAULT_SCRIPT_TIMEOUT_SECS, SopCheckpoint, SopSpec, SopStage, StageKind, StageStatus,
};
use aaf_engine::config::EffectiveConfig;
use aaf_engine::executors;
use aaf_engine::factory::AccountPool;
use aaf_engine::factory::adapters::provider_for;
use aaf_engine::factory::audit_cmd_is_claude;
use aaf_engine::factory::prompt::sandbox_policy_for;
use aaf_engine::factory::stage_router::writer_requires_hook;
use aaf_engine::gate::{GIT_TIMEOUT, head_lock};
use aaf_engine::hook_settings;
use aaf_engine::process::{CommandRunner, SystemRunner};
use aaf_engine::sop::runner::{StageDriver, StageOutcome};
use aaf_engine::throttle::Progress;

use super::super::gate::progress_line;
use super::exec_support::{
    acquire_stage_account, build_exec_env, build_stage_run_config, host_sandbox_available,
    stage_uses_custom_cmd,
};
use super::heavy::{run_heavy_script, script_outcome};
use crate::cmd::run::run_executor_is_claude;
use crate::ctx::{Ctx, SystemClock};
use crate::output::notice;

pub(super) struct CliStageDriver<'a> {
    pub ctx: &'a Ctx,
    pub repo_root: &'a Path,
    pub worktree_path: Option<PathBuf>,
    pub base_sha: Option<String>,
    pub cfg: EffectiveConfig,
    pub spec: &'a SopSpec,
    pub auto: bool,
    pub last_gate_receipt: Option<Receipt>,
}

impl StageDriver for CliStageDriver<'_> {
    fn run_stage(&mut self, stage: &SopStage, cp: &SopCheckpoint) -> StageOutcome {
        match &stage.kind {
            StageKind::Script {
                command,
                heavy,
                timeout_secs,
            } => self.run_script_stage(stage, command, *heavy, *timeout_secs),
            StageKind::Gate { mode } => self.run_gate_stage(stage, *mode),
            StageKind::HumanGate { prompt } => self.run_human_gate_stage(stage, cp, prompt),
            StageKind::Agent {
                role,
                executor,
                model,
                read_only,
                prompt,
            } => self.run_agent_stage(
                stage,
                cp,
                role,
                executor.as_deref(),
                model.as_deref(),
                *read_only,
                prompt,
            ),
        }
    }
}

impl CliStageDriver<'_> {
    fn stage_dir(&self) -> &Path {
        self.worktree_path.as_deref().unwrap_or(self.repo_root)
    }

    fn run_script_stage(
        &mut self,
        stage: &SopStage,
        command: &str,
        heavy: bool,
        timeout_secs: u64,
    ) -> StageOutcome {
        let Some(work_dir) = self.worktree_path.clone() else {
            return StageOutcome::Failed {
                reason: format!(
                    "script stage '{}' refused: missing isolated SOP worktree",
                    stage.id
                ),
            };
        };
        self.last_gate_receipt = None;

        let base_ref = self.base_sha.as_deref().unwrap_or("HEAD");
        let report = head_lock::evaluate(&self.ctx.data_dir, &work_dir, &self.cfg.repo, base_ref);
        let head_lock_ok = match report.status {
            LockStatus::Clear => true,
            LockStatus::Approved => report
                .approval
                .as_ref()
                .is_some_and(|a| a.passes_independent(&report.lock_fp)),
            LockStatus::Blocked => false,
        };
        if !head_lock_ok {
            return StageOutcome::Failed {
                reason: format!(
                    "script stage '{}' blocked by head_lock ({:?}, {} locked file(s)); clear or independent approval required",
                    stage.id,
                    report.status,
                    report.locked_changes.len()
                ),
            };
        }

        let secs = if timeout_secs != 0 {
            timeout_secs
        } else {
            DEFAULT_SCRIPT_TIMEOUT_SECS
        };
        if heavy {
            return run_heavy_script(self.ctx, &work_dir, stage, command, secs);
        }
        notice("sop", &format!("running script stage '{}'", stage.id));
        let runner = SystemRunner {
            timeout: Duration::from_secs(secs),
        };
        match runner.run("sh", &["-c", command], &work_dir) {
            Ok(out) => {
                let detail = if out.success() || out.stderr.trim().is_empty() {
                    &out.stdout
                } else {
                    &out.stderr
                };
                script_outcome(out.exit_code, false, secs, detail, &stage.artifacts)
            }
            Err(e) => StageOutcome::Failed {
                reason: format!("script execution error: {e}"),
            },
        }
    }

    fn run_gate_stage(&mut self, stage: &SopStage, mode: GateMode) -> StageOutcome {
        notice(
            "sop",
            &format!("running gate stage '{}' ({mode:?})", stage.id),
        );
        let work_dir = self.stage_dir().to_path_buf();
        let cfg = match self.ctx.load_config(&work_dir) {
            Ok(c) => c,
            Err(e) => {
                self.last_gate_receipt = None;
                return StageOutcome::Failed {
                    reason: format!("load config: {}", e.message),
                };
            }
        };
        let progress = |event: &Progress<'_>| notice("sop", &progress_line(event));
        match aaf_engine::gate::run_gate_observed_reporting(
            &self.ctx.data_dir,
            &work_dir,
            &cfg.repo,
            mode,
            &SystemClock,
            &cfg.global.machine,
            &progress,
        ) {
            Ok(run) if run.receipt.status == GateStatus::Clear => {
                let tree_fp = run.receipt.tree_fp.clone();
                self.last_gate_receipt = Some(run.receipt);
                StageOutcome::Passed {
                    summary: format!("gate {:?} CLEAR (tree={tree_fp})", mode),
                    artifacts: stage.artifacts.clone(),
                }
            }
            Ok(run) => {
                self.last_gate_receipt = None;
                StageOutcome::Failed {
                    reason: format!(
                        "gate {:?} {:?}: {}",
                        mode,
                        run.receipt.status,
                        run.receipt.reasons.join("; ")
                    ),
                }
            }
            Err(e) => {
                self.last_gate_receipt = None;
                StageOutcome::Failed {
                    reason: format!("gate error: {e}"),
                }
            }
        }
    }

    fn run_human_gate_stage(
        &self,
        stage: &SopStage,
        cp: &SopCheckpoint,
        prompt: &str,
    ) -> StageOutcome {
        if self.auto
            && !is_release_human_gate(self.spec, stage, self.last_gate_receipt.as_ref())
            && let Some(receipt) = self.preceding_clear_gate_receipt(stage, cp)
            && receipt_permits_auto_clear(&receipt)
        {
            return StageOutcome::Passed {
                summary: format!(
                    "auto-cleared human gate '{}' (gate={:?}, head_lock={:?})",
                    stage.id, receipt.mode, receipt.head_lock.status
                ),
                artifacts: stage.artifacts.clone(),
            };
        }
        StageOutcome::WaitHuman {
            prompt: prompt.to_string(),
        }
    }

    fn preceding_clear_gate_receipt(
        &self,
        stage: &SopStage,
        cp: &SopCheckpoint,
    ) -> Option<Receipt> {
        if let Some(receipt) = &self.last_gate_receipt {
            return Some(receipt.clone());
        }
        let has_passed_gate_dep = stage.depends_on.iter().any(|dep_id| {
            self.spec
                .stage(dep_id)
                .is_some_and(|dep| matches!(dep.kind, StageKind::Gate { .. }))
                && cp
                    .stage_states
                    .get(dep_id)
                    .is_some_and(|rec| rec.status == StageStatus::Passed)
        });
        if !has_passed_gate_dep {
            return None;
        }
        let work_dir = self.stage_dir();
        let key = aaf_fp::load_key(&self.ctx.data_dir).ok()?;
        let receipt = aaf_fp::read_receipt(&self.ctx.data_dir, &key, work_dir).ok()?;
        let current_fp = aaf_fp::tree_fp(work_dir, GIT_TIMEOUT).ok()?;
        (receipt.tree_fp == current_fp).then_some(receipt)
    }

    #[allow(clippy::too_many_arguments)]
    fn run_agent_stage(
        &mut self,
        stage: &SopStage,
        cp: &SopCheckpoint,
        role: &str,
        executor: Option<&str>,
        model: Option<&str>,
        read_only: bool,
        prompt: &str,
    ) -> StageOutcome {
        let work_dir = if read_only {
            self.stage_dir().to_path_buf()
        } else {
            let Some(wt) = self.worktree_path.clone() else {
                return StageOutcome::Failed {
                    reason: format!(
                        "write agent stage '{}' refused: missing isolated SOP worktree",
                        stage.id
                    ),
                };
            };
            self.last_gate_receipt = None;
            wt
        };

        if !host_sandbox_available() {
            return StageOutcome::Failed {
                reason: "no agent sandbox on this platform: SOP agent stages require OS sandboxing"
                    .to_string(),
            };
        }

        let effective_exec = executor
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.cfg.repo.run.executor.trim());
        let effective_exec = if effective_exec.is_empty() {
            "claude"
        } else {
            effective_exec
        };
        let effective_model = model
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.cfg.repo.run.model.trim());

        let run_cfg = &self.cfg.repo.run;
        let uses_custom = stage_uses_custom_cmd(effective_exec, run_cfg);
        let custom_cmd = if uses_custom {
            run_cfg.executor_cmd.join(" ")
        } else {
            String::new()
        };
        let uses_custom_claude = uses_custom
            && (audit_cmd_is_claude(&custom_cmd)
                || run_executor_is_claude(&RunConfig {
                    executor: "custom".to_string(),
                    executor_cmd: run_cfg.executor_cmd.clone(),
                    ..RunConfig::default()
                }));
        let hook_base = (work_dir != self.repo_root)
            .then_some(self.base_sha.as_deref())
            .flatten();
        if aaf_domain::pipeline::normalize_executor_id(effective_exec) == "claude"
            || uses_custom_claude
        {
            if let Err(problem) =
                hook_settings::check_worktree(&work_dir, &self.ctx.path_var, hook_base)
            {
                return StageOutcome::Failed { reason: problem };
            }
        } else if writer_requires_hook(effective_exec, &custom_cmd)
            && let Err(problem) = hook_settings::check_worktree_for_executor(
                &work_dir,
                &self.ctx.path_var,
                hook_base,
                effective_exec,
            )
        {
            return StageOutcome::Failed { reason: problem };
        }

        let stage_run_cfg = match build_stage_run_config(
            effective_exec,
            effective_model,
            read_only,
            &self.cfg.repo.run,
            self.ctx,
        ) {
            Ok(cfg) => cfg,
            Err(reason) => return StageOutcome::Failed { reason },
        };

        let exec_env = build_exec_env(self.ctx, &self.cfg);
        let runner = match executors::build_runner_with(&stage_run_cfg, exec_env) {
            Ok(r) => r,
            Err(e) => {
                return StageOutcome::Failed {
                    reason: format!("build executor {effective_exec:?}: {e}"),
                };
            }
        };

        let (grant, pool_opt, all_profiles) = match acquire_stage_account(
            self.ctx,
            self.repo_root,
            &self.cfg,
            effective_exec,
            &cp.run_id,
        ) {
            Ok(res) => res,
            Err(reason) => return StageOutcome::Failed { reason },
        };

        let provider = provider_for(effective_exec);
        let own_account = grant.as_ref().and_then(|g| g.account.as_ref());
        let mut sandbox = sandbox_policy_for(
            true,
            &self.ctx.data_dir,
            &self.ctx.home,
            provider,
            own_account,
            &all_profiles,
        );
        sandbox.read_only = read_only;

        let now = SystemClock.now();
        let stage_run_id = format_stage_run_id(&cp.run_id, &stage.id, now);
        let full_prompt = build_stage_prompt(stage, role, read_only, prompt);
        let timeout_secs = self.cfg.repo.run.run_timeout_secs.max(60);

        notice(
            "sop",
            &format!(
                "running agent stage '{}' (role={role}, executor={effective_exec}, read_only={read_only})",
                stage.id
            ),
        );
        let request = AgentRequest {
            run_id: stage_run_id,
            prompt: full_prompt,
            cwd: work_dir.to_string_lossy().into_owned(),
            account: own_account.cloned(),
            timeout_secs,
            sandbox,
            json_schema: None,
            max_turns: read_only.then_some(20),
        };
        let outcome = match runner.run(&request) {
            Ok(o) => o,
            Err(e) => {
                return StageOutcome::Failed {
                    reason: format!("agent stage '{}' execution error: {e}", stage.id),
                };
            }
        };
        if let (Some(pool), Some(g), Some(hit)) = (
            pool_opt.as_ref(),
            grant.as_ref(),
            outcome.limit_hit.as_ref(),
        ) && let Some(account_id) = g.account_id()
        {
            let _ = pool.report_limit(account_id, hit);
        }
        drop(grant);

        if outcome.succeeded() {
            let excerpt = first_non_empty_line(&outcome.final_text)
                .or_else(|| first_non_empty_line(&outcome.log_tail))
                .unwrap_or_else(|| "completed".to_string());
            StageOutcome::Passed {
                summary: format!(
                    "agent '{role}' ({effective_exec}) passed ({}+{} tok): {excerpt}",
                    outcome.tokens_in, outcome.tokens_out
                ),
                artifacts: stage.artifacts.clone(),
            }
        } else {
            let reason = if outcome.timed_out {
                format!("agent stage '{}' timed out after {timeout_secs}s", stage.id)
            } else if let Some(hit) = &outcome.limit_hit {
                format!("agent stage '{}' hit rate limit: {}", stage.id, hit.raw)
            } else if let Some(infra) = &outcome.infra {
                format!(
                    "agent stage '{}' infra failure ({:?}): {}",
                    stage.id, infra.kind, infra.raw
                )
            } else {
                let code = outcome
                    .exit_code
                    .map_or_else(|| "signal".to_string(), |c| c.to_string());
                let detail = first_non_empty_line(&outcome.log_tail).unwrap_or_default();
                format!("agent stage '{}' exit {code}: {detail}", stage.id)
            };
            StageOutcome::Failed { reason }
        }
    }
}

fn is_release_human_gate(spec: &SopSpec, stage: &SopStage, receipt: Option<&Receipt>) -> bool {
    if stage.id.to_ascii_lowercase().contains("release")
        || stage.title.to_ascii_lowercase().contains("release")
    {
        return true;
    }
    if receipt.is_some_and(|r| r.mode == GateMode::Release) {
        return true;
    }
    stage.depends_on.iter().any(|dep_id| {
        dep_id.to_ascii_lowercase().contains("release")
            || spec.stage(dep_id).is_some_and(|dep| {
                matches!(
                    dep.kind,
                    StageKind::Gate {
                        mode: GateMode::Release
                    }
                )
            })
    })
}

fn receipt_permits_auto_clear(receipt: &Receipt) -> bool {
    if receipt.status != GateStatus::Clear || receipt.mode == GateMode::Release {
        return false;
    }
    match receipt.head_lock.status {
        LockStatus::Clear => true,
        LockStatus::Approved => receipt
            .head_lock
            .approval
            .as_ref()
            .is_some_and(|a| a.passes_independent(&receipt.head_lock.lock_fp)),
        LockStatus::Blocked => false,
    }
}

fn format_stage_run_id(cp_run_id: &str, stage_id: &str, now: i64) -> String {
    let seed = format!("{cp_run_id}:{stage_id}:{now}:{}", std::process::id());
    let digest = &aaf_fp::sha256_hex(seed.as_bytes())[..16];
    let safe_stage: String = stage_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(24)
        .collect();
    format!("sop-{safe_stage}-{digest}")
}

fn build_stage_prompt(stage: &SopStage, role: &str, read_only: bool, prompt: &str) -> String {
    let mode_line = if read_only {
        "Mode: READ-ONLY (do not modify files or run mutating commands)."
    } else {
        "Mode: ISOLATED WORKTREE (work only inside this worktree)."
    };
    format!(
        "SOP Stage: {} ({})\nRole: {role}\n{mode_line}\n\n{}",
        stage.title, stage.id, prompt
    )
}

fn first_non_empty_line(text: &str) -> Option<String> {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().chars().take(160).collect())
}
