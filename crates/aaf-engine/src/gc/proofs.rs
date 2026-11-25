//! Paired `.png` + `.proof.json` retention under `data_dir/proofs/<repo_id>/`.
//!
//! `.png` files use `<safe_tree_fp>.png` while signed `.proof.json` files use
//! `<sha256(tree_fp)[..24]>.proof.json` (distinct stems), and host captures
//! mirror both into `proofs/<common_repo_id>/`. This module removes expired
//! pairs atomically per directory so neither file is left orphaned, while
//! protecting proofs referenced by active tasks or `receipts/*/latest.json`.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use aaf_domain::gate::Receipt;
use aaf_domain::proof::ProofRecord;
use aaf_fp::{RECEIPT_DOMAIN, Signed, verify_tagged};
use serde_json::Value;

use super::expired::{is_older_than, sorted_real_dir_entries};
use crate::db::Db;

const MAX_METADATA_JSON_BYTES: u64 = 4 * 1024 * 1024;

/// Summary of an expired-proof sweep under `data_dir/proofs/`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcProofStats {
    pub removed_files: Vec<String>,
    /// Number of proof files (`.proof.json` and `.png`) removed (or would be removed in dry-run).
    pub proofs_removed: usize,
    pub warnings: Vec<String>,
    pub skipped_locked: bool,
}

/// Computes the `<safe_tree_fp>.png` filename matching `proof_capture::captured_proof_image_path`.
fn proof_png_filename(tree_fp: &str) -> String {
    let safe_fp: String = tree_fp
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let stem = if safe_fp.is_empty() {
        aaf_fp::sha256_hex(tree_fp.as_bytes())
    } else {
        safe_fp
    };
    format!("{stem}.png")
}

fn read_bounded_regular(path: &Path) -> Option<Vec<u8>> {
    let meta = fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > MAX_METADATA_JSON_BYTES {
        return None;
    }
    let file = File::open(path).ok()?;
    let mut buf = Vec::new();
    file.take(MAX_METADATA_JSON_BYTES + 1)
        .read_to_end(&mut buf)
        .ok()?;
    if u64::try_from(buf.len()).unwrap_or(u64::MAX) > MAX_METADATA_JSON_BYTES {
        return None;
    }
    Some(buf)
}

fn extract_receipt_tree_fp(path: &Path, key: Option<&[u8]>) -> Option<String> {
    let bytes = read_bounded_regular(path)?;
    if let Ok(signed) = serde_json::from_slice::<Signed>(&bytes) {
        if let Some(k) = key
            && !verify_tagged(k, RECEIPT_DOMAIN, &signed)
        {
            return None;
        }
        if let Ok(receipt) = serde_json::from_str::<Receipt>(&signed.payload) {
            let trimmed = receipt.tree_fp.trim();
            return (!trimmed.is_empty()).then(|| trimmed.to_string());
        }
        if let Ok(val) = serde_json::from_str::<Value>(&signed.payload)
            && let Some(fp) = val.get("tree_fp").and_then(Value::as_str)
        {
            let trimmed = fp.trim();
            return (!trimmed.is_empty()).then(|| trimmed.to_string());
        }
        return None;
    }
    let val = serde_json::from_slice::<Value>(&bytes).ok()?;
    let fp = val.get("tree_fp").and_then(Value::as_str)?.trim();
    (!fp.is_empty()).then(|| fp.to_string())
}

fn read_proof_record(path: &Path) -> Option<ProofRecord> {
    let bytes = read_bounded_regular(path)?;
    let signed: Signed = serde_json::from_slice(&bytes).ok()?;
    serde_json::from_str::<ProofRecord>(&signed.payload).ok()
}

/// Collects protected `tree_fp` values from all `data_dir/receipts/*/latest.json`
/// files and (when available) non-terminal task events in `db`.
pub fn collect_protected_tree_fps(data_dir: &Path, db: Option<&Db>) -> HashSet<String> {
    let mut protected = HashSet::new();
    let key = aaf_fp::load_key(data_dir).ok();
    for repo_dir in sorted_real_dir_entries(&data_dir.join("receipts")) {
        if !fs::symlink_metadata(&repo_dir).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
        {
            continue;
        }
        let latest = repo_dir.join("latest.json");
        if let Some(fp) = extract_receipt_tree_fp(&latest, key.as_deref()) {
            protected.insert(fp);
        }
    }
    if let Some(db) = db
        && let Ok(active_fps) = db.active_task_tree_fps()
    {
        protected.extend(active_fps);
    }
    protected
}

fn same_dir(candidate_parent: &Path, id_dir: &Path, canon_id_dir: Option<&Path>) -> bool {
    if candidate_parent == id_dir {
        return true;
    }
    if let (Some(canon_dir), Ok(canon_parent)) = (canon_id_dir, fs::canonicalize(candidate_parent))
    {
        return canon_parent == canon_dir;
    }
    false
}

