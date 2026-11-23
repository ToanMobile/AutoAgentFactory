//! Line-based checks on what the change adds.
//!
//! Every pattern below is assembled from fragments, and no comment spells a
//! trigger out, so this file (and any file that quotes a rule the same way)
//! never matches its own rules when the gate runs on this repository.

use std::sync::OnceLock;

use aaf_domain::gate::{Severity, StaticFinding};
use regex::Regex;

use super::GateError;
use super::changes::AddedLines;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StaticReport {
    pub findings: Vec<StaticFinding>,
    /// Findings silenced by an inline allow marker.
    pub suppressions: u32,
}

impl StaticReport {
    pub fn critical(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Critical)
            .count()
    }
}

struct Spec {
    id: &'static str,
    severity: Severity,
    skip_tests: bool,
    /// Skip comment-only lines and documentation files (true for code-syntax
    /// rules like `ERR-001` and `LOG-001`; false for secrets and TODO stubs).
    skip_comments_and_docs: bool,
    message: &'static str,
    fix: &'static str,
    patterns: &'static [&'static [&'static str]],
}

const SPECS: &[Spec] = &[
    Spec {
        id: "SEC-001",
        severity: Severity::Critical,
        skip_tests: false,
        skip_comments_and_docs: false,
        message: "a credential or private key is added in plain text",
        fix: "remove it, rotate the credential and load it from the environment or a secret manager",
        patterns: &[
            &["-----BEGIN (?:[A-Z0-9]+ )*PRIVATE", " KEY-----"],
            &[r"\bAKIA", r"[0-9A-Z]{16}\b"],
            &[r"\bgh[pousr]_", "[A-Za-z0-9]{36}"],
            &["github_pat_", "[A-Za-z0-9_]{22,}"],
            &["sk-ant-", "[A-Za-z0-9_-]{20,}"],
            &[r"\bxox[bp]-", "[A-Za-z0-9-]{10,}"],
            &[r"\bAIza", "[0-9A-Za-z_-]{35}"],
        ],
    },
    Spec {
        id: "SEC-002",
        severity: Severity::Warn,
        skip_tests: false,
        skip_comments_and_docs: false,
        message: "a password or token is assigned a string literal",
        fix: "read the value from the environment or a secret manager",
        patterns: &[&[
            "(?i)(?:password|passwd|secret|token|api[_-]?key)",
            r#"[a-z0-9_]*["']?\s*[:=]\s*["'][^"'\s]{4,}["']"#,
        ]],
    },
    Spec {
        id: "LAZY-001",
        severity: Severity::Critical,
        skip_tests: false,
        skip_comments_and_docs: false,
        message: "placeholder left where real code is expected",
        fix: "write the implementation or remove the stub",
        patterns: &[
            &[r"(?i)\.{3}\s*existing", r" code\s*\.{3}"],
            &[r"\bto", r"do!\s*\("],
            &[r"\bunimpl", r"emented!\s*\("],
            &[r"(?i)\bTODO:?\s+impl", "ement"],
            &[r"\bNotImplemented", r"Error\b"],
        ],
    },
    Spec {
        id: "ERR-001",
        severity: Severity::Critical,
        skip_tests: false,
        skip_comments_and_docs: true,
        message: "an error is caught and silently discarded",
        fix: "log it with context, handle it or rethrow",
        patterns: &[
            &[r"\bcatch\s*(?:\([^)]*\))?\s*\{", r"\s*\}"],
            &[r"\bexcept\b[^:]*:", r"\s*pass\b"],
        ],
    },
    Spec {
        id: "LOG-001",
        severity: Severity::Warn,
        skip_tests: true,
        skip_comments_and_docs: true,
        message: "raw debug output in production code",
        fix: "use the project's structured logger or remove the statement",
        patterns: &[
            &[r"\bconsole\.", r"log\s*\("],
            &[r"\bSystem\.out\.", r"println\s*\("],
            &[r"\.printStack", r"Trace\s*\("],
            &[r"\bdb", r"g!\s*\("],
        ],
    },
];

/// Index of the swallowed-error rule in [`SPECS`], for the two-line form.
const ERR_RULE: usize = 3;
/// (opener on one added line, closer on the next) for an empty handler.
const TWO_LINE: &[(&[&str], &[&str])] = &[
    (&[r"\bcatch\s*(?:\([^)]*\))?", r"\s*\{\s*$"], &[r"^\s*\}"]),
    (
        &[r"^\s*except\b[^:]*:", r"\s*$"],
        &[r"^\s*pass\s*", "(?:#.*)?$"],
    ),
];
const ALLOW: &[&str] = &["aaf:al", r"low\(([A-Z]+-[0-9]{3})\)\s+\S"];

const GENERATED_DIRS: &[&str] = &["target", "build", "node_modules", "dist", ".gradle", "Pods"];
const LOCKFILES: &[&str] = &[
    "package-lock.json",
    "pnpm-lock.yaml",
    "npm-shrinkwrap.json",
    "go.sum",
    "gradle.lockfile",
    "Package.resolved",
];
const TEST_DIRS: &[&str] = &["tests", "test", "__tests__", "androidTest", "spec", "e2e"];

struct Rules {
    rules: Vec<Vec<Regex>>,
    two_line: Vec<(Regex, Regex)>,
    allow: Regex,
}

fn compile(parts: &[&str]) -> Result<Regex, String> {
    Regex::new(&parts.concat()).map_err(|err| err.to_string())
}

fn rules() -> Result<&'static Rules, GateError> {
    static RULES: OnceLock<Result<Rules, String>> = OnceLock::new();
    RULES
        .get_or_init(|| {
            Ok(Rules {
                rules: SPECS
                    .iter()
                    .map(|spec| spec.patterns.iter().map(|p| compile(p)).collect())
                    .collect::<Result<_, _>>()?,
                two_line: TWO_LINE
                    .iter()
                    .map(|(open, close)| Ok((compile(open)?, compile(close)?)))
                    .collect::<Result<_, String>>()?,
                allow: compile(ALLOW)?,
            })
        })
        .as_ref()
        // A rule that does not compile must stop the gate, not vanish.
        .map_err(|err| GateError::Config(format!("static rule does not compile: {err}")))
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn dirs(path: &str) -> impl Iterator<Item = &str> {
    let mut parts: Vec<&str> = path.split('/').collect();
    parts.pop();
    parts.into_iter()
}

/// Lockfiles and generated directories are never scanned.
pub fn skip_path(path: &str) -> bool {
    let name = file_name(path);
    dirs(path).any(|dir| GENERATED_DIRS.contains(&dir))
        || name.ends_with(".lock")
        || LOCKFILES.contains(&name)
}

pub fn is_test_path(path: &str) -> bool {
    let name = file_name(path);
    let stem = name.split('.').next().unwrap_or(name);
    dirs(path).any(|dir| TEST_DIRS.contains(&dir))
        || name.starts_with("test_")
        || ["_test.", ".test.", ".spec."]
            .iter()
            .any(|mark| name.contains(mark))
        || ["Test", "Tests", "Spec"]
            .iter()
            .any(|end| stem.ends_with(end))
}

fn is_comment_line(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("//")
        || trimmed.starts_with("/*")
        || trimmed == "*"
        || trimmed.starts_with("* ")
        || trimmed.starts_with("*\t")
        || trimmed.starts_with("*/")
        || (trimmed.starts_with('#') && !trimmed.starts_with("#[") && !trimmed.starts_with("#!["))
        || trimmed.starts_with("-- ")
        || trimmed == "--"
}

fn is_doc_path(path: &str) -> bool {
    let name = file_name(path);
    name.rsplit_once('.').is_some_and(|(_, ext)| {
        matches!(
            ext.to_ascii_lowercase().as_str(),
            "md" | "mdx" | "rst" | "txt" | "adoc"
        )
    })
}

fn allowed(rules: &Rules, text: &str, id: &str) -> bool {
    rules
        .allow
        .captures_iter(text)
        .any(|caps| caps.get(1).is_some_and(|m| m.as_str() == id))
}

/// Scans added lines. A line carrying the allow marker for a rule, followed
/// by a reason, silences that rule on that line and is counted.
pub fn scan(files: &[AddedLines]) -> Result<StaticReport, GateError> {
    let rules = rules()?;
    let mut report = StaticReport::default();
    for file in files.iter().filter(|file| !skip_path(&file.path)) {
        let is_test = is_test_path(&file.path);
        let is_doc = is_doc_path(&file.path);
        let mut previous: Option<(u32, &str)> = None;
        for (number, text) in &file.lines {
            let is_comment = is_comment_line(text);
            let mut hits: Vec<(usize, u32, bool)> = Vec::new();
            for (index, patterns) in rules.rules.iter().enumerate() {
                if SPECS[index].skip_tests && is_test {
                    continue;
                }
                if SPECS[index].skip_comments_and_docs && (is_comment || is_doc) {
                    continue;
                }
                if patterns.iter().any(|pattern| pattern.is_match(text)) {
                    hits.push((index, *number, allowed(rules, text, SPECS[index].id)));
                }
            }
            if !is_doc
                && !is_comment
                && let Some((prev_number, prev_text)) = previous
                && !is_comment_line(prev_text)
                && prev_number.checked_add(1) == Some(*number)
                && rules
                    .two_line
                    .iter()
                    .any(|(open, close)| open.is_match(prev_text) && close.is_match(text))
            {
                let id = SPECS[ERR_RULE].id;
                let silenced = allowed(rules, prev_text, id) || allowed(rules, text, id);
                hits.push((ERR_RULE, prev_number, silenced));
            }
            for (index, line, silenced) in hits {
                if silenced {
                    report.suppressions = report.suppressions.saturating_add(1);
                    continue;
                }
                let spec = &SPECS[index];
                // The matched text is left out on purpose: a receipt must
                // not carry the secret it reports.
                report.findings.push(StaticFinding {
                    rule: spec.id.to_string(),
                    severity: spec.severity,
                    file: file.path.clone(),
                    line: Some(line),
                    message: spec.message.to_string(),
                    fix: spec.fix.to_string(),
                });
            }
            previous = Some((*number, text.as_str()));
        }
    }
    Ok(report)
}
