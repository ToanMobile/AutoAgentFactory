//! 7 Golden Standards rubric, deterministic mechanical scoring from signed
//! verification receipts, and combined score-gate evaluation.

use aaf_domain::gate::{GateStatus, GoldenScore, Receipt, Severity};
use serde::{Deserialize, Serialize};

/// Canonical identifiers of the 7 Golden Standards in fixed order.
pub const GOLDEN_IDS: [&str; 7] = ["G1", "G2", "G3", "G4", "G5", "G6", "G7"];

/// Names and concise scoring rubrics for the 7 Golden Standards (`G1`..`G7`).
pub const GOLDEN_STANDARDS: [(&str, &str); 7] = [
    (
        "G1",
        "Root Cause & Architecture: fixes the true root cause at the right abstraction boundary without architectural drift",
    ),
    (
        "G2",
        "Security & Trust Boundaries: validates untrusted inputs, avoids secret leaks, and preserves sandbox and permission boundaries",
    ),
    (
        "G3",
        "Correctness & Edge Cases: handles boundary inputs, error paths, concurrency, and failure modes cleanly",
    ),
    (
        "G4",
        "Test Quality & Oracle Proof: adds discriminating tests at the real failure boundary without weakening assertions",
    ),
    (
        "G5",
        "Performance & Resource Lifecycle: avoids hot-loop waste and O(N^2) traps, and releases handles, timers, and scopes cleanly",
    ),
    (
        "G6",
        "Code Hygiene & Modularity: keeps diffs surgical, readable, modular, and free of dead code or silent catches",
    ),
    (
        "G7",
        "Observability & Handover Readiness: emits actionable structured errors/logs without PII and leaves clean handover state",
    ),
];

/// Deterministic score computed purely from a signed [`Receipt`] and audit
/// provider metadata, independent of any LLM judgment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MechanicalScoreReport {
    pub score: u8,
    pub reasons: Vec<String>,
    pub same_account_warning: bool,
}

/// Combined pass/fail evaluation of mechanical score + 7 Golden Standards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScoreGateEvaluation {
    pub passed: bool,
    pub mechanical_score: u8,
    pub llm_average: Option<u8>,
    pub effective_score: u8,
    pub reasons: Vec<String>,
}

/// Computes the deterministic mechanical score (0..=100) from a signed
/// [`Receipt`] and provider-separation settings.
///
/// Hard-zero triggers (returns `0` immediately):
/// - `receipt.status != GateStatus::Clear`
/// - `receipt.store_compliance.critical > 0`
/// - any `receipt.static_findings` item has `Severity::Critical`
/// - `!proof_ok`
///
/// Deductions from `100`:
/// - `-2` per warning (static `Warn` + `store_compliance.warnings`), cap `-10`
/// - `-1` per `receipt.suppressions`, cap `-5`
/// - `-3` per `receipt.store_compliance.unresolved` item, cap `-12`
///
/// Solo-dev non-deadlocking rule:
/// - `require_different_provider && same_account` caps score at `94`
/// - `!require_different_provider && same_account` does not cap, sets
///   `same_account_warning = true`
pub fn compute_mechanical_score(
    receipt: &Receipt,
    proof_ok: bool,
    require_different_provider: bool,
    same_account: bool,
) -> MechanicalScoreReport {
    let mut hard_reasons = Vec::new();
    if receipt.status != GateStatus::Clear {
        hard_reasons.push(format!("gate status is {:?}, not Clear", receipt.status));
    }
    if receipt.store_compliance.critical > 0 {
        hard_reasons.push(format!(
            "store compliance reported {} critical finding(s)",
            receipt.store_compliance.critical
        ));
    }
    let critical_static = receipt
        .static_findings
        .iter()
        .filter(|finding| finding.severity == Severity::Critical)
        .count();
    if critical_static > 0 {
        hard_reasons.push(format!(
            "static analysis reported {critical_static} critical finding(s)"
        ));
    }
    if !proof_ok {
        hard_reasons.push("proof verification did not pass".to_string());
    }
    if !hard_reasons.is_empty() {
        return MechanicalScoreReport {
            score: 0,
            reasons: hard_reasons,
            same_account_warning: !require_different_provider && same_account,
        };
    }

    let mut score: i32 = 100;
    let mut reasons = Vec::new();

    let static_warns = u64::try_from(
        receipt
            .static_findings
            .iter()
            .filter(|finding| finding.severity == Severity::Warn)
            .count(),
    )
    .unwrap_or(u64::MAX);
    let total_warns = static_warns.saturating_add(u64::from(receipt.store_compliance.warnings));
    if total_warns > 0 {
        let penalty = i32::try_from(total_warns.saturating_mul(2).min(10)).unwrap_or(10);
        score -= penalty;
        reasons.push(format!(
            "{total_warns} warning(s) across static/store checks (-{penalty}, cap -10)"
        ));
    }

    if receipt.suppressions > 0 {
        let penalty = i32::try_from(receipt.suppressions.min(5)).unwrap_or(5);
        score -= penalty;
        reasons.push(format!(
            "{} inline suppression(s) honoured (-{penalty}, cap -5)",
            receipt.suppressions
        ));
    }

    let unresolved = receipt.store_compliance.unresolved.len();
    if unresolved > 0 {
        let penalty = i32::try_from(unresolved.saturating_mul(3).min(12)).unwrap_or(12);
        score -= penalty;
        reasons.push(format!(
            "{unresolved} unresolved store-compliance item(s) (-{penalty}, cap -12)"
        ));
    }

    let mut final_score = u8::try_from(score.clamp(0, 100)).unwrap_or(0);
    let same_account_warning = if require_different_provider && same_account {
        final_score = final_score.min(94);
        reasons.push(
            "require_different_provider=true with same_account caps mechanical score at 94"
                .to_string(),
        );
        false
    } else {
        !require_different_provider && same_account
    };

    MechanicalScoreReport {
        score: final_score,
        reasons,
        same_account_warning,
    }
}

