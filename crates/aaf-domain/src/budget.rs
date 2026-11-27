//! Hierarchical root budget: every subtask spends from its root's budget.

use serde::{Deserialize, Serialize};

use crate::config::UnblockerConfig;

/// Subtasks may not spawn subtasks.
pub const MAX_SUBTASK_DEPTH: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootBudget {
    pub root_id: String,
    /// input + output + cache-creation tokens (cache reads are not counted).
    pub tokens_used: u64,
    pub token_limit: u64,
    pub wall_secs_used: u64,
    pub wall_secs_limit: u64,
    pub cycles_used: u32,
    pub cycles_limit: u32,
    pub frozen: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetCheck {
    Ok,
    Frozen,
    TokensExceeded,
    WallClockExceeded,
    CyclesExceeded,
}

impl RootBudget {
    pub fn new(root_id: impl Into<String>, cfg: &UnblockerConfig) -> RootBudget {
        RootBudget {
            root_id: root_id.into(),
            tokens_used: 0,
            token_limit: cfg.max_token_budget,
            wall_secs_used: 0,
            wall_secs_limit: cfg.max_wall_clock_secs,
            cycles_used: 0,
            cycles_limit: cfg.max_unblock_cycles,
            frozen: false,
        }
    }

    pub fn new_scaled(
        root_id: impl Into<String>,
        cfg: &UnblockerConfig,
        active_subtasks: usize,
    ) -> RootBudget {
        let mut budget = RootBudget::new(root_id, cfg);
        budget.token_limit = scaled_token_limit(cfg.max_token_budget, active_subtasks);
        budget.wall_secs_limit = scaled_wall_limit(cfg.max_wall_clock_secs, active_subtasks);
        budget
    }

    /// A limit of `N` allows exactly `N`; the budget is exceeded past it.
    pub fn check(&self) -> BudgetCheck {
        if self.frozen {
            BudgetCheck::Frozen
        } else if self.tokens_used > self.token_limit {
            BudgetCheck::TokensExceeded
        } else if self.wall_secs_used > self.wall_secs_limit {
            BudgetCheck::WallClockExceeded
        } else if self.cycles_used > self.cycles_limit {
            BudgetCheck::CyclesExceeded
        } else {
            BudgetCheck::Ok
        }
    }

    pub fn add_usage(&mut self, tokens: u64, wall_secs: u64, cycles: u32) {
        self.tokens_used = self.tokens_used.saturating_add(tokens);
        self.wall_secs_used = self.wall_secs_used.saturating_add(wall_secs);
        self.cycles_used = self.cycles_used.saturating_add(cycles);
    }
}

/// Whether a task at `parent_depth` may spawn a subtask.
pub fn can_spawn_subtask(parent_depth: u8, max_depth: u8) -> bool {
    parent_depth < max_depth.min(MAX_SUBTASK_DEPTH)
}

fn scale_by_subtasks(base: u64, active_subtasks: usize) -> u64 {
    let extra = active_subtasks.saturating_sub(1).min(3) as u128;
    let added = (u128::from(base) * extra) / 2;
    let scaled = base.saturating_add(u64::try_from(added).unwrap_or(u64::MAX));
    if base <= i64::MAX as u64 {
        scaled.min(i64::MAX as u64)
    } else {
        scaled
    }
}

/// Scales a root task's `max_token_budget` when T4 splits it into multiple
/// active (non-cancelled) subtasks: 0–1 subtask keeps `base` unchanged; each
/// additional subtask adds `base / 2`, capped at +3 extra subtasks (2.5× base).
pub fn scaled_token_limit(base: u64, active_subtasks: usize) -> u64 {
    scale_by_subtasks(base, active_subtasks)
}

/// Scales a root task's `max_wall_clock_secs` across active (non-cancelled)
/// subtasks using the same 1.0×..2.5× curve as [`scaled_token_limit`].
pub fn scaled_wall_limit(base: u64, active_subtasks: usize) -> u64 {
    scale_by_subtasks(base, active_subtasks)
}
