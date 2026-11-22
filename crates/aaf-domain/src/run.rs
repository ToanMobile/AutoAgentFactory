//! One execution of an agent on a task.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Succeeded,
    Failed,
    TimedOut,
    LimitHit,
    Killed,
}

impl RunStatus {
    pub const ALL: [RunStatus; 6] = [
        RunStatus::Running,
        RunStatus::Succeeded,
        RunStatus::Failed,
        RunStatus::TimedOut,
        RunStatus::LimitHit,
        RunStatus::Killed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Succeeded => "succeeded",
            RunStatus::Failed => "failed",
            RunStatus::TimedOut => "timed_out",
            RunStatus::LimitHit => "limit_hit",
            RunStatus::Killed => "killed",
        }
    }

    pub fn parse(s: &str) -> Option<RunStatus> {
        RunStatus::ALL
            .into_iter()
            .find(|status| status.as_str() == s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: String,
    pub task_id: String,
    pub account_id: Option<String>,
    pub executor: String,
    pub worktree_path: String,
    pub branch: String,
    pub status: RunStatus,
    pub exit_code: Option<i32>,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub started_at: i64,
    pub ended_at: Option<i64>,
}

/// Final numbers written when a run ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFinish {
    pub status: RunStatus,
    pub exit_code: Option<i32>,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub ended_at: i64,
}
