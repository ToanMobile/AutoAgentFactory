//! T1: deterministic classification of a failed step. Pure, no I/O.

use aaf_domain::blocked::BlockedReason;
use aaf_domain::budget::BudgetCheck;
use aaf_domain::gate::{GateStatus, LockStatus, Receipt, Severity, StoreStatus};
use aaf_domain::ports::AgentOutcome;

use crate::gate::REASON_TREE_CHANGED;

#[path = "classifier_diag.rs"]
mod diag;

pub(crate) use diag::failure_error_count;
pub(super) use diag::neutralize_untrusted_tags;
pub use diag::{GATE_DIAG_MAX_CHARS, extract_gate_diagnostics};

/// Consecutive identical failure digests after which a task counts as stuck.
pub const STUCK_REPEAT_THRESHOLD: u32 = 3;

const SUMMARY_MAX_CHARS: usize = 200;
const DIGEST_TAIL_LINES: usize = 20;
const DIGEST_HEX_CHARS: usize = 16;
const MAX_LISTED_FILES: usize = 10;

/// Transient upstream provider outage needles in agent stderr/log output.
const TRANSIENT_INFRA_PATTERNS: &[&str] = &[
    "http 500",
    "http 502",
    "http 503",
    "http 504",
    "http 529",
    "status 500",
    "status 502",
    "status 503",
    "status 504",
    "status 529",
    "503 service unavailable",
    "529 overloaded",
    "\"type\":\"overloaded_error\"",
    "\"type\": \"overloaded_error\"",
    "\"type\":\"api_error\"",
    "\"type\": \"api_error\"",
];

fn transient_infra_detail(outcome: &AgentOutcome) -> Option<String> {
    if outcome.succeeded() {
        return None;
    }
    let hay_tail = outcome.log_tail.to_ascii_lowercase();
    let matched = TRANSIENT_INFRA_PATTERNS
        .iter()
        .any(|needle| hay_tail.contains(needle));
    if !matched {
        return None;
    }
    let flat = one_line(&outcome.log_tail);
    if flat.is_empty() {
        Some("transient upstream infrastructure failure".to_string())
    } else {
        Some(truncate_chars(&flat, SUMMARY_MAX_CHARS))
    }
}

/// Result of rerunning the failed tests without any code change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RerunResult {
    pub runs: u32,
    pub passed: u32,
}

/// Everything the factory knows after a failed step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureSignal {
    pub outcome: Option<AgentOutcome>,
    pub gate: Option<Receipt>,
    pub merge_conflict_files: Vec<String>,
    /// Id of a task this one depends on that is not done yet.
    pub open_dependency: Option<String>,
    pub test_rerun: Option<RerunResult>,
    /// Consecutive failures with the same [`failure_digest`], this one included.
    pub same_failure_count: u32,
    pub budget: BudgetCheck,
}

impl FailureSignal {
    /// A signal that carries no evidence; callers fill in what they have.
    pub fn empty() -> FailureSignal {
        FailureSignal {
            outcome: None,
            gate: None,
            merge_conflict_files: Vec::new(),
            open_dependency: None,
            test_rerun: None,
            same_failure_count: 0,
            budget: BudgetCheck::Ok,
        }
    }
}

/// Human-readable text for a budget that is not `Ok`.
pub fn budget_detail(check: BudgetCheck) -> Option<&'static str> {
    match check {
        BudgetCheck::Ok => None,
        BudgetCheck::Frozen => Some("root budget is frozen"),
        BudgetCheck::TokensExceeded => Some("root token budget exceeded"),
        BudgetCheck::WallClockExceeded => Some("root wall-clock budget exceeded"),
        BudgetCheck::CyclesExceeded => Some("root unblock-cycle budget exceeded"),
    }
}

