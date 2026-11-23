//! Pure domain types for declarative SOP workflows and crash-safe checkpoints (no I/O).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::dag::topo_order;
use crate::gate::GateMode;

/// Hard upper bound on automatic reworks per SOP run to guarantee termination.
pub const MAX_SOP_REWORKS: u32 = 10;

/// Default script stage timeout in seconds when omitted in TOML.
pub const DEFAULT_SCRIPT_TIMEOUT_SECS: u64 = 300;

fn default_version() -> u32 {
    1
}

fn default_profile() -> String {
    "auto".to_string()
}

fn default_script_timeout() -> u64 {
    DEFAULT_SCRIPT_TIMEOUT_SECS
}

/// Checks whether a stage or SOP slug matches `[a-z0-9_-]+` (1..=64 bytes).
pub fn is_valid_stage_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Checks whether a checkpoint `run_id` is safe for filenames (`[A-Za-z0-9_-]+`, 1..=128 bytes).
pub fn is_valid_run_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Declarative Standard Operating Procedure specification (DAG of stages).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SopSpec {
    pub name: String,
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_profile")]
    pub profile: String,
    #[serde(default)]
    pub max_reworks: u32,
    pub stages: Vec<SopStage>,
}

/// One node in an SOP DAG.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SopStage {
    pub id: String,
    pub title: String,
    pub kind: StageKind,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub on_fail_goto: Option<String>,
    #[serde(default)]
    pub artifacts: Vec<String>,
}

/// Execution semantics of an SOP stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum StageKind {
    Agent {
        role: String,
        #[serde(default)]
        executor: Option<String>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        read_only: bool,
        #[serde(default)]
        prompt: String,
    },
    Script {
        command: String,
        #[serde(default)]
        heavy: bool,
        #[serde(default = "default_script_timeout")]
        timeout_secs: u64,
    },
    Gate {
        mode: GateMode,
    },
    HumanGate {
        prompt: String,
    },
}

/// Lifecycle state of an individual stage inside a run checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    Pending,
    Running,
    Passed,
    Failed,
    WaitingHuman,
    Skipped,
}

/// Overall lifecycle state of an SOP run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SopRunStatus {
    Running,
    WaitingHuman,
    Completed,
    Failed,
}

/// Structural and safety validation errors for an [`SopSpec`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SopValidationError {
    #[error("SOP name must not be empty")]
    EmptyName,
    #[error("SOP must define at least one stage")]
    NoStages,
    #[error("stage id {0:?} must match [a-z0-9_-]+")]
    InvalidStageId(String),
    #[error("duplicate stage id {0:?}")]
    DuplicateStageId(String),
    #[error("stage {stage:?} depends on itself")]
    SelfDependency { stage: String },
    #[error("stage {stage:?} depends on unknown stage {dependency:?}")]
    UnknownDependency { stage: String, dependency: String },
    #[error("SOP stage dependency graph contains a cycle")]
    DependencyCycle,
    #[error("stage {stage:?} on_fail_goto references unknown stage {target:?}")]
    UnknownReworkTarget { stage: String, target: String },
    #[error("max_reworks must be between 1 and {max} when on_fail_goto is used (got {got})")]
    InvalidMaxReworks { got: u32, max: u32 },
    #[error("max_reworks {got} exceeds maximum allowed {max}")]
    MaxReworksTooHigh { got: u32, max: u32 },
    #[error("script stage {stage:?} must have a non-empty command")]
    EmptyScriptCommand { stage: String },
    #[error("agent stage {stage:?} must have a non-empty role")]
    EmptyAgentRole { stage: String },
    #[error("human_gate stage {stage:?} must have a non-empty prompt")]
    EmptyHumanGatePrompt { stage: String },
}

