//! The adopt lane's share of `aaf run` wiring: its two reviewers and its `gh`.
//! Registered on every router, whatever `cross_audit.dual_review` says and
//! whether or not a task is `Adopting` yet: a PR adopted later by a chat has
//! to find them already there.

use std::path::Path;
use std::sync::Arc;

use aaf_engine::config::EffectiveConfig;
use aaf_engine::db::Db;
use aaf_engine::executors::registry::{find_descriptor, resolve_binary_with_home};
use aaf_engine::factory::audit_backend::adopt_reviewers;
use aaf_engine::factory::{AdoptEnv, ProviderKey, StageRouter, StageTarget, resolve_base};
use aaf_engine::git::SystemGh;

use super::stage_wiring::{build_pool_for_provider, build_stage_runner, exec_env_for};
use crate::ctx::Ctx;
use crate::output::CmdError;

/// Every provider id the lane could ask, probed the way `resolve_auto_audit_executor` does.
const PROVIDERS: [&str; 11] = [
    "claude",
    "codex",
    "gemini",
    "antigravity",
    "opencode",
    "grok",
    "cursor",
    "kimi",
    "pi",
    "codebuddy",
    "deepseek",
];

/// Providers whose CLI is installed (on `PATH` or in its usual place under `$HOME`).
fn installed_providers(ctx: &Ctx) -> Vec<&'static str> {
    PROVIDERS
        .into_iter()
        .filter(|id| {
            find_descriptor(id)
                .and_then(|d| resolve_binary_with_home(d, &ctx.path_var, Some(&ctx.home)))
                .is_some()
        })
        .collect()
}

