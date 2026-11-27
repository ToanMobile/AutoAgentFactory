//! Proof PNG inspection/verification and safe run termination for the Web UI.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use aaf_domain::event::EventKind;
use aaf_domain::ports::{EventSink, RunStore, TaskStore};
use aaf_domain::proof::ProofRecord;
use aaf_domain::run::RunStatus;
use aaf_domain::task::{Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::executors::reaper;
use aaf_engine::proof::{show_proof, show_proof_for_archived_root};
use serde_json::{Value, json};

use super::extras::{
    allowed_task_worktree_roots, canonical_under_allowed_roots, resolve_task_worktree,
};
use crate::ctx::Ctx;

const MAX_PROOF_PNG_BYTES: u64 = 10 * 1024 * 1024;
const MAX_PROOF_SCAN_FILES: usize = 256;
const MAX_PROOF_JSON_BYTES: u64 = 512 * 1024;
const PNG_MAGIC: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

fn allowed_proof_roots(ctx: &Ctx, task: &Task) -> Vec<PathBuf> {
    let mut roots = vec![ctx.data_dir.join("proofs")];
    roots.extend(allowed_task_worktree_roots(ctx, task));
    roots
}

fn archived_root_allowed(path: &Path, ctx: &Ctx, task: &Task) -> bool {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return false;
    }
    let allowed = allowed_task_worktree_roots(ctx, task);
    allowed.iter().any(|root| {
        path == root
            || path.starts_with(root)
            || fs::canonicalize(root)
                .map(|cr| path == cr || path.starts_with(&cr))
                .unwrap_or(false)
    })
}

fn collect_event_proof_candidates(
    db: &Db,
    ctx: &Ctx,
    task: &Task,
) -> (Vec<(String, Option<PathBuf>)>, bool) {
    let kinds = [
        EventKind::ProofRecorded,
        EventKind::GateResult,
        EventKind::CrossAudit,
    ];
    let events = db
        .events_for_task_by_kinds(&task.id, &kinds, 32)
        .unwrap_or_default();
    let mut had_proof_event = false;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for ev in &events {
        if ev.kind == EventKind::ProofRecorded {
            had_proof_event = true;
        }
        let Ok(val) = serde_json::from_str::<Value>(&ev.payload_json) else {
            continue;
        };
        let Some(fp) = val
            .get("tree_fp")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let gated_tree = val
            .get("gated_tree")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .filter(|p| archived_root_allowed(p, ctx, task));
        let key = (fp.to_string(), gated_tree.clone());
        if seen.insert(key.clone()) {
            out.push(key);
        }
    }
    (out, had_proof_event)
}

fn scan_repo_proofs_for_task(ctx: &Ctx, repo: &Path, task_id: &str) -> Option<ProofRecord> {
    let canon_repo = fs::canonicalize(repo).ok()?;
    let proof_dir = ctx
        .data_dir
        .join("proofs")
        .join(aaf_fp::repo_id(&canon_repo));
    let Ok(entries) = fs::read_dir(&proof_dir) else {
        return None;
    };
    let mut best: Option<ProofRecord> = None;
    for entry in entries.flatten().take(MAX_PROOF_SCAN_FILES) {
        let path = entry.path();
        if !path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".proof.json"))
        {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > MAX_PROOF_JSON_BYTES {
            continue;
        }
        let Ok(file) = File::open(&path) else {
            continue;
        };
        let mut buf = Vec::new();
        if file
            .take(MAX_PROOF_JSON_BYTES + 1)
            .read_to_end(&mut buf)
            .is_err()
            || buf.len() as u64 > MAX_PROOF_JSON_BYTES
        {
            continue;
        }
        let Ok(signed) = serde_json::from_slice::<aaf_fp::Signed>(&buf) else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<ProofRecord>(&signed.payload) else {
            continue;
        };
        if parsed.task_id.as_deref() != Some(task_id) {
            continue;
        }
        // Re-verify HMAC + binding through the canonical proof loader.
        let Ok(verified) = show_proof(&canon_repo, &ctx.data_dir, Some(&parsed.tree_fp)) else {
            continue;
        };
        if verified.task_id.as_deref() == Some(task_id)
            && best
                .as_ref()
                .is_none_or(|b| verified.recorded_at > b.recorded_at)
        {
            best = Some(verified);
        }
    }
    best
}

