//! Strict JSON schemas, repo-relative path normalization, canonical hashing,
//! and downstream stripping for Plan Debate artifacts.

use aaf_domain::pipeline::{MAX_PLAN_CONTRACT_BYTES, PlanContract, PlanCritique};
use aaf_domain::prompt_fence::PLAN_APPROVED_PREFIX;
pub use aaf_domain::prompt_fence::{normalize_repo_path, strip_for_downstream};

use super::PLAN_CONTRACT_HASH_TAG;

const MAX_APPROVED_SUMMARY_CHARS: usize = 240;
const SHA256_HEX_LEN: usize = 64;
const MAX_JSON_SCAN_BYTES: usize = MAX_PLAN_CONTRACT_BYTES * 8;

/// Strict JSON Schema for [`PlanContract`] passed to `--json-schema`.
pub fn plan_contract_json_schema() -> &'static str {
    r#"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "summary",
    "tier_assessment",
    "tier_reasons",
    "files_to_change",
    "invariants_and_edge_cases",
    "red_green_tests",
    "dispositions",
    "open_questions"
  ],
  "properties": {
    "summary": { "type": "string", "minLength": 1 },
    "tier_assessment": { "type": "string", "enum": ["S", "M", "L"] },
    "tier_reasons": {
      "type": "array",
      "items": { "type": "string" }
    },
    "files_to_change": {
      "type": "array",
      "minItems": 1,
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["path", "responsibility", "risk"],
        "properties": {
          "path": { "type": "string", "minLength": 1 },
          "responsibility": { "type": "string", "minLength": 1 },
          "risk": { "type": "string" }
        }
      }
    },
    "invariants_and_edge_cases": {
      "type": "array",
      "items": { "type": "string" }
    },
    "red_green_tests": {
      "type": "array",
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["ac_ref", "test_path", "red_condition", "green_condition"],
        "properties": {
          "ac_ref": { "type": "string", "minLength": 1 },
          "test_path": { "type": "string", "minLength": 1 },
          "red_condition": { "type": "string", "minLength": 1 },
          "green_condition": { "type": "string", "minLength": 1 }
        }
      }
    },
    "dispositions": {
      "type": "array",
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["issue_id", "action", "response", "evidence"],
        "properties": {
          "issue_id": { "type": "string", "minLength": 1 },
          "action": { "type": "string", "enum": ["ACCEPTED", "DISPUTED"] },
          "response": { "type": "string", "minLength": 1 },
          "evidence": { "type": "string" }
        }
      }
    },
    "open_questions": {
      "type": "array",
      "items": { "type": "string" }
    }
  }
}"#
}

/// Strict JSON Schema for [`PlanCritique`] passed to `--json-schema`.
pub fn plan_critique_json_schema() -> &'static str {
    r#"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "checklist",
    "resolved_from_previous",
    "issues",
    "verdict"
  ],
  "properties": {
    "checklist": {
      "type": "array",
      "minItems": 1,
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["id", "pass", "evidence"],
        "properties": {
          "id": { "type": "string", "minLength": 1 },
          "pass": { "type": "boolean" },
          "evidence": { "type": "string" }
        }
      }
    },
    "resolved_from_previous": {
      "type": "array",
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["issue_id", "status", "reason"],
        "properties": {
          "issue_id": { "type": "string", "minLength": 1 },
          "status": {
            "type": "string",
            "enum": ["VERIFIED_FIXED", "WITHDRAWN", "UPHELD"]
          },
          "reason": { "type": "string" }
        }
      }
    },
    "issues": {
      "type": "array",
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": [
          "id",
          "severity",
          "category",
          "location",
          "description",
          "proposed_fix"
        ],
        "properties": {
          "id": { "type": "string", "minLength": 1 },
          "severity": { "type": "string", "enum": ["P0", "P1", "P2"] },
          "category": { "type": "string", "minLength": 1 },
          "location": { "type": "string", "minLength": 1 },
          "description": { "type": "string", "minLength": 1 },
          "proposed_fix": { "type": "string" }
        }
      }
    },
    "verdict": { "type": "string", "enum": ["CONVERGED", "REVISE"] }
  }
}"#
}

/// Returns the byte offset just past the matching `}` for the `{` at `start`.
fn balanced_json_object_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth: usize = 0;
    let mut in_string = false;
    let mut escape = false;
    for (idx, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
        } else {
            match b {
                b'"' => in_string = true,
                b'{' => depth += 1,
                b'}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some(idx + 1);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Extracts a single JSON object from raw text or a markdown ```` ```json ```` fence.
fn extract_json_object(raw: &str) -> Result<&str, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("empty JSON response".to_string());
    }
    if trimmed.len() > MAX_JSON_SCAN_BYTES {
        return Err(format!(
            "response length {} exceeds scan limit {MAX_JSON_SCAN_BYTES}",
            trimmed.len()
        ));
    }
    if trimmed.starts_with('{') {
        let Some(end) = balanced_json_object_end(trimmed.as_bytes(), 0) else {
            return Err("unterminated JSON object".to_string());
        };
        let tail = trimmed[end..].trim();
        if !tail.is_empty() {
            return Err("unexpected trailing content after JSON object".to_string());
        }
        return Ok(&trimmed[..end]);
    }
    // Check fenced ```json ... ``` or ``` ... ``` block.
    if let Some(fence_start) = trimmed.find("```") {
        let after_open = &trimmed[fence_start + 3..];
        let body = match after_open.find('\n') {
            Some(nl) => &after_open[nl + 1..],
            None => return Err("malformed markdown code fence".to_string()),
        };
        let Some(fence_end) = body.rfind("```") else {
            return Err("unclosed markdown code fence".to_string());
        };
        let inner = body[..fence_end].trim();
        if !inner.starts_with('{') {
            return Err("code fence does not contain a JSON object".to_string());
        }
        let Some(end) = balanced_json_object_end(inner.as_bytes(), 0) else {
            return Err("unterminated JSON object inside code fence".to_string());
        };
        if !inner[end..].trim().is_empty() {
            return Err("unexpected trailing content inside JSON code fence".to_string());
        }
        return Ok(&inner[..end]);
    }
    Err("response does not contain a JSON object".to_string())
}

