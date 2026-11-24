//! Signed per-reviewer partial audit records (`audit_partial\0`) and per-repo audit cleanup.

use std::path::{Path, PathBuf};
use std::{fs, io};

use aaf_domain::gate::GoldenScore;
use aaf_domain::pipeline::ReviewIssue;
use serde::{Deserialize, Serialize};

use crate::signed::{read_signed, write_signed};
use crate::{FpError, repo_id, sha256_hex};

/// Domain tag of a signed per-reviewer partial audit verdict.
pub const AUDIT_PARTIAL_DOMAIN: &str = "audit_partial\0";

/// Signed per-reviewer partial Dual-Review verdict cached between blind sibling runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartialAuditRecord {
    pub repo_root: String,
    pub tree_fp: String,
    pub base_sha: String,
    pub config_hash: String,
    pub plan_contract_hash: String,
    pub reviewer_id: String,
    pub executor: String,
    pub canonical_provider: String,
    pub model: String,
    pub effort: String,
    pub verdict: String,
    pub p0: u32,
    pub p1: u32,
    pub p2: u32,
    pub mechanical_score: u8,
    #[serde(default)]
    pub llm_average: Option<u8>,
    pub effective_score: u8,
    #[serde(default)]
    pub scores: Vec<GoldenScore>,
    #[serde(default)]
    pub issues: Vec<ReviewIssue>,
    pub created_at: i64,
}

/// Exact binding key verified by [`read_partial_audit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartialAuditKey<'a> {
    pub repo_root: &'a Path,
    pub tree_fp: &'a str,
    pub base_sha: &'a str,
    pub config_hash: &'a str,
    pub plan_contract_hash: &'a str,
    pub reviewer_id: &'a str,
    pub executor: &'a str,
    pub model: &'a str,
    pub effort: &'a str,
}

/// Content-addressed path for a signed partial audit record under `data_dir/audits/<repo_id>/`.
///
/// The filename is strictly derived from SHA-256 over all binding dimensions so
/// `reviewer_id` can never traverse directories or collide across base/plan/config changes.
pub fn partial_audit_path(
    data_dir: &Path,
    repo_root: &Path,
    tree_fp: &str,
    base_sha: &str,
    config_hash: &str,
    plan_contract_hash: &str,
    reviewer_id: &str,
) -> PathBuf {
    let tag_input =
        format!("{tree_fp}\0{base_sha}\0{config_hash}\0{plan_contract_hash}\0{reviewer_id}");
    let hash = sha256_hex(tag_input.as_bytes());
    data_dir
        .join("audits")
        .join(repo_id(repo_root))
        .join(format!("{hash}.partial.json"))
}

/// Signs and stores `record` under [`AUDIT_PARTIAL_DOMAIN`] with mode 0600.
pub fn write_partial_audit(
    data_dir: &Path,
    key: &[u8],
    repo_root: &Path,
    record: &PartialAuditRecord,
) -> Result<PathBuf, FpError> {
    let path = partial_audit_path(
        data_dir,
        repo_root,
        &record.tree_fp,
        &record.base_sha,
        &record.config_hash,
        &record.plan_contract_hash,
        &record.reviewer_id,
    );
    write_signed(data_dir, &path, key, AUDIT_PARTIAL_DOMAIN, record)?;
    Ok(path)
}

/// Reads and verifies a signed partial audit record against every field of `expected`.
///
/// Returns `Ok(None)` when no partial file exists; tampered signatures or any
/// binding mismatch fail closed with [`FpError::BadSignature`].
pub fn read_partial_audit(
    data_dir: &Path,
    key: &[u8],
    expected: &PartialAuditKey<'_>,
) -> Result<Option<PartialAuditRecord>, FpError> {
    let path = partial_audit_path(
        data_dir,
        expected.repo_root,
        expected.tree_fp,
        expected.base_sha,
        expected.config_hash,
        expected.plan_contract_hash,
        expected.reviewer_id,
    );
    if !path.exists() {
        return Ok(None);
    }
    let rec: PartialAuditRecord = read_signed(&path, key, AUDIT_PARTIAL_DOMAIN)?;
    let repo_matches = if expected.repo_root.exists() {
        match (
            fs::canonicalize(expected.repo_root),
            fs::canonicalize(Path::new(&rec.repo_root)),
        ) {
            (Ok(exp), Ok(actual)) => exp == actual,
            _ => false,
        }
    } else {
        Path::new(&rec.repo_root) == expected.repo_root
    };
    if !repo_matches
        || rec.tree_fp != expected.tree_fp
        || rec.base_sha != expected.base_sha
        || rec.config_hash != expected.config_hash
        || rec.plan_contract_hash != expected.plan_contract_hash
        || rec.reviewer_id != expected.reviewer_id
        || rec.executor != expected.executor
        || rec.model != expected.model
        || rec.effort != expected.effort
    {
        return Err(FpError::BadSignature);
    }
    Ok(Some(rec))
}

/// Removes all audit files (final and partial) stored in `data_dir/audits/<repo_id>/`.
///
/// Returns the number of files removed (0 if the directory does not exist) and
/// removes the directory itself if it becomes empty.
pub fn clear_audits_for_repo(data_dir: &Path, repo_root: &Path) -> Result<usize, FpError> {
    let dir = data_dir.join("audits").join(repo_id(repo_root));
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err.into()),
    };
    let mut removed = 0usize;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err.into()),
        };
        if !meta.is_dir() {
            fs::remove_file(&path)?;
            removed += 1;
        }
    }
    // Best-effort cleanup of the now-empty repo audit directory.
    let _ = fs::remove_dir(&dir);
    Ok(removed)
}
