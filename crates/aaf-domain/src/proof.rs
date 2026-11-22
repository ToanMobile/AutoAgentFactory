//! Signed device-verification proof artifact bound to a repository tree fingerprint.

use serde::{Deserialize, Serialize};

pub const PROOF_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofRecord {
    pub schema_version: u32,
    pub repo_root: String,
    pub tree_fp: String,
    pub image_path: String,
    pub image_sha256: String,
    pub width: u32,
    pub height: u32,
    pub byte_len: u64,
    pub source_tool: String,
    pub device_serial: String,
    pub task_id: Option<String>,
    pub recorded_at: i64,
}
