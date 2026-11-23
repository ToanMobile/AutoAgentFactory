//! Resumable SOP workflow runner and crash-safe checkpoint persistence.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

use aaf_domain::ports::Clock;
use aaf_domain::sop::{
    SopCheckpoint, SopEventKind, SopRunStatus, SopSpec, SopStage, SopValidationError, StageRecord,
    StageStatus, is_valid_run_id,
};

use super::{
    atomic_write_bytes, check_existing_real_dir, ensure_real_dir_mode, refuse_symlink_or_non_file,
};

/// Subdirectory under `AAF_DATA_DIR` holding SOP checkpoints (`0700`).
pub const SOPS_DIR_NAME: &str = "sops";

const MAX_CHECKPOINT_BYTES: u64 = 16 * 1024 * 1024;

/// Errors returned by SOP discovery, validation, execution, and checkpoint I/O.
#[derive(Debug, thiserror::Error)]
pub enum SopError {
    #[error("invalid SOP specification: {0}")]
    Validation(#[from] SopValidationError),
    #[error("invalid run_id {0:?}: must match [A-Za-z0-9_-]+")]
    InvalidRunId(String),
    #[error("unsafe path: {0}")]
    InvalidPath(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("file already exists: {} (use --force to overwrite)", .0.display())]
    AlreadyExists(PathBuf),
    #[error("SOP run {0:?} has no stage waiting for human approval")]
    NotWaitingHuman(String),
    #[error("unknown stage {0:?}")]
    UnknownStage(String),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("io error: {0}")]
    Io(String),
}

/// Result returned by a [`StageDriver`] for one stage execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageOutcome {
    Passed {
        summary: String,
        artifacts: Vec<String>,
    },
    Failed {
        reason: String,
    },
    WaitHuman {
        prompt: String,
    },
}

/// Execution seam for SOP stages (faked in unit tests, wired to shell/gate in CLI).
pub trait StageDriver {
    fn run_stage(&mut self, stage: &SopStage, cp: &SopCheckpoint) -> StageOutcome;
}

/// Validates that `run_id` is safe as a checkpoint filename stem.
pub fn validate_run_id(run_id: &str) -> Result<(), SopError> {
    if is_valid_run_id(run_id) {
        Ok(())
    } else {
        Err(SopError::InvalidRunId(run_id.to_string()))
    }
}

/// Returns all currently ready stages in deterministic topological order:
/// status is `Pending` (or `Running` after a crash) and every dependency is `Passed`/`Skipped`.
pub fn ready_stages<'a>(
    spec: &'a SopSpec,
    cp: &SopCheckpoint,
) -> Result<Vec<&'a SopStage>, SopError> {
    let order = spec.validate()?;
    let mut ready = Vec::new();
    for id in order {
        let Some(stage) = spec.stage(&id) else {
            continue;
        };
        let status = cp
            .stage_states
            .get(&id)
            .map_or(StageStatus::Pending, |r| r.status);
        if !matches!(status, StageStatus::Pending | StageStatus::Running) {
            continue;
        }
        let deps_done = stage.depends_on.iter().all(|dep| {
            cp.stage_states
                .get(dep)
                .is_some_and(|r| matches!(r.status, StageStatus::Passed | StageStatus::Skipped))
        });
        if deps_done {
            ready.push(stage);
        }
    }
    Ok(ready)
}

/// Steps an SOP run forward in memory until it pauses at a human gate, completes, or fails.
/// Already-`Passed` stages are skipped, making resume idempotent.
pub fn step_sop(
    spec: &SopSpec,
    cp: &mut SopCheckpoint,
    driver: &mut dyn StageDriver,
    clock: &dyn Clock,
) -> Result<SopRunStatus, SopError> {
    execute_sop_loop(spec, cp, None, driver, clock)
}

/// Steps an SOP run forward while persisting the checkpoint after every stage transition
/// under `<data_dir>/sops/<run_id>.json` (`0700` dir, `0600` file, symlink-safe).
pub fn run_sop_persisted(
    spec: &SopSpec,
    cp: &mut SopCheckpoint,
    data_dir: &Path,
    driver: &mut dyn StageDriver,
    clock: &dyn Clock,
) -> Result<SopRunStatus, SopError> {
    validate_run_id(&cp.run_id)?;
    execute_sop_loop(spec, cp, Some(data_dir), driver, clock)
}

