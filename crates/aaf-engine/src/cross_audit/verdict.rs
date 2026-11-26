//! Strict, fail-closed parsing of the auditor's answer.

use std::borrow::Cow;

use aaf_domain::gate::GoldenScore;
use serde::{Deserialize, Serialize};

use super::checklist::Item;
use super::diff_hunks::{DiffHunks, evidence_in_diff, has_file_line_reference};
use super::scoring::GOLDEN_IDS;

/// A checklist item the auditor answered "yes".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    pub question: String,
    /// Concrete `file:line` evidence from the diff supporting the finding.
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditVerdict {
    /// Every blocking item was answered "no". Advisory "yes" answers ride
    /// along as warnings.
    Pass { warnings: Vec<Finding> },
    /// A well-formed answer with at least one blocking "yes".
    Fail {
        findings: Vec<Finding>,
        warnings: Vec<Finding>,
    },
    /// No usable answer: the audit must be repeated, never treated as a pass.
    Inconclusive { reason: String },
}

impl AuditVerdict {
    pub fn is_pass(&self) -> bool {
        matches!(self, AuditVerdict::Pass { .. })
    }
}

/// Result of parsing both the binary checklist and optional/required 7 Golden
/// Standards scores from an auditor response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoredParse {
    pub verdict: AuditVerdict,
    pub scores: Vec<GoldenScore>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAnswers {
    answers: Vec<RawAnswer>,
    #[serde(default)]
    scores: Option<Vec<RawGoldenScore>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAnswer {
    id: String,
    answer: String,
    #[serde(default)]
    evidence: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGoldenScore {
    id: String,
    score: u8,
    #[serde(default)]
    deduction_reason: String,
}

fn inconclusive(reason: impl Into<String>) -> AuditVerdict {
    AuditVerdict::Inconclusive {
        reason: reason.into(),
    }
}

fn scored_inconclusive(reason: impl Into<String>) -> ScoredParse {
    ScoredParse {
        verdict: inconclusive(reason),
        scores: Vec::new(),
    }
}

const MAX_AUDIT_JSON_SCAN_BYTES: usize = 512 * 1024;
const MAX_AUDIT_JSON_CANDIDATES: usize = 4096;

/// Returns the byte offset just past the matching `}` for the `{` at `start`,
/// respecting JSON string quoting and backslash escapes.
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

/// Strips ANSI CSI (`\x1b\[[0-9;?]*[ -/]*[@-~]`) and OSC
/// (`\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)`) escape sequences from PTY output.
pub fn strip_ansi_escapes(input: &str) -> Cow<'_, str> {
    if !input.as_bytes().contains(&0x1b) {
        return Cow::Borrowed(input);
    }
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let mut seg_start = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            i += 1;
            continue;
        }
        if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            let mut j = i + 2;
            while j < bytes.len() && (bytes[j].is_ascii_digit() || matches!(bytes[j], b';' | b'?'))
            {
                j += 1;
            }
            while j < bytes.len() && (0x20..=0x2f).contains(&bytes[j]) {
                j += 1;
            }
            if j < bytes.len() && (0x40..=0x7e).contains(&bytes[j]) {
                out.push_str(&input[seg_start..i]);
                i = j + 1;
                seg_start = i;
                continue;
            }
        } else if i + 1 < bytes.len() && bytes[i + 1] == b']' {
            let mut j = i + 2;
            while j < bytes.len() && bytes[j] != 0x07 && bytes[j] != 0x1b {
                j += 1;
            }
            let end = if j < bytes.len() && bytes[j] == 0x07 {
                Some(j + 1)
            } else if j + 1 < bytes.len() && bytes[j] == 0x1b && bytes[j + 1] == b'\\' {
                Some(j + 2)
            } else {
                None
            };
            if let Some(next_i) = end {
                out.push_str(&input[seg_start..i]);
                i = next_i;
                seg_start = i;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&input[seg_start..]);
    Cow::Owned(out)
}