fn validate_golden_coverage(scores: &[GoldenScore]) -> Result<(), String> {
    if scores.len() != GOLDEN_IDS.len() {
        return Err(format!(
            "expected {} golden scores (G1..G7), got {}",
            GOLDEN_IDS.len(),
            scores.len()
        ));
    }
    let mut seen = [false; GOLDEN_IDS.len()];
    for entry in scores {
        let Some(index) = GOLDEN_IDS.iter().position(|id| entry.id == *id) else {
            return Err(format!("unknown golden standard id {:?}", entry.id));
        };
        if seen[index] {
            return Err(format!("duplicate golden standard id {}", entry.id));
        }
        if entry.score > 100 {
            return Err(format!(
                "golden standard {} score {} exceeds 100",
                entry.id, entry.score
            ));
        }
        seen[index] = true;
    }
    for (id, present) in GOLDEN_IDS.iter().zip(seen) {
        if !present {
            return Err(format!("missing golden standard id {id}"));
        }
    }
    Ok(())
}

/// Evaluates whether the combined mechanical score and 7 Golden Standards
/// satisfy `min_average` (`min(mech, llm_avg) >= min_average`) and
/// `min_criterion` (every `G1..G7` score `>= min_criterion`).
///
/// Fails closed with `passed: false`, `llm_average: None`, `effective_score: 0`
/// whenever `scores` does not cover `G1..G7` exactly once each with scores
/// `<= 100`.
pub fn evaluate_score_gate(
    mech: &MechanicalScoreReport,
    scores: &[GoldenScore],
    min_average: u8,
    min_criterion: u8,
) -> ScoreGateEvaluation {
    if let Err(reason) = validate_golden_coverage(scores) {
        return ScoreGateEvaluation {
            passed: false,
            mechanical_score: mech.score,
            llm_average: None,
            effective_score: 0,
            reasons: vec![reason],
        };
    }

    let sum: u32 = scores.iter().map(|entry| u32::from(entry.score)).sum();
    let llm_avg = u8::try_from(sum / 7).unwrap_or(100);
    let effective_score = mech.score.min(llm_avg);

    let mut reasons = Vec::new();
    if effective_score < min_average {
        reasons.push(format!(
            "effective score {effective_score} (min(mechanical={}, llm_avg={llm_avg})) is below minimum {min_average}",
            mech.score
        ));
    }
    for id in GOLDEN_IDS {
        if let Some(entry) = scores.iter().find(|entry| entry.id == id)
            && entry.score < min_criterion
        {
            reasons.push(format!(
                "criterion {} scored {} which is below minimum {min_criterion}",
                entry.id, entry.score
            ));
        }
    }

    ScoreGateEvaluation {
        passed: reasons.is_empty(),
        mechanical_score: mech.score,
        llm_average: Some(llm_avg),
        effective_score,
        reasons,
    }
}
