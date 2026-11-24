//! Domain types for Dual-Frontier Plan Debate, Tiered Implementer, and Blind Dual-Review.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::gate::GoldenScore;

/// Hard upper bound on Plan Debate rounds before escalating to a human.
pub const MAX_PLAN_ROUNDS_CAP: u32 = 5;
/// Default maximum Plan Debate rounds when `plan_debate = true`.
pub const DEFAULT_MAX_PLAN_ROUNDS: u32 = 3;
/// Hard upper bound on automatic PR feedback rework rounds.
pub const MAX_PR_REWORK_ROUNDS_CAP: u32 = 3;
/// Default maximum automatic PR feedback rework rounds (`0` disables PR feedback).
pub const DEFAULT_MAX_PR_REWORK_ROUNDS: u32 = 2;
/// Default rework threshold before escalating the implementer stage.
pub const DEFAULT_ESCALATE_IMPLEMENTER_AFTER_REWORKS: u32 = 1;
/// Hard byte cap on a serialized [`PlanContract`] JSON payload (32 KiB).
pub const MAX_PLAN_CONTRACT_BYTES: usize = 32 * 1024;

/// Maximum length of a [`ReviewerConfig::id`] in bytes.
const MAX_REVIEWER_ID_LEN: usize = 64;

/// Maps provider/executor names to their canonical model-family identity.
///
/// `"antigravity"` and `"gemini"` share the Gemini model family and collapse to
/// `"gemini"` so they never count as two independent providers.
pub fn canonical_provider(provider: &str) -> String {
    let norm = provider.trim().to_ascii_lowercase();
    match norm.as_str() {
        "antigravity" | "gemini" => "gemini".to_string(),
        _ => norm,
    }
}

/// Returns `true` for known real provider executors.
///
/// Excludes `"custom"`, empty strings, and unknown executors so unverified
/// wrapper commands cannot satisfy cross-provider diversity requirements.
pub fn is_known_real_provider(executor: &str) -> bool {
    matches!(
        executor.trim().to_ascii_lowercase().as_str(),
        "claude"
            | "codex"
            | "gemini"
            | "antigravity"
            | "opencode"
            | "cursor"
            | "kimi"
            | "pi"
            | "codebuddy"
            | "deepseek"
    )
}

/// Counts distinct canonical providers among known real executors.
///
/// `"custom"`, empty, and unknown executors are ignored; `"antigravity"` and
/// `"gemini"` collapse to one canonical provider (`"gemini"`).
pub fn distinct_real_canonical_providers<'a>(
    executors: impl IntoIterator<Item = &'a str>,
) -> usize {
    let mut providers = BTreeSet::new();
    for executor in executors {
        if is_known_real_provider(executor) {
            providers.insert(canonical_provider(executor));
        }
    }
    providers.len()
}

/// Returns `true` when `reviewer` uses a known real built-in provider with no
/// custom `executor_cmd` override.
pub fn is_known_real_reviewer(reviewer: &ReviewerConfig) -> bool {
    reviewer.executor_cmd.trim().is_empty() && is_known_real_provider(&reviewer.executor)
}

/// Counts distinct canonical providers among [`ReviewerConfig`] entries that
/// pass [`is_known_real_reviewer`].
pub fn distinct_real_reviewer_providers(reviewers: &[ReviewerConfig]) -> usize {
    let mut set = BTreeSet::new();
    for r in reviewers {
        if is_known_real_reviewer(r) {
            set.insert(canonical_provider(&r.executor));
        }
    }
    set.len()
}

/// Executor/model/effort selection for one pipeline stage.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StageModelConfig {
    pub executor: String,
    pub executor_cmd: String,
    pub model: String,
    pub effort: String,
}

impl StageModelConfig {
    pub fn is_empty(&self) -> bool {
        self.executor.trim().is_empty()
            && self.executor_cmd.trim().is_empty()
            && self.model.trim().is_empty()
            && self.effort.trim().is_empty()
    }
}

/// One configured reviewer in `[[cross_audit.reviewers]]`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReviewerConfig {
    pub id: String,
    pub executor: String,
    pub executor_cmd: String,
    pub model: String,
    pub effort: String,
}

/// Validates that a reviewer id is 1..=64 ASCII `[A-Za-z0-9_-]` bytes.
pub fn validate_reviewer_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("reviewer id must not be empty".to_string());
    }
    if id.len() > MAX_REVIEWER_ID_LEN {
        return Err(format!(
            "reviewer id length {} exceeds maximum of {MAX_REVIEWER_ID_LEN}",
            id.len()
        ));
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(format!(
            "reviewer id {id:?} must contain only ASCII letters, digits, '_' or '-'"
        ));
    }
    Ok(())
}

/// Validates all reviewer ids and rejects duplicate ids.
pub fn validate_reviewers(reviewers: &[ReviewerConfig]) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for reviewer in reviewers {
        validate_reviewer_id(&reviewer.id)?;
        if !seen.insert(reviewer.id.as_str()) {
            return Err(format!("duplicate reviewer id {:?}", reviewer.id));
        }
    }
    Ok(())
}