fn find_proof_record(db: &Db, ctx: &Ctx, task: &Task) -> Option<ProofRecord> {
    let mut fallback: Option<ProofRecord> = None;
    let mut accept = |rec: ProofRecord| -> Option<ProofRecord> {
        if resolve_verified_image_path(ctx, task, &rec.image_path).is_ok() {
            return Some(rec);
        }
        if fallback.is_none() {
            fallback = Some(rec);
        }
        None
    };

    let wt_roots = allowed_task_worktree_roots(ctx, task);
    if let Some((wt, _)) = resolve_task_worktree(db, ctx, task)
        && let Some(canon_wt) = canonical_under_allowed_roots(&wt, &wt_roots)
        && let Ok(rec) = show_proof(&canon_wt, &ctx.data_dir, None)
        && let Some(found) = accept(rec)
    {
        return Some(found);
    }

    let repo = Path::new(&task.repo_root);
    let (candidates, _) = collect_event_proof_candidates(db, ctx, task);
    if !candidates.is_empty() {
        let mut run_roots: Vec<PathBuf> = db
            .runs_for_task(&task.id)
            .unwrap_or_default()
            .into_iter()
            .rev()
            .filter_map(|r| {
                let p = PathBuf::from(r.worktree_path);
                archived_root_allowed(&p, ctx, task).then_some(p)
            })
            .collect();
        run_roots.dedup();

        for (fp, gated_tree) in &candidates {
            // Host proofs are mirrored under the common repo root for the exact tree_fp.
            if repo.is_dir()
                && let Ok(rec) = show_proof(repo, &ctx.data_dir, Some(fp))
                && let Some(found) = accept(rec)
            {
                return Some(found);
            }
            if let Some(gt) = gated_tree
                && let Ok(rec) = show_proof_for_archived_root(gt, &ctx.data_dir, fp)
                && let Some(found) = accept(rec)
            {
                return Some(found);
            }
            for rr in &run_roots {
                if gated_tree.as_ref().is_some_and(|gt| gt == rr) {
                    continue;
                }
                if let Ok(rec) = show_proof_for_archived_root(rr, &ctx.data_dir, fp)
                    && let Some(found) = accept(rec)
                {
                    return Some(found);
                }
            }
        }
    }

    if repo.is_dir() {
        if let Ok(rec) = show_proof(repo, &ctx.data_dir, None)
            && rec.task_id.as_deref() == Some(task.id.as_str())
            && let Some(found) = accept(rec)
        {
            return Some(found);
        }
        if let Some(rec) = scan_repo_proofs_for_task(ctx, repo, &task.id)
            && let Some(found) = accept(rec)
        {
            return Some(found);
        }
    }
    fallback
}

fn missing_proof_reason(db: &Db, ctx: &Ctx, task: &Task) -> (&'static str, bool) {
    let (_, had_proof_event) = collect_event_proof_candidates(db, ctx, task);
    let proof_required = ctx
        .load_config(Path::new(&task.repo_root))
        .map(|c| c.repo.proof.required)
        .unwrap_or(false);
    if had_proof_event {
        return ("expired_or_cleaned", proof_required);
    }
    let has_runs = !db.runs_for_task(&task.id).unwrap_or_default().is_empty();
    if !has_runs {
        return ("no_runs", proof_required);
    }
    if task.state == TaskState::Done {
        return ("not_captured_when_done", proof_required);
    }
    ("not_recorded", proof_required)
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
        let (reason, proof_required) = missing_proof_reason(db, ctx, &task);
        return Ok(json!({
            "available": false,
            "reason": reason,
            "proof_required": proof_required,
            "images": [],
        }));
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
    let allowed = allowed_task_worktree_roots(ctx, &task);
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
