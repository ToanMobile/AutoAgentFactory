//! Signed device-verification proof artifact bound to a repository tree fingerprint.

use serde::{Deserialize, Serialize};

pub const PROOF_SCHEMA_VERSION: u32 = 1;
pub const PROOF_SCHEMA_VERSION_V2: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssertionResult {
    pub name: String,
    pub passed: bool,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowResult {
    pub name: String,
    pub passed: bool,
    #[serde(default)]
    pub needs_human: bool,
    #[serde(default)]
    pub assertions: Vec<AssertionResult>,
    #[serde(default)]
    pub duration_ms: u64,
}

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flows: Vec<FlowResult>,
}