/// Most specific rule first: a cause that makes every retry pointless
/// (budget, dependency, quota) must win over symptoms seen in the logs.
pub fn classify(signal: &FailureSignal) -> BlockedReason {
    if let Some(detail) = budget_detail(signal.budget) {
        return BlockedReason::BudgetExceeded {
            detail: detail.to_string(),
        };
    }
    if let Some(task_id) = &signal.open_dependency {
        return BlockedReason::Dependency {
            task_id: task_id.clone(),
        };
    }
    if let Some(hit) = signal.outcome.as_ref().and_then(|o| o.limit_hit.as_ref()) {
        return BlockedReason::Quota {
            reset_at: hit.reset_at,
        };
    }
    if let Some(infra) = signal.outcome.as_ref().and_then(|o| o.infra.as_ref()) {
        let flat = one_line(&infra.raw);
        let detail = if flat.is_empty() {
            "executor infrastructure failure".to_string()
        } else {
            truncate_chars(&flat, SUMMARY_MAX_CHARS)
        };
        return BlockedReason::Infra { detail };
    }
    if !signal.merge_conflict_files.is_empty() {
        return BlockedReason::MergeConflict {
            detail: conflict_detail(&signal.merge_conflict_files),
        };
    }
    if let Some(receipt) = signal.gate.as_ref()
        && receipt.status == GateStatus::Blocked
    {
        if let Some(rerun) = signal.test_rerun
            && rerun.passed > 0
            && is_pure_test_failure(receipt)
        {
            return BlockedReason::FlakyTest {
                detail: format!(
                    "{} of {} reruns passed without a code change",
                    rerun.passed, rerun.runs
                ),
            };
        }
        return BlockedReason::GateFailed {
            summary: gate_summary(receipt),
        };
    }
    if let Some(rerun) = signal.test_rerun
        && rerun.passed > 0
    {
        return BlockedReason::FlakyTest {
            detail: format!(
                "{} of {} reruns passed without a code change",
                rerun.passed, rerun.runs
            ),
        };
    }
    if signal.outcome.as_ref().is_some_and(|o| o.timed_out) {
        return BlockedReason::Stuck {
            detail: "agent run timed out".to_string(),
        };
    }
    if let Some(outcome) = signal.outcome.as_ref()
        && let Some(detail) = transient_infra_detail(outcome)
    {
        return BlockedReason::Infra { detail };
    }
    let digest = failure_digest(signal);
    if signal.same_failure_count >= STUCK_REPEAT_THRESHOLD {
        return BlockedReason::Stuck {
            detail: format!(
                "same failure {} times in a row (digest {digest})",
                signal.same_failure_count
            ),
        };
    }
    BlockedReason::Stuck {
        detail: format!("unclassified failure (digest {digest})"),
    }
}

/// True when `receipt` blocked solely because a test command (`test_scoped` or
/// `test_full`) exited non-zero without timing out, with no other blocking
/// evidence (no critical static findings, head-lock/store-compliance clear,
/// no tree-change/proof/hidden/batch-combination block, and no lint/build failure).
pub fn is_pure_test_failure(receipt: &Receipt) -> bool {
    if receipt.status != GateStatus::Blocked
        || receipt
            .static_findings
            .iter()
            .any(|f| f.severity == Severity::Critical)
        || receipt.head_lock.status == LockStatus::Blocked
        || receipt.store_compliance.status == StoreStatus::Blocked
        || receipt.reasons.iter().any(|r| {
            r == REASON_TREE_CHANGED
                || r.starts_with("fails only in combination with:")
                || r.starts_with("proof required:")
                || r.starts_with("commands skipped:")
                || r.starts_with("no test command is configured")
                || (r.starts_with("work tree:") && !r.starts_with("work tree: removed "))
        })
    {
        return false;
    }
    let failed: Vec<_> = receipt.commands.iter().filter(|c| !c.passed()).collect();
    !failed.is_empty()
        && failed
            .iter()
            .all(|c| !c.timed_out && matches!(c.name.as_str(), "test_scoped" | "test_full"))
}

/// Short stable digest of a failure: two failures that differ only in
/// numbers, paths, hex ids or UUIDs hash equal.
pub fn failure_digest(signal: &FailureSignal) -> String {
    let normalised = normalise(&digest_material(signal));
    let mut hex = aaf_fp::sha256_hex(normalised.as_bytes());
    hex.truncate(DIGEST_HEX_CHARS);
    hex
}

/// Picks the evidence in the same order as [`classify`], so the digest
/// describes the failure that was classified. The gate is preferred over
/// agent logs because agent prose differs on every run.
fn digest_material(signal: &FailureSignal) -> String {
    if let Some(dep) = &signal.open_dependency {
        return format!("dependency:{dep}");
    }
    if signal
        .outcome
        .as_ref()
        .is_some_and(|o| o.limit_hit.is_some())
    {
        return "quota".to_string();
    }
    if signal.outcome.as_ref().is_some_and(|o| o.infra.is_some()) {
        return "infra".to_string();
    }
    if !signal.merge_conflict_files.is_empty() {
        let mut files = signal.merge_conflict_files.clone();
        files.sort();
        return format!("conflict:{}", files.join("\n"));
    }
    if let Some(receipt) = &signal.gate {
        return gate_material(receipt);
    }
    match &signal.outcome {
        Some(outcome) if outcome.timed_out => "agent:timeout".to_string(),
        Some(outcome) if transient_infra_detail(outcome).is_some() => "infra".to_string(),
        Some(outcome) => format!(
            "agent:exit={:?}\n{}",
            outcome.exit_code,
            tail_lines(&outcome.log_tail)
        ),
        None => "unknown".to_string(),
    }
}

