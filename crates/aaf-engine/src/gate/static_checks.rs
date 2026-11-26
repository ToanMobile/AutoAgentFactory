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

mod static_patch;

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
    only_tests: bool,
    extensions: Option<&'static [&'static str]>,
    /// Skip comment-only lines and documentation files (true for code-syntax
    /// rules like `ERR-001`, `LOG-001`, and `LAZY-001` code stubs; false for
    /// secrets and prose placeholders).
    skip_comments_and_docs: bool,
    message: &'static str,
    fix: &'static str,
    patterns: &'static [&'static [&'static str]],
}

const TEST_SKIP_MSG: &str = "a test is disabled or skipped in source control";
const TEST_SKIP_FIX: &str = "fix the test or remove the skip marker";

const fn test_skip(
    only_tests: bool,
    extensions: &'static [&'static str],
    patterns: &'static [&'static [&'static str]],
) -> Spec {
    Spec {
        id: "TEST-SKIP",
        severity: Severity::Warn,
        skip_tests: false,
        only_tests,
        extensions: Some(extensions),
        skip_comments_and_docs: false,
        message: TEST_SKIP_MSG,
        fix: TEST_SKIP_FIX,
        patterns,
    }
}

const SPECS: &[Spec] = &[
    Spec {
        id: "SEC-001",
        severity: Severity::Critical,
        skip_tests: false,
        only_tests: false,
        extensions: None,
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
        only_tests: false,
        extensions: None,
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
        only_tests: false,
        extensions: None,
        skip_comments_and_docs: false,
        message: "placeholder left where real code is expected",
        fix: "write the implementation or remove the stub",
        patterns: &[
            &[r"(?i)\.{3}\s*existing", r" code\s*\.{3}"],
            &[r"(?i)\bTODO:?\s+impl", "ement"],
        ],
    },
    Spec {
        id: "LAZY-001",
        severity: Severity::Critical,
        skip_tests: false,
        only_tests: false,
        extensions: None,
        skip_comments_and_docs: true,
        message: "placeholder left where real code is expected",
        fix: "write the implementation or remove the stub",
        patterns: &[
            &[r"\bto", r"do!\s*\("],
            &[r"\bunimpl", r"emented!\s*\("],
            &[r"\bNotImplemented", r"Error\b"],
        ],
    },
    Spec {
        id: "ERR-001",
        severity: Severity::Critical,
        skip_tests: false,
        only_tests: false,
        extensions: None,
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
        only_tests: false,
        extensions: None,
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
    // Rust inline `#[cfg(test)]` modules live in ordinary `.rs` files.
    test_skip(
        false,
        &["rs"],
        &[
            &[r"#\[\s*ig", r"nore\b"],
            &[r"#\[\s*(?:test|cfg_attr)\b[^\]]*\big", r"nore\b"],
        ],
    ),
    test_skip(
        true,
        &["js", "ts", "jsx", "tsx", "mjs", "cjs"],
        &[
            &[r"\b(?:xd", r"escribe|xit|xtest|fit|fd", r"escribe)\s*\("],
            &[
                r"\b(?:test|it|describe|context|suite)(?:\.concurrent)?\.",
                r"(?:sk",
                r"ip|only|fixme|to",
                r"do)\s*(?:\(|\.)",
            ],
        ],
    ),
    test_skip(
        true,
        &["py"],
        &[
            &[
                r"(?:^|\W)(?:@?pytest\.mark\.(?:sk",
                r"ip|skipif|xfail)|@unittest\.sk",
                r"ip(?:If|Unless)?)\b",
            ],
            &[
                r"\b(?:pytest\.sk",
                r"ip|self\.skipTest|unittest\.Sk",
                r"ipTest)\s*\(",
            ],
        ],
    ),
    // Scoped to test paths so Room annotations on production Kotlin/Java models stay clean.
    test_skip(
        true,
        &["kt", "java"],
        &[&[
            r"@(?:org\.junit\.(?:jupiter\.api\.)?)?(?:Ig",
            r"nore|Dis",
            r"abled(?:If)?)\b",
        ]],
    ),
    test_skip(true, &["go"], &[&[r"\b[tb]\.Sk", r"ip(?:f|Now)?\s*\("]]),
    test_skip(true, &["dart"], &[&[r"\bsk", r#"ip\s*:\s*(?:true|['"])"#]]),
    test_skip(
        true,
        &["cs"],
        &[
            &[r"\[\s*(?:[^\]]*,\s*)?Ig", r"nore\b"],
            &[r"\[\s*(?:Fact|Theory)\s*\([^)]*\bSk", r"ip\s*="],
        ],
    ),
    test_skip(true, &["swift"], &[&[r"\bXCTSk", r"ip(?:If|Unless)?\s*\("]]),
];

/// Index of the code-syntax `LAZY-001` rule in [`SPECS`].
const LAZY_CODE_RULE: usize = 3;
/// Pattern index inside [`LAZY_CODE_RULE`] for the unimplemented-exception stub.
const LAZY_NOT_IMPL_PATTERN: usize = 2;
/// Index of the swallowed-error rule in [`SPECS`], for the two-line form.
const ERR_RULE: usize = 4;
/// Test-assertion wrappers that legitimately name the unimplemented exception.
const TEST_NOT_IMPL_ASSERT: &[&str] = &[
    r"\b(?:pytest\.raises|assertRaises(?:Regex|Regexp)?|unittest\.TestCase\.assertRaises)\s*\(\s*\(?\s*",
    r"NotImplemented",
    r"Error\b",
];
/// (opener on one added line, closer on the next) for an empty handler.
const TWO_LINE: &[(&[&str], &[&str])] = &[
    (&[r"\bcatch\s*(?:\([^)]*\))?", r"\s*\{\s*$"], &[r"^\s*\}"]),
    (
        &[r"^\s*except\b[^:]*:", r"\s*$"],
        &[r"^\s*pass\s*", "(?:#.*)?$"],
    ),
];
const ALLOW: &[&str] = &["aaf:al", r"low\(([A-Z]+-(?:[0-9]{3}|[A-Z0-9]+))\)\s+\S"];

const GENERATED_DIRS: &[&str] = &["target", "build", "node_modules", "dist", ".gradle", "Pods"];
const LOCKFILES: &[&str] = &[
    "package-lock.json",
    "pnpm-lock.yaml",
    "npm-shrinkwrap.json",
    "go.sum",
    "gradle.lockfile",
    "Package.resolved",
];
const TEST_DIRS: &[&str] = &[
    "tests",
    "test",
    "__tests__",
    "androidTest",
    "integrationTest",
    "unit_tests",
    "spec",
    "e2e",
];

struct Rules {
    rules: Vec<Vec<Regex>>,
    two_line: Vec<(Regex, Regex)>,
    allow: Regex,
    test_not_impl_assert: Regex,
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
                test_not_impl_assert: compile(TEST_NOT_IMPL_ASSERT)?,
            })
        })
        .as_ref()
        // A rule that does not compile must stop the gate, not vanish.
        .map_err(|err| GateError::Config(format!("static rule does not compile: {err}")))
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn extension(path: &str) -> &str {
    file_name(path).rsplit_once('.').map_or("", |(_, ext)| ext)
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
    let ext = extension(path);
    dirs(path).any(|dir| TEST_DIRS.contains(&dir))
        || name.starts_with("test_")
        || ["_test.", "_tests.", ".test.", ".tests.", ".spec."]
            .iter()
            .any(|mark| name.contains(mark))
        || ["Test", "Tests", "Spec"]
            .iter()
            .any(|end| stem.ends_with(end))
        || (stem.starts_with("Test") && matches!(ext, "java" | "kt"))
        || matches!(stem, "test" | "tests")
}

/// Returns `true` when `line` contains an inline suppression marker with a
/// rule id and a non-empty reason.
pub fn contains_allow_marker(line: &str) -> Result<bool, GateError> {
    Ok(rules()?.allow.is_match(line))
}

fn is_pure_c_comment(mut s: &str) -> bool {
    loop {
        s = s.trim_start();
        if s.is_empty() || s.starts_with("//") {
            return true;
        }
        if let Some(rest) = s.strip_prefix("/*") {
            let Some((_, after)) = rest.split_once("*/") else {
                return true;
            };
            s = after;
        } else {
            return false;
        }
    }
}

const HASH_COMMENT_FILES: &[&str] = &[
    "makefile",
    "gnumakefile",
    "dockerfile",
    "containerfile",
    "gemfile",
    "rakefile",
    "podfile",
    "fastfile",
    "justfile",
    ".gitignore",
    ".gitattributes",
    ".dockerignore",
    ".env",
    ".bashrc",
    ".zshrc",
    ".profile",
];
const HASH_COMMENT_EXTS: &[&str] = &[
    "py",
    "pyi",
    "sh",
    "bash",
    "zsh",
    "fish",
    "rb",
    "pl",
    "pm",
    "yml",
    "yaml",
    "toml",
    "ini",
    "cfg",
    "conf",
    "mk",
    "r",
    "tf",
    "tfvars",
    "hcl",
    "nix",
    "ps1",
    "cmake",
    "awk",
    "sed",
    "properties",
];
const DASH_COMMENT_EXTS: &[&str] = &[
    "sql",
    "lua",
    "hs",
    "lhs",
    "elm",
    "ada",
    "adb",
    "ads",
    "vhd",
    "vhdl",
    "applescript",
];

fn is_hash_comment_file(path: &str) -> bool {
    let lower = file_name(path).to_ascii_lowercase();
    if HASH_COMMENT_FILES.contains(&lower.as_str())
        || lower.starts_with("dockerfile.")
        || lower.starts_with("makefile.")
        || lower.starts_with(".env.")
    {
        return true;
    }
    lower
        .rsplit_once('.')
        .is_some_and(|(_, ext)| HASH_COMMENT_EXTS.contains(&ext))
}

fn is_dash_comment_file(path: &str) -> bool {
    let lower = file_name(path).to_ascii_lowercase();
    lower
        .rsplit_once('.')
        .is_some_and(|(_, ext)| DASH_COMMENT_EXTS.contains(&ext))
}

fn is_comment_line(path: &str, text: &str) -> bool {
    let trimmed = text.trim_start();
    if trimmed.starts_with("//") {
        return true;
    }
    if trimmed.starts_with("/*") {
        return is_pure_c_comment(trimmed);
    }
    if let Some(after) = trimmed.strip_prefix("*/") {
        return is_pure_c_comment(after);
    }
    if trimmed == "*" {
        return true;
    }
    if trimmed.starts_with("* ") || trimmed.starts_with("*\t") {
        return match trimmed.split_once("*/") {
            Some((_, after)) => is_pure_c_comment(after),
            None => true,
        };
    }
    (trimmed.starts_with('#') && is_hash_comment_file(path))
        || ((trimmed.starts_with("-- ") || trimmed == "--") && is_dash_comment_file(path))
}

fn is_doc_path(path: &str) -> bool {
    let name = file_name(path);
    name.rsplit_once('.')
        .is_some_and(|(stem, ext)| match ext.to_ascii_lowercase().as_str() {
            "md" | "markdown" | "rst" | "adoc" => true,
            "txt" => {
                dirs(path).any(|d| matches!(d.to_ascii_lowercase().as_str(), "docs" | "doc"))
                    || matches!(
                        stem.to_ascii_lowercase().as_str(),
                        "readme" | "changelog" | "notes" | "license" | "contributing"
                    )
            }
            _ => false,
        })
}

fn spec_matches_line(
    rules: &Rules,
    index: usize,
    patterns: &[Regex],
    text: &str,
    is_test: bool,
) -> bool {
    if index == LAZY_CODE_RULE && is_test {
        return patterns.iter().enumerate().any(|(pat_idx, pattern)| {
            if pat_idx == LAZY_NOT_IMPL_PATTERN && rules.test_not_impl_assert.is_match(text) {
                let scrubbed = rules.test_not_impl_assert.replace_all(text, "");
                pattern.is_match(&scrubbed)
            } else {
                pattern.is_match(text)
            }
        });
    }
    patterns.iter().any(|pattern| pattern.is_match(text))
}

fn allowed(rules: &Rules, text: &str, id: &str) -> bool {
    rules
        .allow
        .captures_iter(text)
        .any(|caps| caps.get(1).is_some_and(|m| m.as_str() == id))
}

/// Scans added lines. A line carrying the allow marker for a rule, followed
/// by a reason, silences that rule on that line and is counted; any newly
/// added allow marker also emits an `ALLOW-001` warning so it cannot slip
/// through silently without approval.
pub fn scan(files: &[AddedLines]) -> Result<StaticReport, GateError> {
    let rules = rules()?;
    let mut report = StaticReport::default();
    for file in files.iter().filter(|file| !skip_path(&file.path)) {
        let is_test = is_test_path(&file.path);
        let ext = extension(&file.path);
        let is_doc = is_doc_path(&file.path);
        let is_patch = static_patch::is_patch_path(&file.path);
        let mut previous: Option<(u32, &str)> = None;
        for (number, text) in &file.lines {
            let is_comment = is_comment_line(&file.path, text);
            let quoted = is_patch && !static_patch::is_added_patch_line(text);
            let mut hits: Vec<(usize, u32, bool)> = Vec::new();
            for (index, patterns) in rules.rules.iter().enumerate() {
                let spec = &SPECS[index];
                if quoted && !static_patch::applies_to_quoted_patch_line(spec.id) {
                    continue;
                }
                if spec.skip_tests && is_test {
                    continue;
                }
                if spec.only_tests && !is_test {
                    continue;
                }
                if let Some(exts) = spec.extensions
                    && !exts.contains(&ext)
                {
                    continue;
                }
                if spec.skip_comments_and_docs && (is_comment || is_doc) {
                    continue;
                }
                if spec_matches_line(rules, index, patterns, text, is_test)
                    && !hits.iter().any(|(seen_idx, seen_line, _)| {
                        SPECS[*seen_idx].id == spec.id && *seen_line == *number
                    })
                {
                    hits.push((index, *number, allowed(rules, text, spec.id)));
                }
            }
            if !is_doc
                && !is_comment
                && !quoted
                && let Some((prev_number, prev_text)) = previous
                && !is_comment_line(&file.path, prev_text)
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
            if rules.allow.is_match(text) {
                report.findings.push(StaticFinding {
                    rule: "ALLOW-001".to_string(),
                    severity: Severity::Warn,
                    file: file.path.clone(),
                    line: Some(*number),
                    message: "newly added inline allow marker requires approval".to_string(),
                    fix: "remove the inline allow marker or sign an approval with aaf approve"
                        .to_string(),
                });
            }
            previous = Some((*number, text.as_str()));
        }
    }
    Ok(report)
}