fn execute_sop_loop(
    spec: &SopSpec,
    cp: &mut SopCheckpoint,
    data_dir: Option<&Path>,
    driver: &mut dyn StageDriver,
    clock: &dyn Clock,
) -> Result<SopRunStatus, SopError> {
    let order = spec.validate()?;
    for stage in &spec.stages {
        cp.stage_states
            .entry(stage.id.clone())
            .or_insert_with(StageRecord::pending);
    }

    if cp.status != SopRunStatus::Running {
        return Ok(cp.status);
    }

    let max_iterations = (usize::try_from(spec.max_reworks).unwrap_or(10) + 1)
        .saturating_mul(spec.stages.len().max(1))
        .saturating_add(1);

    for _ in 0..max_iterations {
        let Some(stage_id) = next_ready_id(spec, cp, &order) else {
            return finalize_when_no_ready(spec, cp, data_dir, clock);
        };
        let Some(stage) = spec.stage(&stage_id) else {
            return Err(SopError::UnknownStage(stage_id));
        };

        let start_ts = clock.now();
        {
            let rec = cp
                .stage_states
                .entry(stage.id.clone())
                .or_insert_with(StageRecord::pending);
            rec.status = StageStatus::Running;
            rec.attempts = rec.attempts.saturating_add(1);
            rec.started_at = Some(start_ts);
            rec.finished_at = None;
        }
        cp.push_event(
            start_ts,
            SopEventKind::StageStarted,
            Some(stage.id.clone()),
            format!("started stage '{}'", stage.title),
        );
        persist_if_configured(data_dir, cp)?;

        let outcome = driver.run_stage(stage, cp);
        let finish_ts = clock.now();

        match outcome {
            StageOutcome::Passed { summary, artifacts } => {
                let rec = cp
                    .stage_states
                    .entry(stage.id.clone())
                    .or_insert_with(StageRecord::pending);
                rec.status = StageStatus::Passed;
                rec.summary = Some(summary.clone());
                rec.artifacts = artifacts;
                rec.finished_at = Some(finish_ts);
                cp.push_event(
                    finish_ts,
                    SopEventKind::StagePassed,
                    Some(stage.id.clone()),
                    summary,
                );
                persist_if_configured(data_dir, cp)?;
            }
            StageOutcome::WaitHuman { prompt } => {
                let rec = cp
                    .stage_states
                    .entry(stage.id.clone())
                    .or_insert_with(StageRecord::pending);
                rec.status = StageStatus::WaitingHuman;
                rec.summary = Some(prompt.clone());
                cp.status = SopRunStatus::WaitingHuman;
                cp.push_event(
                    finish_ts,
                    SopEventKind::WaitingHuman,
                    Some(stage.id.clone()),
                    prompt,
                );
                persist_if_configured(data_dir, cp)?;
                return Ok(SopRunStatus::WaitingHuman);
            }
            StageOutcome::Failed { reason } => {
                let rec = cp
                    .stage_states
                    .entry(stage.id.clone())
                    .or_insert_with(StageRecord::pending);
                rec.status = StageStatus::Failed;
                rec.summary = Some(reason.clone());
                rec.finished_at = Some(finish_ts);
                cp.push_event(
                    finish_ts,
                    SopEventKind::StageFailed,
                    Some(stage.id.clone()),
                    reason.clone(),
                );

                if let Some(target) = &stage.on_fail_goto
                    && cp.rework_count < spec.max_reworks
                {
                    cp.rework_count = cp.rework_count.saturating_add(1);
                    reset_downstream(spec, cp, target, Some(&stage.id));
                    cp.status = SopRunStatus::Running;
                    cp.push_event(
                        finish_ts,
                        SopEventKind::Rework,
                        Some(target.clone()),
                        format!(
                            "rework #{}/{}: '{}' failed -> resetting from '{}' ({reason})",
                            cp.rework_count, spec.max_reworks, stage.id, target
                        ),
                    );
                    persist_if_configured(data_dir, cp)?;
                    continue;
                }

                cp.status = SopRunStatus::Failed;
                cp.push_event(
                    finish_ts,
                    SopEventKind::Failed,
                    Some(stage.id.clone()),
                    format!("SOP failed at stage '{}': {reason}", stage.id),
                );
                persist_if_configured(data_dir, cp)?;
                return Ok(SopRunStatus::Failed);
            }
        }
    }

    let now = clock.now();
    cp.status = SopRunStatus::Failed;
    cp.push_event(
        now,
        SopEventKind::Failed,
        None,
        "SOP aborted after hitting internal iteration safeguard",
    );
    persist_if_configured(data_dir, cp)?;
    Ok(SopRunStatus::Failed)
}

