//! Schema, prompts, citation validation, and cross-adjudication for Blind
//! Parallel Dual-Review (`docs/PLAN_DUAL_FRONTIER.md` §D.4).

use std::collections::{BTreeSet, HashMap};

use aaf_domain::gate::GoldenScore;
use aaf_domain::pipeline::{IssueSeverity, PlanContract, ReviewIssue};
use aaf_domain::prompt_fence::normalize_repo_path;
use aaf_domain::task::ImmutableSpec;
use serde::{Deserialize, de::DeserializeOwned};

use super::checklist::{self, ID_ASSERT, ID_GATE_CI, ID_HARDCODED, ID_SECRET, ID_STUB};
use super::diff_hunks::{
    Citation, evidence_in_diff, extract_citations, find_verified_citation,
    parse_diff_hunks_with_truncation,
};
use super::prompt::{MAX_DIFF_BYTES, neutralise, truncate_diff_with_limit};
use super::scoring::{GOLDEN_IDS, MechanicalScoreReport, evaluate_score_gate};
use super::verdict::strip_ansi_escapes;

mod adjudicate;
pub mod conformance;
mod conformance_rules;
mod prompt;
mod repair_check;
pub use adjudicate::evaluate_adjudication;
pub use conformance::{
    CONFORMANCE_BEGIN, CONFORMANCE_END, PlanConformance, UnplannedFile, analyze_plan_conformance,
    deterministic_scope_violations, format_conformance_summary,
};
pub use prompt::{
    build_adjudicate_prompt, build_adjudicate_prompt_with_limit, build_dual_review_prompt,
    build_dual_review_prompt_with_conformance, build_dual_review_prompt_with_limit,
};
pub use repair_check::{can_use_compact_format_repair, names_dual_blocker};

