//! Per-stage native runner, provider-scoped pool, `StageRouter`, and native
//! `AuditBackend` wiring for `aaf run` (§D.3, §D.4.1, R3-CONC-5, R3-PROTO-2).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use aaf_domain::account::ProviderKind;
use aaf_domain::config::{CrossAuditConfig, RunConfig};
use aaf_domain::pipeline::{ReviewerConfig, StageModelConfig, is_known_real_reviewer};
use aaf_domain::ports::AgentRunner;
use aaf_engine::config::EffectiveConfig;
use aaf_engine::db::Db;
use aaf_engine::executors::{self, ExecEnv, agy::AgyRunner};
use aaf_engine::factory::adapters::{RouterPool, SystemClock};
use aaf_engine::factory::{
    AccountPool, AntigravityLockedPool, AuditBackend, ProviderKey, SelectingPool, StageRouter,
    StageTarget, build_audit_command, configured_audit_provider, resolve_auto_audit_executor,
    same_audit_provider,
};

use super::super::board::store::err;
use crate::ctx::Ctx;
use crate::output::CmdError;

type Spec<'a> = (&'a str, &'a str, &'a str, &'a str);

pub(super) fn exec_env_for(ctx: &Ctx, cfg: &EffectiveConfig) -> ExecEnv {
    let s = |p: &Path| p.to_string_lossy().into_owned();
    let mut env = ExecEnv {
        home: Some(s(&ctx.home)),
        data_dir: Some(s(&ctx.data_dir)),
        worktrees_root: Some(s(&ctx.worktrees_dir)),
        claude_bin: executors::find_on_path(&ctx.path_var, "claude").map(|p| s(&p)),
        agy_bin: AgyRunner::resolve_binary(&ctx.path_var).map(|p| s(&p)),
        machine_env_pass: cfg.global.run.env_pass.clone(),
        machine_writable_caches: cfg.global.machine.writable_caches.clone(),
        keychain_policy: cfg.global.safety.deny_macos_keychain,
        ..ExecEnv::from_process()
    };
    env.parent_env.retain(|(k, _)| k != "PATH");
    if !ctx.path_var.is_empty() {
        let p = ctx.path_var.to_string_lossy().into_owned();
        env.parent_env.push(("PATH".into(), p));
    }
    env
}

/// Builds a stage runner: `"claude"` -> `ClaudeCodeRunner`, `"antigravity"`
/// (empty `executor_cmd`) -> native `AgyRunner` (never `GenericRunner` calling
/// `gemini`, R3-PROTO-2), all others -> `GenericRunner` with `model`/`effort`.
pub(super) fn build_stage_runner(
    (executor, executor_cmd, model, effort): Spec<'_>,
    env_pass: &[String],
    exec_env: ExecEnv,
    read_only_audit: bool,
) -> Result<Arc<dyn AgentRunner>, CmdError> {
    let exec = aaf_domain::pipeline::normalize_executor_id(executor);
    let (cmd, model, effort) = (executor_cmd.trim(), model.trim(), effort.trim());
    let ro_gen =
        read_only_audit && matches!(exec.as_str(), "codex" | "gemini" | "opencode" | "grok");
    let mut run_cfg = RunConfig {
        executor: exec.clone(),
        model: model.into(),
        effort: effort.into(),
        env_pass: env_pass.to_vec(),
        ..RunConfig::default()
    };
    if !cmd.is_empty() || ro_gen {
        let audit = CrossAuditConfig {
            executor: exec,
            executor_cmd: cmd.into(),
            model: model.into(),
            effort: effort.into(),
            ..CrossAuditConfig::default()
        };
        run_cfg.executor = "custom".into();
        run_cfg.executor_cmd = build_audit_command(&audit).map_err(|e| err("stage", e))?;
    }
    let runner = executors::build_runner_with(&run_cfg, exec_env).map_err(|e| err("stage", e))?;
    Ok(Arc::from(runner))
}

