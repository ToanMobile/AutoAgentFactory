//! Read-only handover loader bound to `Db::open_read_only` and `aaf_fp::load_key`.

use std::path::{Path, PathBuf};

use aaf_domain::config::RepoConfig;
use aaf_domain::event::EventKind;
use aaf_domain::ports::{EventSink, TaskStore};
use serde_json::Value;

use super::{
    CompactAudit, CompactGate, HandoverData, MAX_EVENTS_LOAD, build_handover_from_parts_with_dual,
    latest_event, non_empty, parse_event,
};
use crate::db::Db;

/// Builds a [`HandoverData`] snapshot from a strictly read-only [`Db`] handle
/// and `data_dir`. Never creates `receipt.key`.
pub fn load_handover(
    db: &Db,
    data_dir: &Path,
    task_id: &str,
    cfg: &RepoConfig,
) -> Result<HandoverData, String> {
    let task = db.get_task(task_id).map_err(|e| e.to_string())?;
    let mut events = db
        .events_for_task(task_id, MAX_EVENTS_LOAD)
        .map_err(|e| e.to_string())?;
    if !events.iter().any(|e| e.kind == EventKind::PlanConverged)
        && let Ok(mut converged) =
            db.events_for_task_by_kinds(task_id, &[EventKind::PlanConverged], 1)
    {
        events.append(&mut converged);
    }
    let cg: Option<CompactGate> = parse_event(&events, EventKind::GateResult);
    let ca: Option<CompactAudit> = parse_event(&events, EventKind::CrossAudit);
    let key = aaf_fp::load_key(data_dir).ok();

    let mut roots: Vec<PathBuf> = cg
        .as_ref()
        .and_then(|g| non_empty(&g.gated_tree))
        .map(PathBuf::from)
        .into_iter()
        .collect();
    let repo_path = PathBuf::from(&task.repo_root);
    if !roots.iter().any(|r| r == &repo_path) {
        roots.push(repo_path.clone());
    }

    let expected_fp = cg.as_ref().and_then(|g| non_empty(&g.tree_fp));
    let receipt = key.as_ref().and_then(|k| {
        roots.iter().find_map(|root| {
            let rec = aaf_fp::read_receipt(data_dir, k, root).ok()?;
            expected_fp
                .is_none_or(|fp| rec.tree_fp == fp)
                .then_some(rec)
        })
    });
    let proof_ev_fp = latest_event(&events, EventKind::ProofRecorded)
        .and_then(|e| serde_json::from_str::<Value>(&e.payload_json).ok())
        .and_then(|v| v.get("tree_fp").and_then(Value::as_str).map(str::to_string));
    let tree_fp = receipt
        .as_ref()
        .map(|r| r.tree_fp.clone())
        .or_else(|| {
            cg.as_ref()
                .and_then(|g| non_empty(&g.tree_fp))
                .map(str::to_string)
        })
        .or_else(|| {
            ca.as_ref()
                .and_then(|a| non_empty(&a.tree_fp))
                .map(str::to_string)
        })
        .or(proof_ev_fp);
    let cfg_hash = receipt
        .as_ref()
        .map(|r| r.config_hash.clone())
        .or_else(|| {
            cg.as_ref()
                .and_then(|g| non_empty(&g.config_hash))
                .map(str::to_string)
        })
        .or_else(|| {
            ca.as_ref()
                .and_then(|a| non_empty(&a.config_hash))
                .map(str::to_string)
        })
        .unwrap_or_else(|| crate::gate::config_hash(cfg));
    let audit = match (key.as_ref(), tree_fp.as_deref()) {
        (Some(k), Some(fp)) => roots.iter().find_map(|r| {
            aaf_fp::read_audit_verdict(data_dir, k, r, fp, &cfg_hash)
                .ok()
                .flatten()
        }),
        _ => None,
    };
    let proof = tree_fp.as_deref().and_then(|fp| {
        roots.iter().find_map(|r| {
            crate::proof::verify_proof_for_archived_root(
                r,
                &repo_path,
                data_dir,
                &cfg.proof,
                Some(fp),
            )
            .ok()
        })
    });
    let mut data = build_handover_from_parts_with_dual(
        &task,
        &events,
        receipt.as_ref(),
        audit.as_ref(),
        proof.as_ref(),
        cfg.proof.required,
        cfg.cross_audit.score_gate,
        cfg.cross_audit.min_average_score,
        cfg.cross_audit.min_criterion_score,
        cfg.cross_audit.require_different_provider,
        cfg.cross_audit.dual_review,
    );
    data.proof_config = cfg.proof.clone();
    Ok(data)
}
