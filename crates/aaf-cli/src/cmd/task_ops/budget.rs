//! Root budget reset & repo-config limit synchronization for human task ops.

use std::path::Path;

use aaf_domain::budget::{BudgetCheck, RootBudget, scaled_token_limit, scaled_wall_limit};
use aaf_domain::ports::{BudgetStore, PortError, TaskStore};
use aaf_domain::task::TaskState;
use aaf_engine::db::Db;

fn active_subtask_count(db: &Db, root_id: &str) -> usize {
    db.children_of(root_id)
        .map(|kids| {
            kids.into_iter()
                .filter(|t| t.state != TaskState::Cancelled)
                .count()
        })
        .unwrap_or(0)
}

fn repo_unblocker_limits(repo_root: &str) -> Option<(u64, u64, u32)> {
    let root = Path::new(repo_root);
    let global_config = std::env::var_os("AAF_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|h| std::path::PathBuf::from(h).join(".config/autofactory"))
        })
        .map(|d| d.join("config.toml"))
        .filter(|p| p.is_file());
    let has_repo_cfg = root.join(".autofactory/config.toml").is_file();
    if !has_repo_cfg && global_config.is_none() {
        return None;
    }
    let no_env = |_: &str| None;
    let eff = aaf_engine::config::load(&aaf_engine::config::ConfigInputs {
        global_config,
        repo_root: root.to_path_buf(),
        env: &no_env,
        cli: aaf_engine::config::ConfigOverlay::default(),
    })
    .ok()?;
    let u = eff.repo.unblocker;
    Some((
        u.max_token_budget,
        u.max_wall_clock_secs,
        u.max_unblock_cycles,
    ))
}

pub(crate) fn reset_budget_for_unblock(
    db: &Db,
    root_id: &str,
    repo_root: &str,
) -> Result<bool, PortError> {
    match db.get_budget(root_id) {
        Ok(budget) => {
            let has_usage = budget.check() != BudgetCheck::Ok
                || budget.tokens_used > 0
                || budget.wall_secs_used > 0
                || budget.cycles_used > 0;
            let cfg_limits = repo_unblocker_limits(repo_root).map(|(t, w, c)| {
                let subs = active_subtask_count(db, root_id);
                (scaled_token_limit(t, subs), scaled_wall_limit(w, subs), c)
            });
            let limits_changed = cfg_limits.is_some_and(|(t, w, c)| {
                budget.token_limit != t || budget.wall_secs_limit != w || budget.cycles_limit != c
            });
            if !has_usage && !limits_changed {
                return Ok(false);
            }
            let (token_limit, wall_secs_limit, cycles_limit) = cfg_limits.unwrap_or((
                budget.token_limit,
                budget.wall_secs_limit,
                budget.cycles_limit,
            ));
            db.put_budget(&RootBudget {
                tokens_used: 0,
                wall_secs_used: 0,
                cycles_used: 0,
                frozen: false,
                token_limit,
                wall_secs_limit,
                cycles_limit,
                ..budget
            })?;
            Ok(has_usage)
        }
        Err(PortError::NotFound(_)) => Ok(false),
        Err(other) => Err(other),
    }
}

pub(crate) fn reset_exhausted_budget(
    db: &Db,
    root_id: &str,
    repo_root: &str,
) -> Result<bool, PortError> {
    match db.get_budget(root_id) {
        Ok(budget) if budget.check() != BudgetCheck::Ok => {
            let (token_limit, wall_secs_limit, cycles_limit) = repo_unblocker_limits(repo_root)
                .map(|(t, w, c)| {
                    let subs = active_subtask_count(db, root_id);
                    (scaled_token_limit(t, subs), scaled_wall_limit(w, subs), c)
                })
                .unwrap_or((
                    budget.token_limit,
                    budget.wall_secs_limit,
                    budget.cycles_limit,
                ));
            db.put_budget(&RootBudget {
                tokens_used: 0,
                wall_secs_used: 0,
                cycles_used: 0,
                frozen: false,
                token_limit,
                wall_secs_limit,
                cycles_limit,
                ..budget
            })?;
            Ok(true)
        }
        Ok(_) | Err(PortError::NotFound(_)) => Ok(false),
        Err(other) => Err(other),
    }
}