/// Computes the deterministic SHA-256 hex digest of a canonical [`PlanContract`].
pub fn canonical_plan_hash(plan: &PlanContract) -> String {
    let json = serde_json::to_string(plan).unwrap_or_default();
    aaf_fp::sha256_hex(json.as_bytes())
}

/// Parses, size-caps, validates, and normalizes a [`PlanContract`], returning
/// `(normalized_plan, canonical_plan_hash)`.
pub fn validate_and_normalize_plan(raw_json: &str) -> Result<(PlanContract, String), String> {
    let extracted = extract_json_object(raw_json)?;
    if extracted.len() > MAX_PLAN_CONTRACT_BYTES {
        return Err(format!(
            "plan contract size {} bytes exceeds maximum of {MAX_PLAN_CONTRACT_BYTES} bytes",
            extracted.len()
        ));
    }
    let mut plan: PlanContract =
        serde_json::from_str(extracted).map_err(|e| format!("invalid PlanContract JSON: {e}"))?;
    let summary = plan.summary.trim().to_string();
    if summary.is_empty() {
        return Err("plan summary must not be empty".to_string());
    }
    plan.summary = summary;
    let tier = plan.tier_assessment.trim().to_ascii_uppercase();
    if !matches!(tier.as_str(), "S" | "M" | "L") {
        return Err(format!(
            "invalid tier_assessment {:?}: expected one of S, M, L",
            plan.tier_assessment
        ));
    }
    plan.tier_assessment = tier;
    if plan.files_to_change.is_empty() {
        return Err("plan files_to_change must not be empty".to_string());
    }
    for change in &mut plan.files_to_change {
        change.path = normalize_repo_path(&change.path)
            .map_err(|e| format!("files_to_change path {:?}: {e}", change.path))?;
    }
    for test in &mut plan.red_green_tests {
        test.test_path = normalize_repo_path(&test.test_path)
            .map_err(|e| format!("red_green_tests test_path {:?}: {e}", test.test_path))?;
    }
    let canonical_json = serde_json::to_string(&plan)
        .map_err(|e| format!("failed to serialize normalized PlanContract: {e}"))?;
    if canonical_json.len() > MAX_PLAN_CONTRACT_BYTES {
        return Err(format!(
            "canonical plan contract size {} bytes exceeds maximum of {MAX_PLAN_CONTRACT_BYTES} bytes",
            canonical_json.len()
        ));
    }
    let hash = aaf_fp::sha256_hex(canonical_json.as_bytes());
    Ok((plan, hash))
}

/// Parses, size-caps, and validates a [`PlanCritique`] JSON payload.
pub fn validate_plan_critique(raw_json: &str) -> Result<PlanCritique, String> {
    let extracted = extract_json_object(raw_json)?;
    if extracted.len() > MAX_PLAN_CONTRACT_BYTES {
        return Err(format!(
            "plan critique size {} bytes exceeds maximum of {MAX_PLAN_CONTRACT_BYTES} bytes",
            extracted.len()
        ));
    }
    let critique: PlanCritique =
        serde_json::from_str(extracted).map_err(|e| format!("invalid PlanCritique JSON: {e}"))?;
    if critique.checklist.is_empty() {
        return Err("plan critique checklist must not be empty".to_string());
    }
    Ok(critique)
}

/// Formats a compact single-line `plan_approved:` context note binding the
/// approved plan summary to its canonical hash.
pub fn format_plan_approved_note(summary: &str, hash: &str) -> String {
    let flat = summary.split_whitespace().collect::<Vec<_>>().join(" ");
    let summary_one_line: String = flat.chars().take(MAX_APPROVED_SUMMARY_CHARS).collect();
    format!("{PLAN_APPROVED_PREFIX} {summary_one_line} [{PLAN_CONTRACT_HASH_TAG}{hash}]")
}

/// Extracts the latest valid 64-hex-char `[plan_contract_hash=<64hex>]` tag
/// from a task's `context_notes`.
pub fn extract_plan_contract_hash(notes: &[String]) -> Option<String> {
    let prefix = format!("[{PLAN_CONTRACT_HASH_TAG}");
    for note in notes.iter().rev() {
        let mut search_end = note.len();
        while let Some(pos) = note[..search_end].rfind(&prefix) {
            let after = &note[pos + prefix.len()..];
            if let Some(close) = after.find(']') {
                let candidate = &after[..close];
                if candidate.len() == SHA256_HEX_LEN
                    && candidate.bytes().all(|b| b.is_ascii_hexdigit())
                {
                    return Some(candidate.to_ascii_lowercase());
                }
            }
            search_end = pos;
        }
    }
    None
}
