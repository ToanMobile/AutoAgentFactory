//! Cross-Adjudication evaluation (Branch 4): fail-closed `CONCUR`, conservative
//! `P0`, non-rebuttable deterministic `SCOPE-HARD-*` violations, and verified
//! `file:line` citations for `P1` rebuttals.

use std::collections::HashMap;
use std::{fs, path::Path};

use aaf_domain::pipeline::{AdjudicateDecision, AdjudicateVerdict, IssueSeverity, ReviewIssue};
use aaf_domain::prompt_fence::normalize_repo_path;
use serde::Deserialize;

use super::{AdjudicationResult, SCOPE_HARD_PREFIX, extract_json};
use crate::cross_audit::diff_hunks::{
    DiffHunks, evidence_in_diff, extract_citations, parse_diff_hunks_with_truncation,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAdjudicate {
    verdicts: Vec<AdjudicateVerdict>,
}

fn verify_rebut_citation(evidence: &str, hunks: &DiffHunks, repo_root: &Path) -> bool {
    if evidence_in_diff(evidence, hunks) {
        return true;
    }
    let Ok(root) = fs::canonicalize(repo_root) else {
        return false;
    };
    extract_citations(evidence).iter().any(|c| {
        let p = c.path.as_str();
        [
            Some(p),
            p.strip_prefix("./"),
            p.strip_prefix("a/"),
            p.strip_prefix("b/"),
        ]
        .into_iter()
        .flatten()
        .any(|cand| {
            let Ok(norm) = normalize_repo_path(cand) else {
                return false;
            };
            let Ok(file) = fs::canonicalize(repo_root.join(norm)) else {
                return false;
            };
            file.starts_with(&root)
                && file.is_file()
                && fs::read_to_string(file).is_ok_and(|s| {
                    c.start >= 1 && c.end >= c.start && (c.end as usize) <= s.lines().count()
                })
        })
    })
}

/// Evaluates one Cross-Adjudication turn (fail-closed `CONCUR`, conservative `P0`,
/// non-rebuttable deterministic `SCOPE-HARD-*`).
pub fn evaluate_adjudication(
    raw: &str,
    original_issues: &[ReviewIssue],
    diff: &str,
    repo_root: &Path,
) -> AdjudicationResult {
    let parsed: RawAdjudicate = match extract_json(raw, "adjudication") {
        Ok(v) => v,
        Err(e) => {
            return AdjudicationResult {
                surviving_issues: original_issues.to_vec(),
                dropped_p1_issues: vec![],
                upheld_reasons: vec![format!("adjudication invalid (fail-closed CONCUR): {e}")],
            };
        }
    };
    let hunks = parse_diff_hunks_with_truncation(diff, false);
    let mut by_id: HashMap<&str, Option<&AdjudicateVerdict>> = HashMap::new();
    for v in &parsed.verdicts {
        by_id
            .entry(v.issue_id.as_str())
            .and_modify(|s| *s = None)
            .or_insert(Some(v));
    }
    let (mut surviving, mut dropped, mut upheld) = (vec![], vec![], vec![]);
    for iss in original_issues {
        let v = by_id.get(iss.id.as_str()).and_then(|x| *x);
        let rebut = v.is_some_and(|x| x.decision == AdjudicateDecision::Rebut);
        let hard_scope = iss.id.starts_with(SCOPE_HARD_PREFIX);
        if iss.severity != IssueSeverity::P0
            && !hard_scope
            && rebut
            && let Some(x) = v
            && verify_rebut_citation(&x.evidence, &hunks, repo_root)
        {
            dropped.push((
                iss.clone(),
                format!("REBUT ({}): {}", x.evidence.trim(), x.reason.trim()),
            ));
            continue;
        }
        surviving.push(iss.clone());
        let why = match (iss.severity, hard_scope, v) {
            (IssueSeverity::P0, _, Some(x)) if rebut => {
                format!("P0 kept despite REBUT ({})", x.reason.trim())
            }
            (IssueSeverity::P0, _, Some(x)) => format!("P0 CONCUR ({})", x.reason.trim()),
            (IssueSeverity::P0, _, None) => "P0 kept (missing/duplicate verdict)".into(),
            (_, true, _) => "deterministic hard scope violation cannot be rebutted".into(),
            (_, false, Some(x)) if rebut => {
                format!("REBUT citation {:?} unverified (CONCUR)", x.evidence)
            }
            (_, false, Some(x)) => format!("CONCUR ({})", x.reason.trim()),
            (_, false, None) => "missing/duplicate verdict (CONCUR)".into(),
        };
        upheld.push(format!("{}: {why}", iss.id));
    }
    AdjudicationResult {
        surviving_issues: surviving,
        dropped_p1_issues: dropped,
        upheld_reasons: upheld,
    }
}
