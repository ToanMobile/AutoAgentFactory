//! Tasks, their lifecycle states and the immutable spec an agent works from.

use serde::{Deserialize, Serialize};

use crate::DomainError;
use crate::blocked::BlockedReason;
use crate::budget::MAX_SUBTASK_DEPTH;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Backlog,
    Ready,
    Running,
    Gating,
    Review,
    Blocked,
    Done,
    Cancelled,
}

impl TaskState {
    pub const ALL: [TaskState; 8] = [
        TaskState::Backlog,
        TaskState::Ready,
        TaskState::Running,
        TaskState::Gating,
        TaskState::Review,
        TaskState::Blocked,
        TaskState::Done,
        TaskState::Cancelled,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Backlog => "backlog",
            TaskState::Ready => "ready",
            TaskState::Running => "running",
            TaskState::Gating => "gating",
            TaskState::Review => "review",
            TaskState::Blocked => "blocked",
            TaskState::Done => "done",
            TaskState::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<TaskState> {
        TaskState::ALL.into_iter().find(|state| state.as_str() == s)
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, TaskState::Done | TaskState::Cancelled)
    }

    /// The only legal lifecycle moves. `Blocked` is re-entered through `Ready`.
    pub fn can_transition_to(self, next: TaskState) -> bool {
        use TaskState::{Backlog, Blocked, Cancelled, Done, Gating, Ready, Review, Running};
        matches!(
            (self, next),
            (Backlog, Ready | Cancelled)
                | (Ready, Running | Blocked | Cancelled)
                | (Running, Gating | Blocked | Ready | Cancelled)
                | (Gating, Review | Done | Blocked | Ready | Cancelled)
                | (Review, Done | Blocked | Ready | Cancelled)
                | (Blocked, Ready | Cancelled)
        )
    }

    pub fn transition(self, next: TaskState) -> Result<TaskState, DomainError> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(DomainError::InvalidTransition {
                from: self.as_str(),
                to: next.as_str(),
            })
        }
    }
}

/// `P0` is the most urgent; the derived ordering sorts it first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Priority {
    P0,
    P1,
    P2,
    P3,
}

impl Priority {
    pub fn as_str(self) -> &'static str {
        match self {
            Priority::P0 => "P0",
            Priority::P1 => "P1",
            Priority::P2 => "P2",
            Priority::P3 => "P3",
        }
    }

    pub fn parse(s: &str) -> Option<Priority> {
        match s.to_ascii_uppercase().as_str() {
            "P0" => Some(Priority::P0),
            "P1" => Some(Priority::P1),
            "P2" => Some(Priority::P2),
            "P3" => Some(Priority::P3),
            _ => None,
        }
    }
}

/// What the task must achieve. Written once at creation and never edited:
/// the unblocker may add `context_notes` but must not touch this.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImmutableSpec {
    pub description: String,
    pub acceptance: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub repo_root: String,
    pub parent_id: Option<String>,
    /// Id of the top-level task; equals `id` for a root task.
    pub root_id: String,
    pub depth: u8,
    pub title: String,
    pub spec: ImmutableSpec,
    pub context_notes: Vec<String>,
    pub state: TaskState,
    pub priority: Priority,
    pub fingerprint: Option<String>,
    pub blocked_reason: Option<BlockedReason>,
    pub unblock_cycles: u32,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Task {
    pub fn new_root(
        id: impl Into<String>,
        repo_root: impl Into<String>,
        title: impl Into<String>,
        spec: ImmutableSpec,
        priority: Priority,
        now: i64,
    ) -> Task {
        let id = id.into();
        Task {
            root_id: id.clone(),
            id,
            repo_root: repo_root.into(),
            parent_id: None,
            depth: 0,
            title: title.into(),
            spec,
            context_notes: Vec::new(),
            state: TaskState::Backlog,
            priority,
            fingerprint: None,
            blocked_reason: None,
            unblock_cycles: 0,
            created_at: now,
            updated_at: now,
        }
    }

    /// A subtask shares the parent's root (and therefore its budget).
    pub fn new_subtask(
        id: impl Into<String>,
        parent: &Task,
        title: impl Into<String>,
        spec: ImmutableSpec,
        now: i64,
    ) -> Result<Task, DomainError> {
        let depth = parent.depth.saturating_add(1);
        if depth > MAX_SUBTASK_DEPTH {
            return Err(DomainError::SubtaskDepthExceeded {
                depth,
                max: MAX_SUBTASK_DEPTH,
            });
        }
        Ok(Task {
            id: id.into(),
            repo_root: parent.repo_root.clone(),
            parent_id: Some(parent.id.clone()),
            root_id: parent.root_id.clone(),
            depth,
            title: title.into(),
            spec,
            context_notes: Vec::new(),
            state: TaskState::Ready,
            priority: parent.priority,
            fingerprint: None,
            blocked_reason: None,
            unblock_cycles: 0,
            created_at: now,
            updated_at: now,
        })
    }
}