/// Tier-1 machine-wide `[pipeline]` defaults/floors.
///
/// Every field is `Option<...>` so unspecified Tier-1 keys never clobber
/// explicit Tier-2 repository configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GlobalPipelineConfig {
    pub plan_stage: Option<bool>,
    pub plan_debate: Option<bool>,
    pub require_plan_approval: Option<bool>,
    pub max_plan_rounds: Option<u32>,
    pub allow_tier_s_skip_debate: Option<bool>,
    pub max_audit_reworks: Option<u32>,
    pub max_pr_rework_rounds: Option<u32>,
    pub escalate_implementer_after_reworks: Option<u32>,
    pub plan_author: Option<StageModelConfig>,
    pub plan_challenger: Option<StageModelConfig>,
    pub implementer: Option<StageModelConfig>,
    pub implementer_escalated: Option<StageModelConfig>,
}

/// Tier-1 machine-wide `[unblocker]` defaults/floors.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GlobalUnblockerConfig {
    pub max_token_budget: Option<u64>,
    pub max_wall_clock_secs: Option<u64>,
    pub max_unblock_cycles: Option<u32>,
}

/// Deterministic task complexity tier used for plan-debate skipping and implementer routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum TaskTier {
    S,
    M,
    L,
}

impl TaskTier {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskTier::S => "S",
            TaskTier::M => "M",
            TaskTier::L => "L",
        }
    }
}

/// One planned file modification in a [`PlanContract`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanFileChange {
    pub path: String,
    pub responsibility: String,
    #[serde(default)]
    pub risk: String,
}

/// One planned RED -> GREEN test case in a [`PlanContract`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedGreenTestPlan {
    pub ac_ref: String,
    pub test_path: String,
    pub red_condition: String,
    pub green_condition: String,
}

/// Author response action to a prior-round challenger issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DispositionAction {
    #[serde(rename = "ACCEPTED")]
    Accepted,
    #[serde(rename = "DISPUTED")]
    Disputed,
}

/// Author disposition for one challenger issue from the previous round.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanDisposition {
    pub issue_id: String,
    pub action: DispositionAction,
    pub response: String,
    #[serde(default)]
    pub evidence: String,
}

/// Structured implementation contract produced by the Plan Author.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanContract {
    pub summary: String,
    pub tier_assessment: String,
    #[serde(default)]
    pub tier_reasons: Vec<String>,
    #[serde(default)]
    pub files_to_change: Vec<PlanFileChange>,
    #[serde(default)]
    pub invariants_and_edge_cases: Vec<String>,
    #[serde(default)]
    pub red_green_tests: Vec<RedGreenTestPlan>,
    #[serde(default)]
    pub dispositions: Vec<PlanDisposition>,
    #[serde(default)]
    pub open_questions: Vec<String>,
}

/// One checklist entry evaluated by the Plan Challenger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanChecklistItem {
    pub id: String,
    pub pass: bool,
    #[serde(default)]
    pub evidence: String,
}

/// Challenger resolution status for an issue carried from the prior round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ResolvedStatus {
    #[serde(rename = "VERIFIED_FIXED")]
    VerifiedFixed,
    #[serde(rename = "WITHDRAWN")]
    Withdrawn,
    #[serde(rename = "UPHELD")]
    Upheld,
}

/// Resolution record for one previous-round issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedIssue {
    pub issue_id: String,
    pub status: ResolvedStatus,
    #[serde(default)]
    pub reason: String,
}

/// Finding severity shared by Plan Debate critiques and Dual-Review audits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum IssueSeverity {
    P0,
    P1,
    P2,
}

impl IssueSeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            IssueSeverity::P0 => "P0",
            IssueSeverity::P1 => "P1",
            IssueSeverity::P2 => "P2",
        }
    }
}

/// One actionable finding reported by a Plan Challenger or Dual-Review auditor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewIssue {
    pub id: String,
    pub severity: IssueSeverity,
    pub category: String,
    pub location: String,
    pub description: String,
    #[serde(default)]
    pub proposed_fix: String,
}

/// Plan Challenger's self-reported verdict (mechanically verified by the engine).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PlanCritiqueVerdict {
    #[serde(rename = "CONVERGED")]
    Converged,
    #[serde(rename = "REVISE")]
    Revise,
}

/// Structured critique returned by the Plan Challenger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanCritique {
    #[serde(default)]
    pub checklist: Vec<PlanChecklistItem>,
    #[serde(default)]
    pub resolved_from_previous: Vec<ResolvedIssue>,
    #[serde(default)]
    pub issues: Vec<ReviewIssue>,
    pub verdict: PlanCritiqueVerdict,
}

/// Decision returned by a passing reviewer during Cross-Adjudication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AdjudicateDecision {
    #[serde(rename = "CONCUR")]
    Concur,
    #[serde(rename = "REBUT")]
    Rebut,
}

/// One per-issue verdict during Cross-Adjudication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdjudicateVerdict {
    pub issue_id: String,
    pub decision: AdjudicateDecision,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub reason: String,
}

/// Structured JSON payload returned by a Cross-Adjudication turn.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AdjudicateResponse {
    #[serde(default)]
    pub verdicts: Vec<AdjudicateVerdict>,
}

/// Per-reviewer summary embedded in a signed [`crate::gate::PostGateAuditRecord`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewerVerdictRecord {
    pub id: String,
    pub executor: String,
    pub canonical_provider: String,
    pub model: String,
    pub effort: String,
    pub verdict: String,
    pub p0: u32,
    pub p1: u32,
    pub p2: u32,
    #[serde(default)]
    pub llm_average: Option<u8>,
    #[serde(default)]
    pub scores: Vec<GoldenScore>,
}