/// Builds a provider-scoped [`AccountPool`]. R3-CONC-5: [`ProviderKind::Antigravity`]
/// ALWAYS clamps `max_concurrent_per_account = 1` AND wraps in [`AntigravityLockedPool`].
/// Uses strict audit reserve only for auxiliary pools (never for the primary writer
/// pool, which would stall coding whenever any unrelated repo is in `Gating`).
pub(super) fn build_pool_for_provider(
    ctx: &Ctx,
    repo: &Path,
    cfg: &EffectiveConfig,
    db: &Db,
    key: &ProviderKey,
    strict_audit_reserve: bool,
) -> Arc<dyn AccountPool> {
    let acc = &cfg.global.accounts;
    let provider = key.provider_kind();
    let is_agy = provider == Some(ProviderKind::Antigravity);
    let max_conc = if is_agy {
        1
    } else {
        acc.max_concurrent_per_account
    };
    let db_arc = Arc::new(db.clone());
    let rp = RouterPool {
        accounts: db_arc.clone(),
        runs: db_arc.clone(),
        clock: Arc::new(SystemClock),
        leases_dir: ctx.data_dir.join("leases"),
        max_concurrent_per_account: max_conc,
        provider,
    };
    let mut selecting = SelectingPool::new(rp, acc.selection, repo, db_arc);
    selecting = if is_agy && strict_audit_reserve {
        let has_gating = super::audit_reserve::has_live_gating_for_provider(
            ctx,
            repo,
            cfg,
            db.clone(),
            ProviderKind::Antigravity,
        );
        selecting.with_strict_audit_reserve(has_gating)
    } else {
        selecting.with_gating_audit_reserve(super::audit_reserve::gating_authors_provider(
            ctx,
            repo,
            cfg,
            db.clone(),
        ))
    };
    let pool: Arc<dyn AccountPool> = Arc::new(selecting);
    if is_agy {
        Arc::new(AntigravityLockedPool::new(pool, ctx.data_dir.clone()))
    } else {
        pool
    }
}

fn first_distinct<'a>(rs: &'a [ReviewerConfig], against: &str) -> Option<&'a ReviewerConfig> {
    rs.iter()
        .find(|r| is_known_real_reviewer(r) && !same_audit_provider(against, &r.executor))
}

fn to_audit(t: &StageTarget) -> AuditBackend {
    AuditBackend {
        provider: t.executor.clone(),
        runner: Arc::clone(&t.runner),
        accounts: Arc::clone(&t.accounts),
        hook_required: t.hook_required,
    }
}

fn st(s: &StageModelConfig) -> Spec<'_> {
    (&s.executor, &s.executor_cmd, &s.model, &s.effort)
}

fn rv(r: &ReviewerConfig) -> Spec<'_> {
    (&r.executor, &r.executor_cmd, &r.model, &r.effort)
}

fn pick_str<'a>(inherit: bool, ov: &'a str, base: &'a str) -> &'a str {
    if inherit && ov.trim().is_empty() {
        base
    } else {
        ov
    }
}

/// Builds [`StageRouter`] for `aaf run`, sharing one [`AccountPool`] per [`ProviderKey`].
pub(super) fn build_stage_router(
    ctx: &Ctx,
    repo: &Path,
    cfg: &EffectiveConfig,
    db: &Db,
    default_runner: Arc<dyn AgentRunner>,
    default_accounts: Arc<dyn AccountPool>,
) -> Result<StageRouter, CmdError> {
    let (run, pipe, cross) = (&cfg.repo.run, &cfg.repo.pipeline, &cfg.repo.cross_audit);
    let exec_env = exec_env_for(ctx, cfg);
    let dkey = ProviderKey::from_executor(&run.executor);
    let dpool = match dkey.provider_kind() {
        Some(ProviderKind::Antigravity) => {
            build_pool_for_provider(ctx, repo, cfg, db, &dkey, false)
        }
        _ => default_accounts,
    };
    let mut pools = HashMap::from([(dkey, Arc::clone(&dpool))]);
    let (dc, dr) = (run.executor_cmd.join(" "), default_runner);
    // Keep writer-stage effective executor mapping in sync with
    // `aaf_engine::factory::stage_router::writer_hook_executors`.
    let mut def = StageTarget::new_writer(&run.executor, dc, &run.model, &run.effort, dr, dpool);
    def.hook_required |= super::run_executor_is_claude(run);
    let mut mk = |sp: Spec<'_>, ro: bool| -> Result<StageTarget, CmdError> {
        let (exec, c, m, e) = (sp.0.trim(), sp.1.trim(), sp.2.trim(), sp.3.trim());
        let eff = match (exec, c) {
            ("", "") => run.executor.trim(),
            ("", _) => "custom",
            (v, _) => v,
        };
        let r = build_stage_runner((eff, c, m, e), &run.env_pass, exec_env.clone(), ro)?;
        let key = ProviderKey::from_executor(eff);
        let strict = super::audit_reserve::is_auxiliary_auditor(ctx, cfg, &key);
        let p = pools
            .entry(key.clone())
            .or_insert_with(|| build_pool_for_provider(ctx, repo, cfg, db, &key, strict));
        let target = if ro {
            StageTarget::new(eff, c, m, e, r, Arc::clone(p))
        } else {
            StageTarget::new_writer(eff, c, m, e, r, Arc::clone(p))
        };
        Ok(target)
    };

    let (p_i, p_e) = (&pipe.implementer, &pipe.implementer_escalated);
    let (p_a, p_c) = (&pipe.plan_author, &pipe.plan_challenger);
    let impl_t = (!p_i.is_empty()).then(|| mk(st(p_i), false)).transpose()?;
    let esc_t = (!p_e.is_empty()).then(|| mk(st(p_e), false)).transpose()?;
    let (pa, pc) = if pipe.plan_debate {
        let pa = (!p_a.is_empty()).then(|| mk(st(p_a), true)).transpose()?;
        let a_exec = pa.as_ref().unwrap_or(&def).executor.as_str();
        let pc = if !p_c.is_empty() {
            Some(mk(st(p_c), true)?)
        } else if !cross.executor.trim().is_empty() || !cross.executor_cmd.trim().is_empty() {
            let eff = match cross.executor.trim() {
                "" => "custom".into(),
                ex => resolve_auto_audit_executor(a_exec, ex, &ctx.path_var, &ctx.home),
            };
            let same_prov = same_audit_provider(a_exec, &eff)
                || (aaf_engine::factory::audit_cmd_is_claude(&cross.executor_cmd)
                    && same_audit_provider(a_exec, "claude"));
            if same_prov {
                let found = first_distinct(&cross.reviewers, a_exec);
                found.map(|r| mk(rv(r), true)).transpose()?
            } else {
                let x = cross;
                Some(mk((&eff, &x.executor_cmd, &x.model, &x.effort), true)?)
            }
        } else {
            let found = first_distinct(&cross.reviewers, a_exec);
            found.map(|r| mk(rv(r), true)).transpose()?
        };
        (pa, pc)
    } else {
        (None, None)
    };
    let mut revs = Vec::new();
    if cross.dual_review {
        for r in &cross.reviewers {
            revs.push((r.id.clone(), mk(rv(r), true)?));
        }
    }
    let router = StageRouter::from_targets(def, impl_t, esc_t, pa, pc, revs, pools);
    super::adopt_wiring::attach(router, ctx, repo, cfg, db)
}

