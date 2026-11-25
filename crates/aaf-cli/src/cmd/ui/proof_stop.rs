//! Proof PNG inspection/verification and safe run termination for the Web UI.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use aaf_domain::ports::TaskStore;
use aaf_domain::proof::ProofRecord;
use aaf_domain::run::RunStatus;
use aaf_domain::task::Task;
use aaf_engine::db::Db;
use aaf_engine::executors::reaper;
use aaf_engine::proof::show_proof;
use serde_json::{Value, json};

use super::extras::{canonical_under_allowed_roots, resolve_task_worktree};
use crate::ctx::Ctx;

const MAX_PROOF_PNG_BYTES: u64 = 10 * 1024 * 1024;
const PNG_MAGIC: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

fn allowed_proof_roots(ctx: &Ctx, task: &Task) -> Vec<PathBuf> {
    vec![
        ctx.data_dir.join("proofs"),
        ctx.worktrees_dir.clone(),
        PathBuf::from(&task.repo_root),
    ]
}

fn find_proof_record(db: &Db, ctx: &Ctx, task: &Task) -> Option<ProofRecord> {
    let wt_roots = vec![ctx.worktrees_dir.clone(), PathBuf::from(&task.repo_root)];
    if let Some((wt, _)) = resolve_task_worktree(db, ctx, task)
        && let Some(canon_wt) = canonical_under_allowed_roots(&wt, &wt_roots)
        && let Ok(rec) = show_proof(&canon_wt, &ctx.data_dir, None)
    {
        return Some(rec);
    }
    let repo = Path::new(&task.repo_root);
    if repo.is_dir()
        && let Ok(rec) = show_proof(repo, &ctx.data_dir, None)
        && rec.task_id.as_deref() == Some(task.id.as_str())
    {
        return Some(rec);
    }
    None
}

fn resolve_verified_image_path(ctx: &Ctx, task: &Task, raw_path: &str) -> Result<PathBuf, String> {
    let img_path = Path::new(raw_path);
    let meta = fs::symlink_metadata(img_path)
        .map_err(|_| "not found: proof image file missing".to_string())?;
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return Err("refused: proof image path is not a regular file".into());
    }
    if meta.len() > MAX_PROOF_PNG_BYTES {
        return Err("refused: proof image exceeds 10 MiB limit".into());
    }
    let allowed = allowed_proof_roots(ctx, task);
    canonical_under_allowed_roots(img_path, &allowed)
        .ok_or_else(|| "refused: proof image path outside allowed roots".to_string())
}

/// Inspect recorded proof metadata & image endpoints for a task.
pub(super) fn read_task_proof(db: &Db, ctx: &Ctx, task_id: &str) -> Result<Value, String> {
    let task = db.get_task(task_id).map_err(|e| e.to_string())?;
    let Some(rec) = find_proof_record(db, ctx, &task) else {
        return Ok(json!({ "available": false, "images": [] }));
    };
    let name = Path::new(&rec.image_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("proof.png")
        .to_string();
    let images = if resolve_verified_image_path(ctx, &task, &rec.image_path).is_ok() {
        vec![json!({
            "idx": 0,
            "name": name,
            "width": rec.width,
            "height": rec.height,
            "url": format!("/api/proof-image/{}/0", task.id),
        })]
    } else {
        Vec::new()
    };
    Ok(json!({
        "available": true,
        "record": {
            "schema_version": rec.schema_version,
            "tree_fp": rec.tree_fp,
            "width": rec.width,
            "height": rec.height,
            "byte_len": rec.byte_len,
            "source_tool": rec.source_tool,
            "device_serial": rec.device_serial,
            "task_id": rec.task_id,
            "recorded_at": rec.recorded_at,
            "flows": rec.flows,
        },
        "images": images,
    }))
}

/// Read verified PNG bytes for proof image `idx` of `task_id`.
pub(super) fn read_task_proof_image(
    db: &Db,
    ctx: &Ctx,
    task_id: &str,
    idx: usize,
) -> Result<Vec<u8>, String> {
    if idx != 0 {
        return Err("not found: proof image index out of range".into());
    }
    let task = db.get_task(task_id).map_err(|e| e.to_string())?;
    let rec = find_proof_record(db, ctx, &task)
        .ok_or_else(|| "not found: no proof recorded for task".to_string())?;
    let canon_path = resolve_verified_image_path(ctx, &task, &rec.image_path)?;
    let file = File::open(&canon_path).map_err(|e| format!("failed to read proof image: {e}"))?;
    let file_meta = file
        .metadata()
        .map_err(|e| format!("failed to stat proof image: {e}"))?;
    if !file_meta.is_file() {
        return Err("refused: proof image is not a regular file".into());
    }
    let mut buf =
        Vec::with_capacity(usize::try_from(file_meta.len().min(MAX_PROOF_PNG_BYTES)).unwrap_or(0));
    file.take(MAX_PROOF_PNG_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("failed to read proof image: {e}"))?;
    if buf.len() as u64 > MAX_PROOF_PNG_BYTES {
        return Err("refused: proof image exceeds 10 MiB limit".into());
    }
    if buf.len() < PNG_MAGIC.len() || &buf[..PNG_MAGIC.len()] != PNG_MAGIC {
        return Err("refused: proof image is not a valid PNG".into());
    }
    if aaf_fp::sha256_hex(&buf) != rec.image_sha256 {
        return Err("refused: proof image sha256 mismatch".into());
    }
    Ok(buf)
}

/// Safely terminate verified processes carrying `AAF_RUN_ID=<run_id>` via native reaper.
pub(super) fn do_stop_run(db: &Db, ctx: &Ctx, run_id: &str) -> Result<Value, String> {
    let running = db.running_runs(None).map_err(|e| e.to_string())?;
    let run = running
        .into_iter()
        .find(|r| r.id == run_id && r.status == RunStatus::Running)
        .ok_or_else(|| format!("not found: active running run '{run_id}'"))?;
    let task = db.get_task(&run.task_id).map_err(|e| e.to_string())?;
    let wt = PathBuf::from(&run.worktree_path);
    let allowed = vec![ctx.worktrees_dir.clone(), PathBuf::from(&task.repo_root)];
    if canonical_under_allowed_roots(&wt, &allowed).is_none() {
        return Err("refused: worktree outside allowed roots".into());
    }
    let signaled = reaper::reap(run_id).map_err(|e| format!("reaper failed: {e}"))?;
    if signaled.is_empty() {
        return Err("not found: no live agent process found for that run".into());
    }
    Ok(json!({
        "action": "stop_run",
        "run_id": run_id,
        "signaled_pids": signaled,
    }))
}