/// Gives `router` the adopt reviewers (their pools shared with the router's
/// own, one pool per provider) and, when the base has a remote, the adopt
/// environment with the real `gh`. Without a remote the lane stays off.
pub(super) fn attach(
    router: StageRouter,
    ctx: &Ctx,
    repo: &Path,
    cfg: &EffectiveConfig,
    db: &Db,
) -> Result<StageRouter, CmdError> {
    let installed = installed_providers(ctx);
    let mut targets = Vec::new();
    for reviewer in adopt_reviewers(&cfg.repo, &installed) {
        let spec = (
            reviewer.executor.as_str(),
            reviewer.executor_cmd.as_str(),
            reviewer.model.as_str(),
            reviewer.effort.as_str(),
        );
        // A reviewer that cannot be built only costs the lane (it cancels with
        // `needs_human_reviewers`); it must not stop `aaf run`.
        let Ok(runner) =
            build_stage_runner(spec, &cfg.repo.run.env_pass, exec_env_for(ctx, cfg), true)
        else {
            continue;
        };
        let key = ProviderKey::from_executor(&reviewer.executor);
        let accounts = router.pool_for(&key).unwrap_or_else(|| {
            let strict = super::audit_reserve::is_auxiliary_auditor(ctx, cfg, &key);
            build_pool_for_provider(ctx, repo, cfg, db, &key, strict)
        });
        let target = StageTarget::new(spec.0, spec.1, spec.2, spec.3, runner, accounts);
        targets.push((reviewer.id.clone(), target));
    }
    let router = router.with_adopt_reviewers(targets);
    let remote = resolve_base(repo, &cfg.repo.gate.base_ref)
        .ok()
        .and_then(|base| base.remote);
    let Some(remote) = remote else {
        return Ok(router);
    };
    // As the factory's own PR merges: a PR with no registered check is not a pass.
    let template = AdoptEnv::request_template(
        repo.to_path_buf(),
        ctx.data_dir.clone(),
        &remote,
        Some(cfg.global.clone()),
        true,
    );
    Ok(router.with_adopt_env(AdoptEnv::new(Arc::new(SystemGh), template)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use aaf_domain::config::{GlobalConfig, RepoConfig};

    use super::*;

    fn ctx(dir: &Path, bin: &Path) -> Ctx {
        Ctx {
            cwd: dir.to_path_buf(),
            home: dir.join("home"),
            data_dir: dir.join("data"),
            worktrees_dir: dir.join("wt"),
            config_dir: dir.to_path_buf(),
            path_var: OsString::from(bin),
            run_id: None,
            stdin_is_tty: false,
            aaf_env: Vec::new(),
        }
    }

    fn fake_cli(bin: &Path, name: &str) {
        let path = bin.join(name);
        fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn base_router(ctx: &Ctx, cfg: &EffectiveConfig, db: &Db) -> StageRouter {
        let runner =
            build_stage_runner(("codex", "", "", ""), &[], exec_env_for(ctx, cfg), false).unwrap();
        let key = ProviderKey::from_executor("codex");
        let pool = build_pool_for_provider(ctx, &ctx.cwd, cfg, db, &key, false);
        StageRouter::new(StageTarget::new("codex", "", "", "", runner, pool))
    }

    #[test]
    fn the_adopt_reviewers_are_registered_even_with_dual_review_off() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        for cli in ["codex", "opencode"] {
            fake_cli(&bin, cli);
        }
        let mut repo_cfg = RepoConfig::default();
        repo_cfg.run.executor = "codex".into();
        repo_cfg.cross_audit.executor = "opencode".into();
        repo_cfg.cross_audit.dual_review = false;
        let cfg = EffectiveConfig {
            global: GlobalConfig::default(),
            repo: repo_cfg,
            sources: Vec::new(),
            warnings: Vec::new(),
        };
        let ctx = ctx(dir.path(), &bin);
        let db = Db::open(&dir.path().join("aaf.db")).unwrap();
        let router = base_router(&ctx, &cfg, &db);
        // Not a git repository: no remote, so no environment, but the reviewers
        // are there for the day a PR is adopted.
        let router = attach(router, &ctx, dir.path(), &cfg, &db).unwrap();
        let ids: Vec<&str> = router
            .adopt_reviewers
            .iter()
            .map(|(id, _)| id.as_str())
            .collect();
        assert_eq!(ids, ["codex", "opencode"]);
        assert!(router.reviewers.is_empty(), "dual_review stays off");
        assert!(
            router
                .pool_for(&ProviderKey::from_executor("opencode"))
                .is_some()
        );
        assert!(router.adopt.is_none(), "no remote, no lane");
    }

    #[test]
    fn a_repository_with_a_remote_gets_the_adopt_environment() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        for cli in ["codex", "opencode"] {
            fake_cli(&bin, cli);
        }
        let (bare, repo) = (dir.path().join("bare.git"), dir.path().join("repo"));
        fs::create_dir_all(&repo).unwrap();
        let git = |cwd: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(
            dir.path(),
            &["init", "--bare", "-b", "main", bare.to_str().unwrap()],
        );
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
        fs::write(repo.join("f"), "x\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "init"]);
        git(&repo, &["push", "-u", "origin", "main"]);
        let mut repo_cfg = RepoConfig::default();
        repo_cfg.run.executor = "codex".into();
        repo_cfg.cross_audit.executor = "opencode".into();
        repo_cfg.gate.base_ref = "origin/main".into();
        let cfg = EffectiveConfig {
            global: GlobalConfig::default(),
            repo: repo_cfg,
            sources: Vec::new(),
            warnings: Vec::new(),
        };
        let ctx = ctx(dir.path(), &bin);
        let db = Db::open(&dir.path().join("aaf.db")).unwrap();
        let router = attach(base_router(&ctx, &cfg, &db), &ctx, &repo, &cfg, &db).unwrap();
        let env = router.adopt.as_deref().expect("the lane is on");
        assert_eq!(env.request.remote, "origin");
        assert!(
            env.request.require_checks,
            "a PR with no check is not a pass"
        );
        assert!(env.request.no_receipt_reuse && env.request.strict_merged_head);
    }

    #[test]
    fn a_single_installed_provider_registers_no_reviewers() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        fake_cli(&bin, "codex");
        let mut repo_cfg = RepoConfig::default();
        repo_cfg.run.executor = "codex".into();
        repo_cfg.cross_audit.executor = "opencode".into();
        let cfg = EffectiveConfig {
            global: GlobalConfig::default(),
            repo: repo_cfg,
            sources: Vec::new(),
            warnings: Vec::new(),
        };
        let ctx = ctx(dir.path(), &bin);
        let db = Db::open(&dir.path().join("aaf.db")).unwrap();
        let router = attach(base_router(&ctx, &cfg, &db), &ctx, dir.path(), &cfg, &db).unwrap();
        assert!(router.adopt_reviewers.is_empty());
    }
}