/// Extracts the single valid `RawAnswers` JSON object from `text`, tolerating
/// surrounding non-audit CLI logs, ANSI escapes, banners, braces, or trailing
/// `[aaf]` lines. Fails closed if zero or more than one valid `RawAnswers`
/// object is found.
fn extract_answers_json(text: &str) -> Result<RawAnswers, String> {
    if text.len() > MAX_AUDIT_JSON_SCAN_BYTES {
        return Err(format!(
            "audit output exceeds {MAX_AUDIT_JSON_SCAN_BYTES} byte scan limit"
        ));
    }
    let cleaned = strip_ansi_escapes(text);
    let text = cleaned.as_ref();
    let bytes = text.as_bytes();
    let mut found: Option<RawAnswers> = None;
    let mut candidates = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let Some(rel) = bytes[i..].iter().position(|&b| b == b'{') else {
            break;
        };
        let start = i + rel;
        let next_non_ws = bytes[start + 1..]
            .iter()
            .copied()
            .find(|b| !b.is_ascii_whitespace());
        if next_non_ws != Some(b'"') {
            i = start + 1;
            continue;
        }
        candidates += 1;
        if candidates > MAX_AUDIT_JSON_CANDIDATES {
            return Err("audit output contains too many JSON object candidates".to_string());
        }
        let Some(end) = balanced_json_object_end(bytes, start) else {
            i = start + 1;
            continue;
        };
        let slice = &text[start..end];
        match serde_json::from_str::<RawAnswers>(slice) {
            Ok(parsed) => {
                if found.is_some() {
                    return Err("answer contains more than one audit JSON object".to_string());
                }
                found = Some(parsed);
                i = end;
            }
            Err(_) => {
                if serde_json::from_str::<serde_json::Value>(slice).is_ok() {
                    i = end;
                } else {
                    i = start + 1;
                }
            }
        }
    }
    found.ok_or_else(|| "no valid audit JSON object found".to_string())
}

/// Extracts the single audit JSON object of `text` (bounded, fail-closed).
fn single_object(text: &str) -> Result<RawAnswers, String> {
    extract_answers_json(text)
}

fn parse_golden_scores(raw_scores: Vec<RawGoldenScore>) -> Result<Vec<GoldenScore>, String> {
    let mut ordered: [Option<GoldenScore>; GOLDEN_IDS.len()] = std::array::from_fn(|_| None);
    for entry in raw_scores {
        let Some(index) = GOLDEN_IDS.iter().position(|id| entry.id == *id) else {
            return Err(format!("unknown golden standard id {:?}", entry.id));
        };
        if ordered[index].is_some() {
            return Err(format!("golden standard id {} scored twice", entry.id));
        }
        if entry.score > 100 {
            return Err(format!(
                "golden standard {} score {} exceeds 100",
                entry.id, entry.score
            ));
        }
        if entry.score < 100 && entry.deduction_reason.trim().is_empty() {
            return Err(format!(
                "golden standard {} score {} (< 100) requires a non-empty deduction_reason",
                entry.id, entry.score
            ));
        }
        ordered[index] = Some(GoldenScore {
            id: entry.id,
            score: entry.score,
            deduction_reason: entry.deduction_reason,
        });
    }

    let mut canonical = Vec::with_capacity(GOLDEN_IDS.len());
    for (id, slot) in GOLDEN_IDS.iter().zip(ordered) {
        let Some(score) = slot else {
            return Err(format!("missing golden standard id {id}"));
        };
        canonical.push(score);
    }
    Ok(canonical)
}

