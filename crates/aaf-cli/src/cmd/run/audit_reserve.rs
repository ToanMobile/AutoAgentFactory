//! Live cross-repo & provider-aware `TaskState::Gating` audit reservation helpers.
//!
//! Filters `TaskState::Gating` tasks to repositories that are either the current
//! repo or have an active `.run.lock` flock held under `<data_dir>/locks/`, and
//! whose repo configuration actually audits on the target [`ProviderKind`],
//! preventing stopped repos or unrelated-provider gating tasks from deadlocking
//! live `Purpose::Work` dispatch.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aaf_domain::account::ProviderKind;
use aaf_domain::config::RepoConfig;
use aaf_domain::ports::TaskStore;
use aaf_domain::task::{Task, TaskState};
use aaf_engine::accounts::selection::RunHistory;
use aaf_engine::config::{
    self, ConfigInputs, ConfigOverlay, EffectiveConfig, LOCAL_CONFIG_REL, REPO_CONFIG_REL,
};
use aaf_engine::db::Db;
use aaf_engine::factory::{ProviderKey, configured_audit_provider, resolve_auto_audit_executor};

use crate::ctx::Ctx;

fn same_repo(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

/// Returns `true` when `task_repo` is the current repo OR has a live `aaf run`
/// process holding `<data_dir>/locks/<repo_id>.run.lock`.
pub(super) fn repo_has_live_run(data_dir: &Path, current_repo: &Path, task_repo: &str) -> bool {
    let task_path = Path::new(task_repo);
    if same_repo(task_path, current_repo) {
        return true;
    }
    let lock_path = data_dir
        .join("locks")
        .join(format!("{}.run.lock", aaf_fp::repo_id(task_path)));
    let is_reg =
        fs::symlink_metadata(&lock_path).is_ok_and(|m| !m.file_type().is_symlink() && m.is_file());
    if !is_reg {
        return false;
    }
    File::open(&lock_path).is_ok_and(|file| match file.try_lock_shared() {
        Err(TryLockError::WouldBlock) => true,
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(TryLockError::Error(_)) => false,
    })
}

/// Returns `true` when `repo_cfg` can invoke `provider` during post-gate audit
/// or dual-review.
pub(super) fn repo_audits_on_provider(
    repo_cfg: &RepoConfig,
    path_var: &OsStr,
    home: &Path,
    provider: ProviderKind,
) -> bool {
    let eff = configured_audit_provider(&repo_cfg.run.executor, &repo_cfg.cross_audit);
    let resolved = resolve_auto_audit_executor(&repo_cfg.run.executor, eff, path_var, home);
    if ProviderKey::from_executor(&resolved).provider_kind() == Some(provider) {
        return true;
    }
    if repo_cfg.cross_audit.dual_review {
        if repo_cfg.cross_audit.reviewers.is_empty() {
            if ProviderKey::from_executor(&repo_cfg.run.executor).provider_kind() == Some(provider)
            {
                return true;
            }
        } else if repo_cfg
            .cross_audit
            .reviewers
            .iter()
            .any(|r| ProviderKey::from_executor(&r.executor).provider_kind() == Some(provider))
        {
            return true;
        }
    }
    false
}

/// Derives `strict_audit_reserve` per [`ProviderKey`] solely from repo config,
/// independent of lazy stage-insertion order (`ro`).
pub(super) fn is_auxiliary_auditor(ctx: &Ctx, cfg: &EffectiveConfig, key: &ProviderKey) -> bool {
    let primary = ProviderKey::from_executor(&cfg.repo.run.executor);
    key.provider_kind() == Some(ProviderKind::Antigravity)
        && key.provider_kind() != primary.provider_kind()
        && repo_audits_on_provider(
            &cfg.repo,
            &ctx.path_var,
            &ctx.home,
            ProviderKind::Antigravity,
        )
}

#[derive(Clone)]
struct ReserveCtx {
    data_dir: PathBuf,
    config_dir: PathBuf,
    current_repo: PathBuf,
    current_repo_cfg: RepoConfig,
    path_var: OsString,
    home: PathBuf,
}

impl ReserveCtx {
    fn from_ctx(ctx: &Ctx, repo: &Path, cfg: &EffectiveConfig) -> Self {
        Self {
            data_dir: ctx.data_dir.clone(),
            config_dir: ctx.config_dir.clone(),
            current_repo: repo.to_path_buf(),
            current_repo_cfg: cfg.repo.clone(),
            path_var: ctx.path_var.clone(),
            home: ctx.home.clone(),
        }
    }

    fn task_needs_provider_audit(&self, task: &Task, provider: ProviderKind) -> bool {
        if !repo_has_live_run(&self.data_dir, &self.current_repo, &task.repo_root) {
            return false;
        }
        let task_path = Path::new(&task.repo_root);
        if same_repo(task_path, &self.current_repo) {
            return repo_audits_on_provider(
                &self.current_repo_cfg,
                &self.path_var,
                &self.home,
                provider,
            );
        }
        if !task_path.join(REPO_CONFIG_REL).is_file() && !task_path.join(LOCAL_CONFIG_REL).is_file()
        {
            // If another live repo has no `.autofactory/config*.toml` on disk
            // (e.g. unit test fixtures), conservatively assume it may audit on `provider`.
            return true;
        }
        let Ok(eff) = config::load(&ConfigInputs {
            global_config: Some(self.config_dir.join("config.toml")),
            repo_root: task_path.to_path_buf(),
            env: &|_| None,
            cli: ConfigOverlay::default(),
        }) else {
            return true;
        };
        repo_audits_on_provider(&eff.repo, &self.path_var, &self.home, provider)
    }
}

/// Returns a closure reporting the author account id (if any) of every live
/// `TaskState::Gating` task that may audit on `provider`.
pub(super) fn gating_authors_provider(
    ctx: &Ctx,
    repo: &Path,
    cfg: &EffectiveConfig,
    db: Db,
) -> Arc<dyn Fn(ProviderKind) -> Vec<Option<String>> + Send + Sync> {
    let rctx = ReserveCtx::from_ctx(ctx, repo, cfg);
    Arc::new(move |provider| {
        let Ok(tasks) = db.list_tasks(None, Some(TaskState::Gating)) else {
            return Vec::new();
        };
        tasks
            .into_iter()
            .filter(|t| rctx.task_needs_provider_audit(t, provider))
            .map(|t| {
                RunHistory::last_account_of_task(&db, &t.id, provider)
                    .ok()
                    .flatten()
            })
            .collect()
    })
}

/// Returns a closure reporting whether any live `TaskState::Gating` task needs
/// an audit on `provider` (used for strict single-slot Antigravity reserve).
pub(super) fn has_live_gating_for_provider(
    ctx: &Ctx,
    repo: &Path,
    cfg: &EffectiveConfig,
    db: Db,
    provider: ProviderKind,
) -> Arc<dyn Fn() -> bool + Send + Sync> {
    let rctx = ReserveCtx::from_ctx(ctx, repo, cfg);
    Arc::new(move || {
        db.list_tasks(None, Some(TaskState::Gating))
            .is_ok_and(|tasks| {
                tasks
                    .iter()
                    .any(|t| rctx.task_needs_provider_audit(t, provider))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaf_domain::config::{GlobalConfig, RepoConfig};
    use aaf_domain::pipeline::ReviewerConfig;
    use aaf_domain::task::{ImmutableSpec, Priority};

    fn test_ctx(dir: &Path) -> Ctx {
        Ctx {
            cwd: dir.to_path_buf(),
            home: dir.to_path_buf(),
            data_dir: dir.to_path_buf(),
            worktrees_dir: dir.join("wt"),
            config_dir: dir.to_path_buf(),
            path_var: OsString::new(),
            run_id: None,
            stdin_is_tty: false,
            aaf_env: Vec::new(),
        }
    }

    #[test]
    fn stopped_repo_gating_task_never_blocks_live_repo_and_claude_only_never_blocks_agy() {
        let dir = tempfile::tempdir().unwrap();
        let repo_a = dir.path().join("repo-a");
        let repo_b = dir.path().join("repo-b");
        fs::create_dir_all(repo_a.join(".autofactory")).unwrap();
        fs::create_dir_all(&repo_b).unwrap();
        // Repo A explicitly audits ONLY on antigravity (non-default executor/auditor)
        // to prove `.autofactory/config.toml` is genuinely loaded from disk.
        fs::write(
            repo_a.join(REPO_CONFIG_REL),
            "[run]\nexecutor = \"antigravity\"\n[cross_audit]\nexecutor = \"antigravity\"\ndual_review = false\n",
        )
        .unwrap();

        let db = Db::open(&dir.path().join("aaf.db")).unwrap();
        let mut t = Task::new_root(
            "t-gate-a",
            repo_a.to_string_lossy().into_owned(),
            "gating task",
            ImmutableSpec {
                description: String::new(),
                acceptance: Vec::new(),
            },
            Priority::P1,
            100,
        );
        t.state = TaskState::Gating;
        db.insert_task(&t).unwrap();

        let ctx = test_ctx(dir.path());
        let cfg = EffectiveConfig {
            global: GlobalConfig::default(),
            repo: RepoConfig::default(),
            sources: Vec::new(),
            warnings: Vec::new(),
        };

        // 1. Stopped Repo A (no live .run.lock) must NOT reserve Claude or Antigravity in Repo B.
        let authors = gating_authors_provider(&ctx, &repo_b, &cfg, db.clone());
        let has_agy = has_live_gating_for_provider(
            &ctx,
            &repo_b,
            &cfg,
            db.clone(),
            ProviderKind::Antigravity,
        );
        assert!(authors(ProviderKind::ClaudeCode).is_empty());
        assert!(!has_agy());

        // 2. Once Repo A holds a live .run.lock, it reserves Antigravity (its
        //    configured auditor) and does NOT reserve Claude in Repo B.
        let locks = dir.path().join("locks");
        fs::create_dir_all(&locks).unwrap();
        let lock_file =
            File::create(locks.join(format!("{}.run.lock", aaf_fp::repo_id(&repo_a)))).unwrap();
        lock_file.lock().unwrap();
        assert!(
            authors(ProviderKind::ClaudeCode).is_empty(),
            "Antigravity-only live Repo A in Gating must not reserve Claude in Repo B"
        );
        assert!(
            has_agy(),
            "Antigravity-only live Repo A in Gating must reserve Antigravity in Repo B"
        );
    }

    #[test]
    fn is_auxiliary_auditor_depends_only_on_config_not_stage_order() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let agy_key = ProviderKey::from_executor("antigravity");

        // Claude primary + Claude-only audit -> antigravity is NOT auxiliary auditor
        // even if plan_author uses antigravity.
        let mut repo = RepoConfig::default();
        repo.cross_audit.executor = "claude".into();
        repo.cross_audit.dual_review = false;
        let cfg = EffectiveConfig {
            global: GlobalConfig::default(),
            repo: repo.clone(),
            sources: Vec::new(),
            warnings: Vec::new(),
        };
        assert!(!is_auxiliary_auditor(&ctx, &cfg, &agy_key));

        // Dual review with antigravity reviewer -> antigravity IS auxiliary auditor
        // even if implementer_escalated also uses antigravity.
        repo.cross_audit.dual_review = true;
        repo.cross_audit.reviewers = vec![ReviewerConfig {
            id: "agy".into(),
            executor: "antigravity".into(),
            ..ReviewerConfig::default()
        }];
        let cfg_dual = EffectiveConfig {
            global: GlobalConfig::default(),
            repo: repo.clone(),
            sources: Vec::new(),
            warnings: Vec::new(),
        };
        assert!(is_auxiliary_auditor(&ctx, &cfg_dual, &agy_key));

        // Primary executor alias (`gemini` == ProviderKind::Antigravity) must NOT
        // treat `antigravity` as an auxiliary auditor.
        repo.run.executor = "gemini".into();
        let cfg_gemini_primary = EffectiveConfig {
            global: GlobalConfig::default(),
            repo,
            sources: Vec::new(),
            warnings: Vec::new(),
        };
        assert!(!is_auxiliary_auditor(&ctx, &cfg_gemini_primary, &agy_key));
    }
}