/// Builds the single-auditor [`AuditBackend`] using native `AgyRunner` +
/// [`AntigravityLockedPool`] when effective auditor is `"antigravity"` (R3-PROTO-2).
pub(super) fn build_native_audit_backend(
    ctx: &Ctx,
    repo: &Path,
    cfg: &EffectiveConfig,
    db: &Db,
    router: Option<&StageRouter>,
) -> Result<Option<AuditBackend>, CmdError> {
    let (x, r) = (&cfg.repo.cross_audit, &cfg.repo.run);
    let (run_exec, blank) = (r.executor.trim(), |v: &str| v.trim().is_empty());
    let tail_empty = blank(&x.executor_cmd) && blank(&x.model) && blank(&x.effort);
    let (prov, cmd, model, effort) = if blank(&x.executor) && tail_empty {
        let Some(rev) = first_distinct(&x.reviewers, run_exec) else {
            return Ok(None);
        };
        if let Some(t) = router.and_then(|rt| rt.reviewer(&rev.id)) {
            return Ok(Some(to_audit(t)));
        }
        let (_, c, m, e) = rv(rev);
        (rev.executor.trim().to_string(), c, m, e)
    } else {
        let raw = configured_audit_provider(run_exec, x);
        let res = resolve_auto_audit_executor(run_exec, raw, &ctx.path_var, &ctx.home);
        let is_auto = x.executor.trim().eq_ignore_ascii_case("auto");
        if is_auto && res.eq_ignore_ascii_case(run_exec) && tail_empty {
            return Ok(None);
        }
        let inh = res.eq_ignore_ascii_case("claude")
            && blank(&x.executor_cmd)
            && run_exec.eq_ignore_ascii_case("claude");
        let m = pick_str(inh, &x.model, &r.model);
        let e = pick_str(inh, &x.effort, &r.effort);
        (res, x.executor_cmd.as_str(), m, e)
    };
    let sp = (prov.as_str(), cmd, model, effort);
    let runner = build_stage_runner(sp, &r.env_pass, exec_env_for(ctx, cfg), true)?;
    let key = ProviderKey::from_executor(&prov);
    let accounts = router
        .and_then(|rt| rt.pool_for(&key))
        .unwrap_or_else(|| build_pool_for_provider(ctx, repo, cfg, db, &key, true));
    let target = StageTarget::new(prov, cmd, model, effort, runner, accounts);
    Ok(Some(to_audit(&target)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use aaf_domain::account::AccountProfile;
    use aaf_domain::config::{GlobalConfig, RepoConfig};
    use aaf_domain::ports::{AccountStore, TaskStore};
    use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
    use aaf_engine::factory::Purpose;

    #[test]
    fn provider_pool_reserves_last_account_for_gating_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("aaf.db")).unwrap();
        for (id, name) in [("claude_code-a", "a"), ("claude_code-b", "b")] {
            let cfg_dir = dir.path().join(format!("cfg-{name}"));
            std::fs::create_dir_all(&cfg_dir).unwrap();
            db.upsert_account(&AccountProfile {
                id: id.into(),
                provider: ProviderKind::ClaudeCode,
                config_dir: cfg_dir.to_string_lossy().into_owned(),
                name: name.into(),
            })
            .unwrap();
        }
        let mut gating = Task::new_root(
            "t-gate",
            "/tmp/repo-a",
            "gating task",
            ImmutableSpec {
                description: String::new(),
                acceptance: Vec::new(),
            },
            Priority::P1,
            100,
        );
        gating.state = TaskState::Gating;
        db.insert_task(&gating).unwrap();

        let locks_dir = dir.path().join("locks");
        std::fs::create_dir_all(&locks_dir).unwrap();
        let repo_a_lock = std::fs::File::create(locks_dir.join(format!(
            "{}.run.lock",
            aaf_fp::repo_id(Path::new("/tmp/repo-a"))
        )))
        .unwrap();
        repo_a_lock.lock().unwrap();

        let ctx = Ctx {
            cwd: dir.path().to_path_buf(),
            home: dir.path().to_path_buf(),
            data_dir: dir.path().to_path_buf(),
            worktrees_dir: dir.path().join("wt"),
            config_dir: dir.path().to_path_buf(),
            path_var: std::ffi::OsString::new(),
            run_id: None,
            stdin_is_tty: false,
            aaf_env: Vec::new(),
        };
        let cfg = EffectiveConfig {
            global: GlobalConfig::default(),
            repo: RepoConfig::default(),
            sources: Vec::new(),
            warnings: Vec::new(),
        };
        let pool = build_pool_for_provider(
            &ctx,
            Path::new("/tmp/repo-b"),
            &cfg,
            &db,
            &ProviderKey::from_executor("claude"),
            true,
        );

        // First coding work leases 1 of 2 accounts; second coding work must NOT steal
        // the last remaining account while a Gating task exists in another repo...
        let _held = pool
            .acquire_for("t-coding-1", Purpose::Work)
            .unwrap()
            .expect("first work should lease 1 of 2 accounts");
        assert!(
            pool.acquire_for("t-coding-2", Purpose::Work)
                .unwrap()
                .is_none(),
            "stage_wiring pool must reserve the last account when any task is Gating"
        );
        // ...while Audit MUST still be able to claim the reserved second account.
        assert!(
            pool.acquire_for("t-gate", Purpose::Audit)
                .unwrap()
                .is_some(),
            "audit acquisition must still succeed on the reserved account"
        );

        // Single-account Antigravity pool uses strict Finish-First reservation:
        // Work (plan challenger) yields even the sole slot when any task is Gating.
        let agy_dir = dir.path().join("cfg-agy");
        std::fs::create_dir_all(&agy_dir).unwrap();
        db.upsert_account(&AccountProfile {
            id: "antigravity-sole".into(),
            provider: ProviderKind::Antigravity,
            config_dir: agy_dir.to_string_lossy().into_owned(),
            name: "agy".into(),
        })
        .unwrap();
        let agy_pool = build_pool_for_provider(
            &ctx,
            Path::new("/tmp/repo-b"),
            &cfg,
            &db,
            &ProviderKey::from_executor("antigravity"),
            true,
        );
        assert!(
            agy_pool
                .acquire_for("t-plan-ready", Purpose::Work)
                .unwrap()
                .is_none(),
            "sole auxiliary Antigravity slot must be reserved for Gating Audit over Ready plan Work"
        );
        assert!(
            agy_pool
                .acquire_for("t-gate", Purpose::Audit)
                .unwrap()
                .is_some(),
            "Gating Audit must immediately acquire the sole Antigravity slot"
        );
        // Primary writer Antigravity pool (strict_audit_reserve = false) must NOT deadlock
        // coding work when another repo has a Gating task.
        let agy_primary = build_pool_for_provider(
            &ctx,
            Path::new("/tmp/repo-b"),
            &cfg,
            &db,
            &ProviderKey::from_executor("antigravity"),
            false,
        );
        assert!(
            agy_primary
                .acquire_for("t-coding-primary", Purpose::Work)
                .unwrap()
                .is_some(),
            "primary Antigravity writer pool must not stall on unrelated Gating tasks"
        );
    }

    #[test]
    fn gating_audit_reserve_steers_work_to_author_account_so_non_author_audits() {
        use aaf_domain::ports::RunStore;
        use aaf_domain::run::{RunRecord, RunStatus};

        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("aaf.db")).unwrap();
        for (id, name) in [
            ("claude-toan", "toan"),
            ("claude-max", "max"),
            ("claude-lua", "lua"),
        ] {
            let cfg_dir = dir.path().join(format!("cfg-{id}"));
            std::fs::create_dir_all(&cfg_dir).unwrap();
            let p = AccountProfile {
                id: id.into(),
                provider: ProviderKind::ClaudeCode,
                config_dir: cfg_dir.to_string_lossy().into_owned(),
                name: name.into(),
            };
            db.upsert_account(&p).unwrap();
        }
        let seed_run =
            |tid: &str, repo: &str, rid: &str, acct: &str, state: TaskState, now: i64| {
                let mut t = Task::new_root(
                    tid,
                    repo,
                    tid,
                    ImmutableSpec {
                        description: String::new(),
                        acceptance: Vec::new(),
                    },
                    Priority::P1,
                    now,
                );
                t.state = state;
                db.insert_task(&t).unwrap();
                let rec = RunRecord {
                    id: rid.into(),
                    task_id: tid.into(),
                    account_id: Some(acct.into()),
                    executor: "claude".into(),
                    worktree_path: "/tmp/wt".into(),
                    branch: "aaf/t".into(),
                    status: RunStatus::Succeeded,
                    exit_code: Some(0),
                    tokens_in: 10,
                    tokens_out: 5,
                    started_at: now + 1,
                    ended_at: Some(now + 2),
                };
                db.insert_run(&rec).unwrap();
            };
        // Repo A has a Gating task authored by `claude-lua`; Repo B is sticky to `claude-toan`.
        seed_run(
            "t-gate-lua",
            "/tmp/repo-a",
            "r-a",
            "claude-lua",
            TaskState::Gating,
            1_000,
        );
        seed_run(
            "t-repo-b-prev",
            "/tmp/repo-b",
            "r-b",
            "claude-toan",
            TaskState::Done,
            1_004,
        );

        let locks_dir = dir.path().join("locks");
        std::fs::create_dir_all(&locks_dir).unwrap();
        let repo_a_lock = std::fs::File::create(locks_dir.join(format!(
            "{}.run.lock",
            aaf_fp::repo_id(Path::new("/tmp/repo-a"))
        )))
        .unwrap();
        repo_a_lock.lock().unwrap();

        let mut global = GlobalConfig::default();
        global.accounts.selection = aaf_domain::config::AccountSelection::Sticky;
        let cfg = EffectiveConfig {
            global,
            repo: RepoConfig::default(),
            sources: Vec::new(),
            warnings: Vec::new(),
        };
        let ctx = Ctx {
            cwd: dir.path().to_path_buf(),
            home: dir.path().to_path_buf(),
            data_dir: dir.path().to_path_buf(),
            worktrees_dir: dir.path().join("wt"),
            config_dir: dir.path().to_path_buf(),
            path_var: std::ffi::OsString::new(),
            run_id: None,
            stdin_is_tty: false,
            aaf_env: Vec::new(),
        };
        let key = ProviderKey::from_executor("claude");
        let pool_a =
            build_pool_for_provider(&ctx, Path::new("/tmp/repo-a"), &cfg, &db, &key, false);
        let pool_b =
            build_pool_for_provider(&ctx, Path::new("/tmp/repo-b"), &cfg, &db, &key, false);
        let rp = RouterPool {
            accounts: Arc::new(db.clone()),
            runs: Arc::new(db.clone()),
            clock: Arc::new(SystemClock),
            leases_dir: dir.path().join("leases"),
            max_concurrent_per_account: 1,
            provider: Some(ProviderKind::ClaudeCode),
        };
        let _hold_max = rp
            .acquire_where(&|p| p.id == "claude-max")
            .unwrap()
            .unwrap();

        let work_b = pool_b
            .acquire_for("t-repo-b-new", Purpose::Work)
            .unwrap()
            .expect("work must take claude-lua so claude-toan stays free for audit");
        assert_eq!(work_b.account_id(), Some("claude-lua"));

        let audit_a = pool_a
            .acquire_for("t-gate-lua", Purpose::Audit)
            .unwrap()
            .expect("gating audit must immediately acquire non-author claude-toan");
        assert_eq!(audit_a.account_id(), Some("claude-toan"));
    }
}
