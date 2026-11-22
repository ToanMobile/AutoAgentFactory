//! Strict, fail-closed parsing of the auditor's answer.

use serde::Deserialize;

use super::checklist::Item;

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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAnswers {
    answers: Vec<RawAnswer>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAnswer {
    id: String,
    answer: String,
    #[serde(default)]
    evidence: String,
}

fn inconclusive(reason: impl Into<String>) -> AuditVerdict {
    AuditVerdict::Inconclusive {
        reason: reason.into(),
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

/// Turns the auditor's raw text into a verdict. `Pass` requires exactly one
/// parsable JSON object, every checklist id answered exactly once with "yes"
/// or "no", no unknown id, and "no" on every blocking item.
pub fn parse_verdict(text: &str, checklist: &[Item]) -> AuditVerdict {
    if !checklist.iter().any(|item| item.blocking) {
        return inconclusive("checklist has no blocking item");
    }
    let raw = match single_object(text) {
        Ok(raw) => raw,
        Err(reason) => return inconclusive(reason),
    };

    // The checklist is a dozen items: a linear id lookup is enough.
    let mut answers: Vec<Option<(bool, String)>> = vec![None; checklist.len()];
    for answer in raw.answers {
        let Some(index) = checklist.iter().position(|item| item.id == answer.id) else {
            return inconclusive(format!("unknown checklist id {:?}", answer.id));
        };
        let yes = match answer.answer.as_str() {
            "yes" => true,
            "no" => false,
            other => return inconclusive(format!("{}: answer {other:?} is not yes/no", answer.id)),
        };
        if answers[index].is_some() {
            return inconclusive(format!("checklist id {} answered twice", answer.id));
        }
        answers[index] = Some((yes, answer.evidence));
    }

    let mut findings = Vec::new();
    let mut warnings = Vec::new();
    for (item, answer) in checklist.iter().zip(answers) {
        let Some((yes, evidence)) = answer else {
            return inconclusive(format!("checklist id {} not answered", item.id));
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
    if findings.is_empty() {
        AuditVerdict::Pass { warnings }
    } else {
        AuditVerdict::Fail { findings, warnings }
    }
}
