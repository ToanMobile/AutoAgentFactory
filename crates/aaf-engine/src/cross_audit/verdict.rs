//! Strict, fail-closed parsing of the auditor's answer.

use aaf_domain::gate::GoldenScore;
use serde::Deserialize;

use super::checklist::Item;
use super::scoring::GOLDEN_IDS;

/// A checklist item the auditor answered "yes".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub id: String,
    pub question: String,
    /// The auditor's evidence; may be empty (a bare "yes" still counts).
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

/// Extracts the single JSON object of `text`. Any brace outside that object
/// (a second object, a broken fragment) is rejected rather than guessed at,
/// which also covers one object wrapped in a markdown fence.
fn single_object(text: &str) -> Result<RawAnswers, String> {
    let start = text.find('{').ok_or("no JSON object in the answer")?;
    let mut stream = serde_json::Deserializer::from_str(&text[start..]).into_iter::<RawAnswers>();
    let parsed = match stream.next() {
        Some(Ok(parsed)) => parsed,
        Some(Err(err)) => return Err(format!("answer is not the required JSON: {err}")),
        None => return Err("no JSON object in the answer".to_string()),
    };
    let rest = &text[start + stream.byte_offset()..];
    if rest.contains(['{', '}']) {
        return Err("answer contains more than one JSON object".to_string());
    }
    Ok(parsed)
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
    for (item, answer) in checklist.iter().zip(answers) {
        let Some((yes, evidence)) = answer else {
            return scored_inconclusive(format!("checklist id {} not answered", item.id));
        };
        if yes {
            let finding = Finding {
                id: item.id.clone(),
                question: item.question.clone(),
                evidence,
            };
            if item.blocking {
                findings.push(finding);
            } else {
                warnings.push(finding);
            }
        }
    }
    let verdict = if findings.is_empty() {
        AuditVerdict::Pass { warnings }
    } else {
        AuditVerdict::Fail { findings, warnings }
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