fn next_ready_id(spec: &SopSpec, cp: &SopCheckpoint, order: &[String]) -> Option<String> {
    for id in order {
        let stage = spec.stage(id)?;
        let status = cp
            .stage_states
            .get(id)
            .map_or(StageStatus::Pending, |r| r.status);
        if !matches!(status, StageStatus::Pending | StageStatus::Running) {
            continue;
        }
        let deps_ok = stage.depends_on.iter().all(|dep| {
            cp.stage_states
                .get(dep)
                .is_some_and(|r| matches!(r.status, StageStatus::Passed | StageStatus::Skipped))
        });
        if deps_ok {
            return Some(id.clone());
        }
    }
    None
}

fn finalize_when_no_ready(
    spec: &SopSpec,
    cp: &mut SopCheckpoint,
    data_dir: Option<&Path>,
    clock: &dyn Clock,
) -> Result<SopRunStatus, SopError> {
    let now = clock.now();
    let all_done = spec.stages.iter().all(|s| {
        cp.stage_states
            .get(&s.id)
            .is_some_and(|r| matches!(r.status, StageStatus::Passed | StageStatus::Skipped))
    });
    if all_done {
        cp.status = SopRunStatus::Completed;
        cp.push_event(now, SopEventKind::Completed, None, "all SOP stages passed");
        persist_if_configured(data_dir, cp)?;
        return Ok(SopRunStatus::Completed);
    }
    let waiting = spec.stages.iter().any(|s| {
        cp.stage_states
            .get(&s.id)
            .is_some_and(|r| r.status == StageStatus::WaitingHuman)
    });
    if waiting {
        cp.status = SopRunStatus::WaitingHuman;
        persist_if_configured(data_dir, cp)?;
        return Ok(SopRunStatus::WaitingHuman);
    }
    cp.status = SopRunStatus::Failed;
    cp.push_event(
        now,
        SopEventKind::Failed,
        None,
        "no runnable stages remain before completion",
    );
    persist_if_configured(data_dir, cp)?;
    Ok(SopRunStatus::Failed)
}

fn reset_downstream(
    spec: &SopSpec,
    cp: &mut SopCheckpoint,
    target: &str,
    extra_origin: Option<&str>,
) {
    let mut ids: HashSet<String> = spec.transitive_dependents(target).into_iter().collect();
    if let Some(origin) = extra_origin {
        ids.extend(spec.transitive_dependents(origin));
    }
    for id in ids {
        if let Some(rec) = cp.stage_states.get_mut(&id) {
            rec.status = StageStatus::Pending;
            rec.summary = None;
            rec.artifacts.clear();
            rec.started_at = None;
            rec.finished_at = None;
        }
    }
}

fn persist_if_configured(data_dir: Option<&Path>, cp: &SopCheckpoint) -> Result<(), SopError> {
    if let Some(dir) = data_dir {
        save_checkpoint(dir, cp)?;
    }
    Ok(())
}

/// Approves the currently `WaitingHuman` stage, transitions the run back to `Running`,
/// and returns the approved stage id.
pub fn approve_human_gate(
    cp: &mut SopCheckpoint,
    reviewer: &str,
    clock: &dyn Clock,
) -> Result<String, SopError> {
    let reviewer = reviewer.trim();
    if reviewer.is_empty() {
        return Err(SopError::InvalidInput(
            "reviewer must not be empty".to_string(),
        ));
    }
    let waiting_id = cp
        .stage_states
        .iter()
        .find(|(_, rec)| rec.status == StageStatus::WaitingHuman)
        .map(|(id, _)| id.clone())
        .ok_or_else(|| SopError::NotWaitingHuman(cp.run_id.clone()))?;

    let now = clock.now();
    if let Some(rec) = cp.stage_states.get_mut(&waiting_id) {
        rec.status = StageStatus::Passed;
        rec.summary = Some(format!("approved by {reviewer}"));
        rec.finished_at = Some(now);
    }
    cp.status = SopRunStatus::Running;
    cp.push_event(
        now,
        SopEventKind::HumanApproved,
        Some(waiting_id.clone()),
        format!("human gate '{waiting_id}' approved by {reviewer}"),
    );
    Ok(waiting_id)
}