/// Turns the auditor's raw text into a verdict and validated 7 Golden Standards
/// scores (ordered canonically `G1..G7`).
///
/// When `require_scores` is `true`, `scores` MUST be present and cover `G1..G7`
/// exactly once each with `score <= 100` and non-empty `deduction_reason` for
/// any score `< 100`. When `require_scores` is `false`, an absent `scores`
/// field yields `Vec::new()`, while a present `scores` field is still validated
/// strictly (fail closed as `Inconclusive` if malformed).
pub fn parse_scored_verdict(text: &str, checklist: &[Item], require_scores: bool) -> ScoredParse {
    if !checklist.iter().any(|item| item.blocking) {
        return scored_inconclusive("checklist has no blocking item");
    }
    let raw = match single_object(text) {
        Ok(raw) => raw,
        Err(reason) => return scored_inconclusive(reason),
    };

    // The checklist is a dozen items: a linear id lookup is enough.
    let mut answers: Vec<Option<(bool, String)>> = vec![None; checklist.len()];
    for answer in raw.answers {
        let Some(index) = checklist.iter().position(|item| item.id == answer.id) else {
            return scored_inconclusive(format!("unknown checklist id {:?}", answer.id));
        };
        let yes = match answer.answer.as_str() {
            "yes" => true,
            "no" => false,
            other => {
                return scored_inconclusive(format!(
                    "{}: answer {other:?} is not yes/no",
                    answer.id
                ));
            }
        };
        if answers[index].is_some() {
            return scored_inconclusive(format!("checklist id {} answered twice", answer.id));
        }
        answers[index] = Some((yes, answer.evidence));
    }

    let mut findings = Vec::new();
    let mut warnings = Vec::new();
    let mut unbacked_blocking = Vec::new();
    for (item, answer) in checklist.iter().zip(answers) {
        let Some((yes, evidence)) = answer else {
            return scored_inconclusive(format!("checklist id {} not answered", item.id));
        };
        if yes {
            let valid = has_file_line_reference(&evidence);
            if item.blocking {
                if valid {
                    findings.push(Finding {
                        id: item.id.clone(),
                        question: item.question.clone(),
                        evidence,
                    });
                } else {
                    unbacked_blocking.push(item.id.clone());
                }
            } else if valid {
                warnings.push(Finding {
                    id: item.id.clone(),
                    question: item.question.clone(),
                    evidence,
                });
            }
        }
    }
    let verdict = if !findings.is_empty() {
        AuditVerdict::Fail { findings, warnings }
    } else if !unbacked_blocking.is_empty() {
        return scored_inconclusive(format!(
            "blocking finding(s) {} lack concrete file:line evidence",
            unbacked_blocking.join(", ")
        ));
    } else {
        AuditVerdict::Pass { warnings }
    };

    let scores = match raw.scores {
        Some(raw_scores) => match parse_golden_scores(raw_scores) {
            Ok(scores) => scores,
            Err(reason) => return scored_inconclusive(reason),
        },
        None if require_scores => {
            return scored_inconclusive("missing required golden standards scores (G1..G7)");
        }
        None => Vec::new(),
    };

    ScoredParse { verdict, scores }
}

/// Turns the auditor's raw text into a verdict. `Pass` requires exactly one
/// parsable JSON object, every checklist id answered exactly once with "yes"
/// or "no", no unknown id, and "no" on every blocking item.
pub fn parse_verdict(text: &str, checklist: &[Item]) -> AuditVerdict {
    parse_scored_verdict(text, checklist, false).verdict
}

/// Whether `text` holds a readable audit answer that said "yes" to any
/// blocking checklist item, cited or not. A format repair of such a reply may
/// cite the blocker but never turn it into a pass.
pub fn names_blocking_yes(text: &str, checklist: &[Item]) -> bool {
    single_object(text).is_ok_and(|raw| {
        raw.answers.iter().any(|answer| {
            answer.answer == "yes"
                && checklist
                    .iter()
                    .any(|item| item.blocking && item.id == answer.id)
        })
    })
}

/// Validates every finding's `file:line` citation against the parsed diff hunks.
///
/// Blocking findings whose citations do not match any hunk are dropped; if no
/// valid blocking findings remain, the verdict becomes `Inconclusive`. Advisory
/// warnings with out-of-diff citations are dropped without turning `Pass`
/// inconclusive.
pub fn validate_verdict_against_diff(parsed: ScoredParse, hunks: &DiffHunks) -> ScoredParse {
    let ScoredParse { verdict, scores } = parsed;
    match verdict {
        AuditVerdict::Fail { findings, warnings } => {
            let valid_findings: Vec<Finding> = findings
                .into_iter()
                .filter(|f| evidence_in_diff(&f.evidence, hunks))
                .collect();
            let valid_warnings: Vec<Finding> = warnings
                .into_iter()
                .filter(|w| evidence_in_diff(&w.evidence, hunks))
                .collect();
            if valid_findings.is_empty() {
                scored_inconclusive("blocking finding(s) cite file:line not present in the diff")
            } else {
                ScoredParse {
                    verdict: AuditVerdict::Fail {
                        findings: valid_findings,
                        warnings: valid_warnings,
                    },
                    scores,
                }
            }
        }
        AuditVerdict::Pass { warnings } => {
            let valid_warnings: Vec<Finding> = warnings
                .into_iter()
                .filter(|w| evidence_in_diff(&w.evidence, hunks))
                .collect();
            ScoredParse {
                verdict: AuditVerdict::Pass {
                    warnings: valid_warnings,
                },
                scores,
            }
        }
        AuditVerdict::Inconclusive { reason } => scored_inconclusive(reason),
    }
}