fn find_paired_pngs(
    id_dir: &Path,
    canon_id_dir: Option<&Path>,
    record: &ProofRecord,
) -> Vec<(PathBuf, fs::Metadata)> {
    let mut candidates = Vec::new();
    let expected = id_dir.join(proof_png_filename(&record.tree_fp));
    candidates.push(expected);

    let img_path = Path::new(&record.image_path);
    if img_path.extension().and_then(|e| e.to_str()) == Some("png")
        && let (Some(parent), Some(name)) = (img_path.parent(), img_path.file_name())
        && same_dir(parent, id_dir, canon_id_dir)
    {
        let local = id_dir.join(name);
        if !candidates.contains(&local) {
            candidates.push(local);
        }
    }

    let mut found = Vec::new();
    for path in candidates {
        if let Ok(meta) = fs::symlink_metadata(&path)
            && meta.is_file()
            && !meta.file_type().is_symlink()
        {
            found.push((path, meta));
        }
    }
    found
}

fn record_removal(path: &Path, dry_run: bool, stats: &mut GcProofStats) -> bool {
    if !dry_run && let Err(err) = fs::remove_file(path) {
        stats.warnings.push(format!("{}: {err}", path.display()));
        return false;
    }
    stats.removed_files.push(path.display().to_string());
    stats.proofs_removed += 1;
    true
}

/// Sweeps expired paired `.proof.json` + `.png` files under `data_dir/proofs/<id>/`.
///
/// Never follows symlinks, never touches files outside `data_dir/proofs/<id>/`,
/// protects `protected_tree_fps` and `active_task_ids`, and skips completely
/// when `older_than_days == 0 && any_run_lock_held(data_dir)`.
pub fn clean_expired_proofs(
    data_dir: &Path,
    now: SystemTime,
    older_than_days: u64,
    protected_tree_fps: &HashSet<String>,
    active_task_ids: &HashSet<String>,
    dry_run: bool,
) -> GcProofStats {
    let mut stats = GcProofStats::default();
    if older_than_days == 0 && crate::gc_audits::any_run_lock_held(data_dir) {
        stats.skipped_locked = true;
        return stats;
    }

    let max_age = Duration::from_secs(older_than_days.saturating_mul(86_400));
    let protected_png_names: HashSet<String> = protected_tree_fps
        .iter()
        .map(|fp| proof_png_filename(fp))
        .collect();

    for id_dir in sorted_real_dir_entries(&data_dir.join("proofs")) {
        if !fs::symlink_metadata(&id_dir).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink()) {
            continue;
        }
        let canon_id_dir = fs::canonicalize(&id_dir).ok();
        let mut proof_jsons: Vec<(PathBuf, fs::Metadata)> = Vec::new();
        let mut pngs: Vec<(PathBuf, fs::Metadata)> = Vec::new();

        for path in sorted_real_dir_entries(&id_dir) {
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !meta.is_file() || meta.file_type().is_symlink() {
                continue;
            }
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.ends_with(".proof.json") {
                proof_jsons.push((path, meta));
            } else if name.ends_with(".png") {
                pngs.push((path, meta));
            }
        }

        let mut handled_pngs: HashSet<PathBuf> = HashSet::new();
        let mut protected_pngs: HashSet<PathBuf> = HashSet::new();
        let mut removed_any = false;

        // 1a. Classify every `.proof.json` and populate `protected_pngs` first so
        // an expired proof processed earlier cannot remove a `.png` still shared
        // with a protected or non-expired proof in the same directory.
        let mut expired_pairs: Vec<(PathBuf, Vec<PathBuf>)> = Vec::new();
        for (json_path, json_meta) in proof_jsons {
            let record = read_proof_record(&json_path);
            let paired_pngs = record
                .as_ref()
                .map(|r| find_paired_pngs(&id_dir, canon_id_dir.as_deref(), r))
                .unwrap_or_default();

            let is_protected = record.as_ref().is_some_and(|r| {
                protected_tree_fps.contains(&r.tree_fp)
                    || r.task_id
                        .as_deref()
                        .is_some_and(|tid| active_task_ids.contains(tid))
            });
            let pair_expired = !is_protected
                && is_older_than(&json_meta, max_age, now)
                && paired_pngs
                    .iter()
                    .all(|(_, m)| is_older_than(m, max_age, now));
            if !pair_expired {
                for (png, _) in paired_pngs {
                    protected_pngs.insert(png.clone());
                    handled_pngs.insert(png);
                }
                continue;
            }
            expired_pairs.push((
                json_path,
                paired_pngs.into_iter().map(|(png, _)| png).collect(),
            ));
        }

        // 1b. Remove `.proof.json` first so a partial I/O failure never leaves a
        // dangling proof record pointing at a deleted screenshot, and never delete
        // a paired `.png` that is protected.
        for (json_path, paired_pngs) in expired_pairs {
            if record_removal(&json_path, dry_run, &mut stats) {
                removed_any = true;
                for png in paired_pngs {
                    let name = png.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if protected_pngs.contains(&png)
                        || protected_png_names.contains(name)
                        || !handled_pngs.insert(png.clone())
                    {
                        continue;
                    }
                    if record_removal(&png, dry_run, &mut stats) {
                        removed_any = true;
                    }
                }
            }
        }

        // 2. Clean orphan `.png` files in the same dir whose `.proof.json` is already gone.
        for (png_path, png_meta) in pngs {
            if handled_pngs.contains(&png_path) || protected_pngs.contains(&png_path) {
                continue;
            }
            let name = png_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if protected_png_names.contains(name) {
                continue;
            }
            if is_older_than(&png_meta, max_age, now)
                && record_removal(&png_path, dry_run, &mut stats)
            {
                removed_any = true;
            }
        }

        if !dry_run && removed_any {
            let _ = fs::remove_dir(&id_dir);
        }
    }

    stats
}