/// Sends an SOP run back to `to_stage`, resetting `to_stage` and all of its transitive
/// dependents to `Pending`, incrementing `rework_count`, and marking the run `Running`.
pub fn send_back(
    spec: &SopSpec,
    cp: &mut SopCheckpoint,
    to_stage: &str,
    reason: &str,
    clock: &dyn Clock,
) -> Result<(), SopError> {
    spec.validate()?;
    if spec.stage(to_stage).is_none() {
        return Err(SopError::UnknownStage(to_stage.to_string()));
    }
    let reason = reason.trim();
    if reason.is_empty() {
        return Err(SopError::InvalidInput(
            "send-back reason must not be empty".to_string(),
        ));
    }
    reset_downstream(spec, cp, to_stage, None);
    cp.rework_count = cp.rework_count.saturating_add(1);
    cp.status = SopRunStatus::Running;
    let now = clock.now();
    cp.push_event(
        now,
        SopEventKind::SendBack,
        Some(to_stage.to_string()),
        format!("sent back to '{to_stage}': {reason}"),
    );
    Ok(())
}

/// Computes the validated checkpoint file path `<data_dir>/sops/<run_id>.json`.
pub fn checkpoint_path(data_dir: &Path, run_id: &str) -> Result<PathBuf, SopError> {
    validate_run_id(run_id)?;
    Ok(data_dir.join(SOPS_DIR_NAME).join(format!("{run_id}.json")))
}

/// Persists `cp` atomically to `<data_dir>/sops/<run_id>.json` (`0700` dir, `0600` file),
/// refusing invalid `run_id`s and symlinks.
pub fn save_checkpoint(data_dir: &Path, cp: &SopCheckpoint) -> Result<PathBuf, SopError> {
    validate_run_id(&cp.run_id)?;
    let sops_dir = data_dir.join(SOPS_DIR_NAME);
    ensure_real_dir_mode(&sops_dir, Some(0o700))?;
    let dest = sops_dir.join(format!("{}.json", cp.run_id));
    refuse_symlink_or_non_file(&dest)?;
    let bytes = serde_json::to_vec_pretty(cp)
        .map_err(|e| SopError::Parse(format!("serialize checkpoint: {e}")))?;
    atomic_write_bytes(&sops_dir, &dest, &bytes, Some(0o600))?;
    Ok(dest)
}

/// Loads a checkpoint from `<data_dir>/sops/<run_id>.json`, refusing invalid `run_id`s and symlinks.
pub fn load_checkpoint(data_dir: &Path, run_id: &str) -> Result<SopCheckpoint, SopError> {
    validate_run_id(run_id)?;
    let sops_dir = data_dir.join(SOPS_DIR_NAME);
    if !check_existing_real_dir(&sops_dir)? {
        return Err(SopError::NotFound(format!("checkpoint {run_id:?}")));
    }
    let path = sops_dir.join(format!("{run_id}.json"));
    let cp = read_checkpoint_file(&path)?;
    if cp.run_id != run_id {
        return Err(SopError::Parse(format!(
            "checkpoint file {} has mismatched run_id {:?}",
            path.display(),
            cp.run_id
        )));
    }
    Ok(cp)
}

/// Lists all saved checkpoints under `<data_dir>/sops/`, newest `updated_at` first.
pub fn list_checkpoints(data_dir: &Path) -> Result<Vec<SopCheckpoint>, SopError> {
    let sops_dir = data_dir.join(SOPS_DIR_NAME);
    if !check_existing_real_dir(&sops_dir)? {
        return Ok(Vec::new());
    }
    let entries = fs::read_dir(&sops_dir)
        .map_err(|e| SopError::Io(format!("read {}: {e}", sops_dir.display())))?;
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| SopError::Io(format!("dir entry: {e}")))?;
        let path = entry.path();
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if name_str.starts_with('.') || !name_str.ends_with(".json") {
            continue;
        }
        out.push(read_checkpoint_file(&path)?);
    }
    out.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| a.run_id.cmp(&b.run_id))
    });
    Ok(out)
}

fn read_checkpoint_file(path: &Path) -> Result<SopCheckpoint, SopError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(SopError::NotFound(format!("checkpoint {}", path.display())));
        }
        Err(e) => return Err(SopError::Io(format!("stat {}: {e}", path.display()))),
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(SopError::InvalidPath(format!(
            "{} is a symlink or not a regular file",
            path.display()
        )));
    }
    let file =
        fs::File::open(path).map_err(|e| SopError::Io(format!("open {}: {e}", path.display())))?;
    let mut buf = Vec::new();
    file.take(MAX_CHECKPOINT_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| SopError::Io(format!("read {}: {e}", path.display())))?;
    if u64::try_from(buf.len()).unwrap_or(u64::MAX) > MAX_CHECKPOINT_BYTES {
        return Err(SopError::InvalidPath(format!(
            "checkpoint {} exceeds {} bytes",
            path.display(),
            MAX_CHECKPOINT_BYTES
        )));
    }
    serde_json::from_slice(&buf)
        .map_err(|e| SopError::Parse(format!("decode {}: {e}", path.display())))
}
