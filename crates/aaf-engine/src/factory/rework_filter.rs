//! Filtering of unverified / evidenceless cross-audit findings before they
//! reach author-facing rework notes or rejection summaries.

use aaf_domain::pipeline::ReviewIssue;

use crate::cross_audit::{AuditVerdict, Finding, extract_citations, has_file_line_reference};

const LEAD_WRAP: &[char] = &['(', '[', '{', '`', '\'', '"', '<'];
const TRAIL_WRAP: &[char] = &[')', ']', '}', ',', ';', '`', '\'', '"', '>', '.'];

/// Well-known extensionless root files that may appear in `file:line` citations.
const EXTENSIONLESS_FILES: &[&str] = &[
    "Makefile",
    "GNUmakefile",
    "Dockerfile",
    "Containerfile",
    "Jenkinsfile",
    "Justfile",
    "justfile",
    "Procfile",
    "Gemfile",
    "Rakefile",
    "Podfile",
    "gradlew",
    "BUILD",
    "WORKSPACE",
    "LICENSE",
    "README",
    "CHANGELOG",
];

fn is_domain_tld(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "com"
            | "net"
            | "org"
            | "edu"
            | "gov"
            | "mil"
            | "int"
            | "local"
            | "localdomain"
            | "invalid"
            | "localhost"
    )
}

pub(crate) fn is_score_finding(id: &str) -> bool {
    id == "score_gate"
        || id
            .strip_prefix('G')
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

fn is_valid_path_token(path: &str) -> bool {
    if path.is_empty()
        || !path.bytes().any(|b| b.is_ascii_alphabetic())
        || path.ends_with('/')
        || path.ends_with('.')
    {
        return false;
    }
    if path.contains('/') || EXTENSIONLESS_FILES.contains(&path) {
        return true;
    }
    let Some((_, ext)) = path.rsplit_once('.') else {
        return false;
    };
    !ext.is_empty() && ext.bytes().any(|b| b.is_ascii_alphabetic()) && !is_domain_tld(ext)
}

/// True when `evidence` cites at least one concrete `path:line` token
/// (excluding URLs, `host.tld:port`, timestamps like `12:30`, bare words like
/// `note:1`, version strings like `v1.2:3`, or `:0`).
///
/// Unifies whitespace-token checks with [`crate::cross_audit::has_file_line_reference`]
/// so normalized `<path>:1` omission citations, `#L` anchors, and quoted paths
/// with spaces pass cleanly to Implementer rework (R3-PROTO-3).
pub(crate) fn has_file_line_ref(evidence: &str) -> bool {
    let token_match = evidence.split_whitespace().any(|raw| {
        let token = raw
            .trim_start_matches(LEAD_WRAP)
            .trim_end_matches(TRAIL_WRAP);
        if token.contains("://") {
            return false;
        }
        let Some((path, rest)) = token.split_once(':') else {
            return false;
        };
        let line_ok = rest.split([':', '-']).next().is_some_and(|n| {
            !n.is_empty()
                && n.bytes().all(|b| b.is_ascii_digit())
                && n.parse::<u32>().is_ok_and(|v| v >= 1)
        });
        is_valid_path_token(path) && line_ok
    });
    if token_match {
        return true;
    }
    has_file_line_reference(evidence)
        && extract_citations(evidence)
            .iter()
            .any(|c| is_valid_path_token(&c.path))
}

pub fn is_valid_rework_finding(finding: &Finding) -> bool {
    let evidence = finding.evidence.trim();
    !evidence.is_empty() && (is_score_finding(&finding.id) || has_file_line_ref(evidence))
}

/// Converts validated dual-review [`ReviewIssue`] findings (including normalized
/// `<path>:1` omission locations) into [`Finding`] records for rework notes.
pub fn findings_from_review_issues(issues: &[ReviewIssue]) -> Vec<Finding> {
    issues
        .iter()
        .map(|issue| {
            let (loc, desc, fix) = (
                issue.location.trim(),
                issue.description.trim(),
                issue.proposed_fix.trim(),
            );
            let evidence = match (loc.is_empty(), desc.is_empty(), fix.is_empty()) {
                (false, false, false) => format!("{loc} — {desc} (fix: {fix})"),
                (false, false, true) => format!("{loc} — {desc}"),
                (false, true, false) => format!("{loc} (fix: {fix})"),
                (false, true, true) => loc.to_string(),
                (true, false, _) => desc.to_string(),
                (true, true, _) => String::new(),
            };
            Finding {
                id: issue.id.clone(),
                question: format!("[{}] {}", issue.severity.as_str(), issue.category),
                evidence,
            }
        })
        .collect()
}

/// Formats a fallback rejection summary for author-facing `context_notes` when
/// `handle_audit_failure` returns `Ok(None)`. Raw unverified `Fail` findings
/// are stripped so they never leak into the author's prompt.
pub(crate) fn fallback_verdict_summary(verdict: &AuditVerdict, default_text: &str) -> String {
    let AuditVerdict::Fail { findings, .. } = verdict else {
        return default_text.to_string();
    };
    let valid: Vec<String> = findings
        .iter()
        .filter(|f| is_valid_rework_finding(f))
        .map(|f| format!("{} ({})", f.id, f.evidence.trim()))
        .collect();
    if valid.is_empty() {
        "unverified findings dropped".to_string()
    } else {
        format!("fail: {}", valid.join("; "))
    }
}