pub type ChecklistItem = checklist::Item;
/// Legacy id kept for backward compatibility (omission rules still accept it).
pub const PLAN_DRIFT_ID: &str = "PLAN-DRIFT";
pub const PLAN_OMISSION_ID: &str = "PLAN-OMISSION";
pub const PLAN_SCOPE_ID: &str = "PLAN-SCOPE";
pub const SCOPE_HARD_PREFIX: &str = "SCOPE-HARD-";
/// Version hashed into `config_hash` for `dual_review` so checklist/policy changes invalidate cached partials.
pub const DUAL_CHECKLIST_VERSION: u8 = 2;
pub const FINDINGS_BEGIN: &str = "<<<AAF_UNTRUSTED_FINDINGS_BEGIN>>>";
pub const FINDINGS_END: &str = "<<<AAF_UNTRUSTED_FINDINGS_END>>>";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DualReviewerOutcome {
    Pass {
        scores: Vec<GoldenScore>,
        llm_average: Option<u8>,
        p2_issues: Vec<ReviewIssue>,
    },
    Fail {
        issues: Vec<ReviewIssue>,
        scores: Vec<GoldenScore>,
        llm_average: Option<u8>,
        score_fail_reasons: Vec<String>,
    },
    Inconclusive {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdjudicationResult {
    pub surviving_issues: Vec<ReviewIssue>,
    pub dropped_p1_issues: Vec<(ReviewIssue, String)>,
    pub upheld_reasons: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DualReviewRaw {
    answers: Vec<RawAnswer>,
    #[serde(default)]
    issues: Vec<ReviewIssue>,
    #[serde(default)]
    scores: Option<Vec<GoldenScore>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAnswer {
    id: String,
    answer: String,
    #[serde(default)]
    evidence: String,
}

/// Strict inline JSON Schema for a Blind Dual-Review response.
pub fn dual_review_json_schema(score_gate: bool) -> String {
    let req = if score_gate {
        r#"["answers","issues","scores"]"#
    } else {
        r#"["answers","issues"]"#
    };
    let sc = if score_gate {
        r#","scores":{"type":"array","minItems":7,"maxItems":7,"items":{"type":"object","additionalProperties":false,"required":["id","score","deduction_reason"],"properties":{"id":{"type":"string","enum":["G1","G2","G3","G4","G5","G6","G7"]},"score":{"type":"integer","minimum":0,"maximum":100},"deduction_reason":{"type":"string"}}}}"#
    } else {
        ""
    };
    format!(
        r#"{{"type":"object","additionalProperties":false,"required":{req},"properties":{{"answers":{{"type":"array","minItems":1,"items":{{"type":"object","additionalProperties":false,"required":["id","answer","evidence"],"properties":{{"id":{{"type":"string","minLength":1}},"answer":{{"type":"string","enum":["yes","no"]}},"evidence":{{"type":"string"}}}}}}}},"issues":{{"type":"array","items":{{"type":"object","additionalProperties":false,"required":["id","severity","category","location","description","proposed_fix"],"properties":{{"id":{{"type":"string","minLength":1}},"severity":{{"type":"string","enum":["P0","P1","P2"]}},"category":{{"type":"string","minLength":1}},"location":{{"type":"string","minLength":1}},"description":{{"type":"string","minLength":1}},"proposed_fix":{{"type":"string"}}}}}}}}{sc}}}}}"#
    )
}

/// Strict inline JSON Schema for [`aaf_domain::pipeline::AdjudicateResponse`].
pub fn adjudicate_json_schema() -> &'static str {
    r#"{"type":"object","additionalProperties":false,"required":["verdicts"],"properties":{"verdicts":{"type":"array","items":{"type":"object","additionalProperties":false,"required":["issue_id","decision","evidence","reason"],"properties":{"issue_id":{"type":"string","minLength":1},"decision":{"type":"string","enum":["CONCUR","REBUT"]},"evidence":{"type":"string"},"reason":{"type":"string"}}}}}}"#
}

/// Builds the Dual-Review checklist, appending [`PLAN_OMISSION_ID`] and [`PLAN_SCOPE_ID`] when `has_plan`.
pub fn build_dual_checklist(
    spec: &ImmutableSpec,
    mobile: bool,
    has_plan: bool,
) -> Vec<ChecklistItem> {
    let mut items = checklist::build_checklist(spec, mobile);
    if has_plan {
        items.push(ChecklistItem {
            id: PLAN_OMISSION_ID.into(),
            question: "Does the implementation omit any required file, invariant, or RED->GREEN \
                       test from the converged PlanContract?"
                .into(),
            blocking: true,
        });
        items.push(ChecklistItem {
            id: PLAN_SCOPE_ID.into(),
            question: "Does any UNPLANNED changed file introduce behavior unrelated to the \
                       spec/plan, or touch auth, permission, sandbox, crypto, or network code \
                       not named in the PlanContract? Helpers, test seams, tests, and docs \
                       directly serving a planned responsibility are NOT drift."
                .into(),
            blocking: true,
        });
    }
    items
}

pub(super) fn extract_json<T: DeserializeOwned>(text: &str, label: &str) -> Result<T, String> {
    if text.len() > 512 * 1024 {
        return Err(format!("{label} exceeds scan limit"));
    }
    let clean = strip_ansi_escapes(text);
    let (s, b) = (clean.as_ref(), clean.as_bytes());
    let (mut found, mut count, mut i) = (None, 0usize, 0usize);
    while let Some(rel) = b.get(i..).and_then(|x| x.iter().position(|&c| c == b'{')) {
        let start = i + rel;
        if b[start + 1..].iter().find(|c| !c.is_ascii_whitespace()) != Some(&b'"') {
            i = start + 1;
            continue;
        }
        count += 1;
        if count > 4096 {
            return Err(format!("{label} has too many JSON candidates"));
        }
        let mut de =
            serde_json::Deserializer::from_str(&s[start..]).into_iter::<serde_json::Value>();
        if let Some(Ok(val)) = de.next() {
            if let Ok(p) = serde_json::from_value::<T>(val)
                && found.replace(p).is_some()
            {
                return Err(format!("{label} has multiple JSON objects"));
            }
            i = start + de.byte_offset().max(1);
        } else {
            i = start + 1;
        }
    }
    found.ok_or_else(|| format!("no valid {label} JSON object found"))
}

pub(super) fn parse_scores(raw: Vec<GoldenScore>) -> Result<Vec<GoldenScore>, String> {
    let mut slots: [Option<GoldenScore>; 7] = std::array::from_fn(|_| None);
    for e in raw {
        let idx = GOLDEN_IDS
            .iter()
            .position(|id| e.id == *id)
            .ok_or_else(|| format!("unknown golden id {:?}", e.id))?;
        if e.score > 100 || (e.score < 100 && e.deduction_reason.trim().is_empty()) {
            return Err(format!("invalid score for {}: {}", e.id, e.score));
        }
        if slots[idx].replace(e).is_some() {
            return Err(format!("duplicate golden id {}", GOLDEN_IDS[idx]));
        }
    }
    GOLDEN_IDS
        .iter()
        .zip(slots)
        .map(|(id, s)| s.ok_or_else(|| format!("missing {id}")))
        .collect()
}

fn checklist_sev(id: &str) -> IssueSeverity {
    let u = id.trim().to_ascii_uppercase();
    let p0 = matches!(
        u.as_str(),
        ID_SECRET | checklist::ID_TEST_BRANCH | ID_HARDCODED | ID_ASSERT | ID_GATE_CI
    );
    if p0 {
        IssueSeverity::P0
    } else {
        IssueSeverity::P1
    }
}

fn is_omission(id: &str, cat: &str) -> bool {
    [id, cat].iter().any(|s| {
        let u = s.trim().to_ascii_uppercase();
        u == PLAN_DRIFT_ID || u == PLAN_OMISSION_ID || u == ID_STUB || u.starts_with("AC-")
    })
}

fn format_citation(c: &Citation) -> String {
    if c.end > c.start {
        format!("{}:{}-{}", c.path, c.start, c.end)
    } else {
        format!("{}:{}", c.path, c.start)
    }
}

fn find_omission(
    text: &str,
    planned: &BTreeSet<String>,
    in_diff: &BTreeSet<String>,
) -> Option<String> {
    const W: &[char] = &['`', '"', '\'', '(', ')', '[', ']', '<', '>', ',', ';', '.'];
    let check = |raw: &str| {
        let t = raw.trim().trim_matches(W);
        let t = t.split_once("#L").map_or(t, |(p, _)| p);
        let t = t.split_once(':').map_or(t, |(p, _)| p).trim_matches(W);
        let t = t
            .strip_prefix("a/")
            .or_else(|| t.strip_prefix("b/"))
            .unwrap_or(t);
        let norm = normalize_repo_path(t).ok()?;
        (planned.contains(&norm) && !in_diff.contains(&norm)).then_some(norm)
    };
    check(text).or_else(|| {
        text.split(|c: char| c.is_whitespace() || c == ',' || c == ';')
            .find_map(check)
    })
}

/// Parses and validates a Blind Dual-Review response: reconciles `answers` with
/// `issues`, validates citations (with `PlanContract` omissions normalized to
/// `<path>:1`), fails closed on 100% hallucinated citations, and checks `score_gate`.
#[allow(clippy::too_many_arguments)]
pub fn parse_and_validate_dual_review(
    raw: &str,
    checklist: &[ChecklistItem],
    diff: &str,
    plan: Option<&PlanContract>,
    score_gate: bool,
    min_avg: u8,
    min_crit: u8,
    mechanical_score: u8,
) -> DualReviewerOutcome {
    parse_and_validate_dual_review_with_limit(
        raw,
        checklist,
        diff,
        plan,
        score_gate,
        min_avg,
        min_crit,
        mechanical_score,
        MAX_DIFF_BYTES,
    )
}

/// Like [`parse_and_validate_dual_review`] with an explicit diff byte ceiling.
#[allow(clippy::too_many_arguments)]
pub fn parse_and_validate_dual_review_with_limit(
    raw: &str,
    checklist: &[ChecklistItem],
    diff: &str,
    plan: Option<&PlanContract>,
    score_gate: bool,
    min_avg: u8,
    min_crit: u8,
    mechanical_score: u8,
    max_diff_bytes: usize,
) -> DualReviewerOutcome {
    let run = || -> Result<DualReviewerOutcome, String> {
        if !checklist.iter().any(|it| it.blocking) {
            return Err("checklist has no blocking item".into());
        }
        let parsed: DualReviewRaw = extract_json(raw, "dual-review")?;
        let (mut seen, mut yes_ev) = (BTreeSet::new(), HashMap::new());
        for a in parsed.answers {
            if !checklist.iter().any(|it| it.id == a.id) {
                return Err(format!("unknown checklist id {:?}", a.id));
            }
            if !seen.insert(a.id.clone()) {
                return Err(format!("duplicate checklist id {}", a.id));
            }
            match a.answer.as_str() {
                "yes" => {
                    yes_ev.insert(a.id.to_ascii_uppercase(), a.evidence);
                }
                "no" => {}
                x => return Err(format!("{}: invalid answer {x:?}", a.id)),
            }
        }
        if seen.len() != checklist.len() {
            return Err("missing required checklist id".into());
        }
        if score_gate && parsed.scores.is_none() {
            return Err("missing required golden scores (G1..G7)".into());
        }
        let scores = parsed
            .scores
            .map(parse_scores)
            .transpose()?
            .unwrap_or_default();
        let (mut blocking, mut p2): (Vec<_>, Vec<_>) = parsed
            .issues
            .into_iter()
            .partition(|i| i.severity != IssueSeverity::P2);
        for it in checklist.iter().filter(|it| it.blocking) {
            let Some(ev) = yes_ev.get(&it.id.to_ascii_uppercase()) else {
                continue;
            };
            let dup = blocking.iter().any(|i| {
                i.id.eq_ignore_ascii_case(&it.id) || i.category.eq_ignore_ascii_case(&it.id)
            });
            if !dup {
                let loc = ev.trim();
                let desc = if loc.is_empty() {
                    it.question.clone()
                } else {
                    format!("{} ({loc})", it.question)
                };
                blocking.push(ReviewIssue {
                    id: it.id.clone(),
                    severity: checklist_sev(&it.id),
                    category: it.id.clone(),
                    location: loc.into(),
                    description: desc,
                    proposed_fix: format!("Resolve {}", it.id),
                });
            }
        }
        let (vis_diff, trunc) = truncate_diff_with_limit(diff, max_diff_bytes);
        let hunks = parse_diff_hunks_with_truncation(vis_diff, trunc);
        let planned: BTreeSet<String> = plan
            .map(|p| {
                let f = p.files_to_change.iter().map(|x| x.path.as_str());
                let t = p.red_green_tests.iter().map(|x| x.test_path.as_str());
                f.chain(t)
                    .filter_map(|s| normalize_repo_path(s).ok())
                    .collect()
            })
            .unwrap_or_default();
        let dp: BTreeSet<String> = hunks
            .keys()
            .map(|k| normalize_repo_path(k).unwrap_or_else(|_| k.clone()))
            .collect();
        let format_rejected = |id: &str, raw_loc: &str| -> String {
            let clean = |s: &str| -> String {
                let filtered: String = s
                    .chars()
                    .map(|c| if c.is_control() || c == '`' { ' ' } else { c })
                    .collect();
                neutralise(&filtered.split_whitespace().collect::<Vec<_>>().join(" "))
            };
            let cid = clean(id);
            let cloc = clean(raw_loc);
            let cloc = if cloc.is_empty() { "<empty>" } else { &cloc };
            format!("{cid}({cloc})")
        };
        let reported = blocking.len();
        let mut valid = Vec::new();
        let mut rejected_locations: Vec<String> = Vec::new();
        let mut path_hints: BTreeSet<String> = BTreeSet::new();
        for mut iss in blocking {
            // Prevent reviewer-emitted issues from spoofing engine-reserved non-rebuttable IDs.
            if iss
                .id
                .trim()
                .to_ascii_uppercase()
                .starts_with(SCOPE_HARD_PREFIX)
            {
                iss.id = format!("REV-{}", iss.id.trim());
            }
            let linked = yes_ev
                .get(&iss.id.to_ascii_uppercase())
                .or_else(|| yes_ev.get(&iss.category.to_ascii_uppercase()))
                .map_or("", String::as_str);
            if let Some(c) = find_verified_citation(&iss.location, &hunks) {
                iss.location = format_citation(&c);
                valid.push(iss);
            } else if !linked.is_empty()
                && let Some(c) = find_verified_citation(linked, &hunks)
            {
                iss.location = format_citation(&c);
                valid.push(iss);
            } else if plan.is_some() && is_omission(&iss.id, &iss.category) {
                let hit = find_omission(&iss.location, &planned, &dp)
                    .or_else(|| find_omission(linked, &planned, &dp))
                    .or_else(|| find_omission(&iss.description, &planned, &dp));
                if let Some(norm) = hit {
                    iss.location = format!("{norm}:1");
                    valid.push(iss);
                } else {
                    rejected_locations.push(format_rejected(&iss.id, &iss.location));
                }
            } else {
                for c in extract_citations(&iss.location) {
                    let suffix = format!("/{}", c.path.trim_start_matches("./"));
                    for full in &dp {
                        if full.ends_with(&suffix) {
                            path_hints.insert(full.clone());
                        }
                    }
                }
                rejected_locations.push(format_rejected(&iss.id, &iss.location));
            }
        }
        if reported > 0 && valid.is_empty() {
            if trunc {
                return Err("diff was truncated: unseen changes cannot be approved".into());
            }
            let preview: Vec<String> = rejected_locations
                .into_iter()
                .take(4)
                .map(|s| s.chars().take(100).collect())
                .collect();
            let hint_clause = if path_hints.is_empty() {
                String::new()
            } else {
                let sample: Vec<&str> = path_hints.iter().take(3).map(String::as_str).collect();
                format!(
                    "; use full repo-relative path from diff header (e.g. {}), not bare filename",
                    sample.join(", ")
                )
            };
            return Err(format!(
                "all reported blocking issues failed citation verification \
                 (rejected: {}; cite an exact `file:line` inside a changed diff hunk, \
                 not untouched code outside the diff{hint_clause})",
                preview.join(", ")
            ));
        }
        p2.retain(|i| {
            evidence_in_diff(&i.location, &hunks)
                || (plan.is_some()
                    && is_omission(&i.id, &i.category)
                    && find_omission(&i.location, &planned, &dp).is_some())
        });
        let mech = MechanicalScoreReport {
            score: mechanical_score,
            reasons: vec![],
            same_account_warning: false,
        };
        let (avg, score_fails) = if score_gate {
            let ev = evaluate_score_gate(&mech, &scores, min_avg, min_crit);
            let a = ev.llm_average.ok_or_else(|| ev.reasons.join("; "))?;
            (Some(a), if ev.passed { vec![] } else { ev.reasons })
        } else if !scores.is_empty() {
            (
                evaluate_score_gate(&mech, &scores, 0, 0).llm_average,
                vec![],
            )
        } else {
            (None, vec![])
        };
        if !valid.is_empty() || !score_fails.is_empty() {
            Ok(DualReviewerOutcome::Fail {
                issues: valid,
                scores,
                llm_average: avg,
                score_fail_reasons: score_fails,
            })
        } else if trunc {
            Err("diff was truncated: unseen changes cannot be approved".into())
        } else {
            Ok(DualReviewerOutcome::Pass {
                scores,
                llm_average: avg,
                p2_issues: p2,
            })
        }
    };
    run().unwrap_or_else(|reason| DualReviewerOutcome::Inconclusive { reason })
}