impl SopSpec {
    /// Validates the SOP spec and returns stages in deterministic topological order
    /// (dependencies before dependents).
    pub fn validate(&self) -> Result<Vec<String>, SopValidationError> {
        if self.name.trim().is_empty() {
            return Err(SopValidationError::EmptyName);
        }
        if self.stages.is_empty() {
            return Err(SopValidationError::NoStages);
        }
        if self.max_reworks > MAX_SOP_REWORKS {
            return Err(SopValidationError::MaxReworksTooHigh {
                got: self.max_reworks,
                max: MAX_SOP_REWORKS,
            });
        }

        let mut known = HashSet::with_capacity(self.stages.len());
        let mut stage_ids = Vec::with_capacity(self.stages.len());
        for stage in &self.stages {
            if !is_valid_stage_id(&stage.id) {
                return Err(SopValidationError::InvalidStageId(stage.id.clone()));
            }
            if !known.insert(stage.id.as_str()) {
                return Err(SopValidationError::DuplicateStageId(stage.id.clone()));
            }
            stage_ids.push(stage.id.clone());
        }

        let mut edges = Vec::new();
        let mut has_rework = false;
        for stage in &self.stages {
            match &stage.kind {
                StageKind::Script { command, .. } if command.trim().is_empty() => {
                    return Err(SopValidationError::EmptyScriptCommand {
                        stage: stage.id.clone(),
                    });
                }
                StageKind::Agent { role, .. } if role.trim().is_empty() => {
                    return Err(SopValidationError::EmptyAgentRole {
                        stage: stage.id.clone(),
                    });
                }
                StageKind::HumanGate { prompt } if prompt.trim().is_empty() => {
                    return Err(SopValidationError::EmptyHumanGatePrompt {
                        stage: stage.id.clone(),
                    });
                }
                _ => {}
            }

            for dep in &stage.depends_on {
                if dep == &stage.id {
                    return Err(SopValidationError::SelfDependency {
                        stage: stage.id.clone(),
                    });
                }
                if !known.contains(dep.as_str()) {
                    return Err(SopValidationError::UnknownDependency {
                        stage: stage.id.clone(),
                        dependency: dep.clone(),
                    });
                }
                edges.push((stage.id.clone(), dep.clone()));
            }

            if let Some(target) = &stage.on_fail_goto {
                has_rework = true;
                if !known.contains(target.as_str()) {
                    return Err(SopValidationError::UnknownReworkTarget {
                        stage: stage.id.clone(),
                        target: target.clone(),
                    });
                }
            }
        }

        if has_rework && (self.max_reworks == 0 || self.max_reworks > MAX_SOP_REWORKS) {
            return Err(SopValidationError::InvalidMaxReworks {
                got: self.max_reworks,
                max: MAX_SOP_REWORKS,
            });
        }

        topo_order(&stage_ids, &edges).ok_or(SopValidationError::DependencyCycle)
    }

    /// Looks up a stage definition by id.
    pub fn stage(&self, id: &str) -> Option<&SopStage> {
        self.stages.iter().find(|s| s.id == id)
    }

    /// Returns `target_id` together with every stage that transitively depends on it.
    pub fn transitive_dependents(&self, target_id: &str) -> Vec<String> {
        let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
        for stage in &self.stages {
            for dep in &stage.depends_on {
                children
                    .entry(dep.as_str())
                    .or_default()
                    .push(stage.id.as_str());
            }
        }
        let mut seen = HashSet::new();
        let mut queue = VecDeque::new();
        if self.stage(target_id).is_some() {
            seen.insert(target_id);
            queue.push_back(target_id);
        }
        while let Some(cur) = queue.pop_front() {
            if let Some(next_list) = children.get(cur) {
                for &next in next_list {
                    if seen.insert(next) {
                        queue.push_back(next);
                    }
                }
            }
        }
        self.stages
            .iter()
            .filter(|s| seen.contains(s.id.as_str()))
            .map(|s| s.id.clone())
            .collect()
    }
}

/// Recorded execution state for a single stage in an [`SopCheckpoint`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageRecord {
    pub status: StageStatus,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub started_at: Option<i64>,
    #[serde(default)]
    pub finished_at: Option<i64>,
}

impl StageRecord {
    pub fn pending() -> Self {
        Self {
            status: StageStatus::Pending,
            attempts: 0,
            summary: None,
            artifacts: Vec::new(),
            started_at: None,
            finished_at: None,
        }
    }
}

/// Event kind stored in an [`SopCheckpoint`] audit trail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SopEventKind {
    StageStarted,
    StagePassed,
    StageFailed,
    WaitingHuman,
    HumanApproved,
    Rework,
    SendBack,
    Completed,
    Failed,
}

/// Immutable audit log entry inside an [`SopCheckpoint`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SopEvent {
    pub timestamp: i64,
    pub kind: SopEventKind,
    #[serde(default)]
    pub stage_id: Option<String>,
    pub message: String,
}

/// Crash-safe serializable checkpoint for resuming an SOP workflow run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SopCheckpoint {
    pub run_id: String,
    pub sop_name: String,
    pub repo_root: String,
    pub status: SopRunStatus,
    pub rework_count: u32,
    pub stage_states: BTreeMap<String, StageRecord>,
    pub history: Vec<SopEvent>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl SopCheckpoint {
    /// Initializes a fresh checkpoint with every stage in `spec` set to [`StageStatus::Pending`].
    pub fn new(run_id: String, spec: &SopSpec, repo_root: String, now: i64) -> Self {
        let mut stage_states = BTreeMap::new();
        for stage in &spec.stages {
            stage_states.insert(stage.id.clone(), StageRecord::pending());
        }
        Self {
            run_id,
            sop_name: spec.name.clone(),
            repo_root,
            status: SopRunStatus::Running,
            rework_count: 0,
            stage_states,
            history: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    /// Appends an event and bumps `updated_at`.
    pub fn push_event(
        &mut self,
        timestamp: i64,
        kind: SopEventKind,
        stage_id: Option<String>,
        message: impl Into<String>,
    ) {
        self.updated_at = timestamp;
        self.history.push(SopEvent {
            timestamp,
            kind,
            stage_id,
            message: message.into(),
        });
    }
}