fn gate_material(receipt: &Receipt) -> String {
    // A throttle note says how busy the machine was, not what failed: with
    // it, the same failure after a wait for a slot looked like a new one and
    // the ladder never saw the repeat.
    let reasons: Vec<&str> = receipt
        .reasons
        .iter()
        .map(String::as_str)
        .filter(|reason| !reason.starts_with(crate::gate::runner::THROTTLE_NOTE))
        .collect();
    let mut out = format!("gate:{:?}\n{}", receipt.status, reasons.join("\n"));
    for command in receipt.commands.iter().filter(|c| !c.passed()) {
        out.push_str(&format!(
            "\ncmd:{}\n{}",
            command.name,
            tail_lines(&command.log_tail)
        ));
    }
    for finding in &receipt.static_findings {
        out.push_str(&format!("\nfinding:{}:{}", finding.rule, finding.file));
    }
    out
}

fn tail_lines(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(DIGEST_TAIL_LINES)..].join("\n")
}

fn conflict_detail(files: &[String]) -> String {
    let listed: Vec<&str> = files
        .iter()
        .take(MAX_LISTED_FILES)
        .map(String::as_str)
        .collect();
    let more = files.len().saturating_sub(listed.len());
    let mut detail = format!("{} conflicted file(s): {}", files.len(), listed.join(", "));
    if more > 0 {
        detail.push_str(&format!(" (+{more} more)"));
    }
    detail
}

fn gate_summary(receipt: &Receipt) -> String {
    let mut parts: Vec<String> = receipt.reasons.clone();
    if parts.is_empty() {
        parts.extend(
            receipt
                .commands
                .iter()
                .filter(|c| !c.passed())
                .map(|c| format!("{} failed", c.name)),
        );
    }
    if parts.is_empty() {
        parts.push("gate blocked".to_string());
    }
    truncate_chars(&one_line(&parts.join("; ")), SUMMARY_MAX_CHARS)
}

/// Collapses whitespace and control characters so the text stays on one line.
pub(crate) fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn normalise(text: &str) -> String {
    let mut out = String::new();
    for token in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        // Directories differ per worktree and temp dir; the file name does not.
        let rest = match token.rfind(['/', '\\']) {
            Some(idx) => {
                out.push_str("<path>/");
                &token[idx + 1..]
            }
            None => token,
        };
        let mut chunk = String::new();
        for c in rest.chars() {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                chunk.push(c);
            } else {
                flush_chunk(&mut chunk, &mut out);
                out.push(c);
            }
        }
        flush_chunk(&mut chunk, &mut out);
    }
    out
}

fn flush_chunk(chunk: &mut String, out: &mut String) {
    if is_uuid(chunk) {
        out.push_str("<hex>");
    } else {
        for (i, word) in chunk.split('-').enumerate() {
            if i > 0 {
                out.push('-');
            }
            push_word(word, out);
        }
    }
    chunk.clear();
}

fn is_uuid(chunk: &str) -> bool {
    let groups: Vec<&str> = chunk.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8usize, 4, 4, 4, 12])
            .all(|(g, len)| g.len() == len && g.chars().all(|c| c.is_ascii_hexdigit()))
}

fn is_hex_word(word: &str) -> bool {
    let all_hex = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit());
    if let Some(rest) = word.strip_prefix("0x").or_else(|| word.strip_prefix("0X")) {
        return all_hex(rest);
    }
    // A digit is required so plain words such as "feedback" survive.
    word.len() >= 7 && all_hex(word) && word.chars().any(|c| c.is_ascii_digit())
}

fn push_word(word: &str, out: &mut String) {
    if is_hex_word(word) {
        out.push_str("<hex>");
        return;
    }
    // Only words that start with a digit are volatile (line numbers, pids,
    // durations such as `12ms`, timestamp parts). Digits inside an
    // identifier carry meaning: `E0425` and `E0308` are different failures.
    if !word.starts_with(|c: char| c.is_ascii_digit()) {
        out.push_str(word);
        return;
    }
    let mut in_digits = false;
    for c in word.chars() {
        if c.is_ascii_digit() {
            if !in_digits {
                out.push_str("<n>");
            }
            in_digits = true;
        } else {
            in_digits = false;
            out.push(c);
        }
    }
}
