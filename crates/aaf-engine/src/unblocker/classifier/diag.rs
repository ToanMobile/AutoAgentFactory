//! Gate diagnostic extraction, untrusted-tag neutralization, and deterministic
//! error counting for [`super`].

use aaf_domain::gate::{LockStatus, Severity};

use super::{FailureSignal, one_line, truncate_chars};

pub const GATE_DIAG_MAX_CHARS: usize = 3000;
const MAX_DIAG_STATIC_FINDINGS: usize = 5;
const MAX_DIAG_STORE_FINDINGS: usize = 5;
const MAX_DIAG_LOCKED_FILES: usize = 5;
const MAX_DIAG_ERROR_LINES: usize = 8;
const FALLBACK_DIAG_TAIL_LINES: usize = 6;

const HIGH_SIGNAL_NEEDLES: &[&str] = &[
    "error",
    "failed",
    "panicked at",
    "assertion",
    "cannot find",
    "mismatched types",
    "expected ",
    "thread '",
    "test result: failed",
    "traceback",
    "exception",
    "syntaxerror",
    "typeerror",
    "referenceerror",
    "--> ",
];

fn strip_ansi(text: &str) -> String {
    let (mut out, mut chars) = (String::with_capacity(text.len()), text.chars().peekable());
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            if chars.peek() == Some(&'[') {
                let _ = chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            } else {
                let _ = chars.next();
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn is_high_signal_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    HIGH_SIGNAL_NEEDLES.iter().any(|n| lower.contains(n))
}

/// Escapes every `<untrusted-data` / `</untrusted-data` (any ASCII case) so
/// text quoted inside an untrusted-data block cannot close or reopen it.
/// Shared with the proposer's prompt builder.
pub(in crate::unblocker) fn neutralize_untrusted_tags(text: &str) -> String {
    const TAGS: [&[u8]; 2] = [b"</untrusted-data", b"<untrusted-data"];
    let (bytes, mut out, mut copied, mut at) =
        (text.as_bytes(), String::with_capacity(text.len()), 0, 0);
    while at < bytes.len() {
        if let Some(tag) = TAGS.iter().copied().find(|t| {
            at + t.len() <= bytes.len() && bytes[at..at + t.len()].eq_ignore_ascii_case(t)
        }) {
            out.push_str(&text[copied..at]);
            out.push_str(if tag.starts_with(b"</") {
                "&lt;/untrusted-data"
            } else {
                "&lt;untrusted-data"
            });
            at += tag.len();
            copied = at;
        } else {
            at += 1;
        }
    }
    out.push_str(&text[copied..]);
    out
}

fn format_finding_diag(rule: &str, file: &str, line: Option<u32>, msg: &str, fix: &str) -> String {
    let loc = line.map_or_else(|| file.to_string(), |l| format!("{file}:{l}"));
    let flat_fix = one_line(fix);
    if flat_fix.is_empty() {
        format!("{rule} @ {loc}: {}", one_line(msg))
    } else {
        format!("{rule} @ {loc}: {} [fix: {flat_fix}]", one_line(msg))
    }
}

/// Extracts compact, ANSI-stripped, tag-neutralized gate diagnostics (<= 3000
/// chars) from static/store findings (with fix hints), blocked head-lock paths,
/// oracle/proof/work-tree reasons, and failing command output.
pub fn extract_gate_diagnostics(signal: &FailureSignal) -> Option<String> {
    let receipt = signal.gate.as_ref()?;
    let mut parts: Vec<String> = Vec::new();
    let crit: Vec<String> = receipt
        .static_findings
        .iter()
        .filter(|f| f.severity == Severity::Critical)
        .take(MAX_DIAG_STATIC_FINDINGS)
        .map(|f| format_finding_diag(&f.rule, &f.file, f.line, &f.message, &f.fix))
        .collect();
    if !crit.is_empty() {
        parts.push(format!("static_critical: {}", crit.join("; ")));
    }
    let store_items: Vec<String> = receipt
        .store_compliance
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Critical)
        .chain(receipt.store_compliance.findings.iter().filter(|f| {
            f.severity == Severity::Warn
                && !f
                    .message
                    .starts_with(crate::store_compliance::PRE_EXISTING_PREFIX)
        }))
        .take(MAX_DIAG_STORE_FINDINGS)
        .map(|f| format_finding_diag(&f.rule, &f.file, f.line, &f.message, &f.fix))
        .collect();
    if !store_items.is_empty() {
        parts.push(format!("store_compliance: {}", store_items.join("; ")));
    }
    if receipt.head_lock.status == LockStatus::Blocked
        && !receipt.head_lock.locked_changes.is_empty()
    {
        let locked: Vec<&str> = receipt
            .head_lock
            .locked_changes
            .iter()
            .take(MAX_DIAG_LOCKED_FILES)
            .map(|c| c.path.as_str())
            .collect();
        parts.push(format!("head_lock: {}", locked.join(", ")));
    }
    for r in &receipt.reasons {
        if r.starts_with("oracle:")
            || r.starts_with("warning: oracle:")
            || r.starts_with("proof required:")
            || (r.starts_with("work tree:") && !r.starts_with("work tree: removed "))
        {
            parts.push(one_line(r));
        }
    }
    for cmd in receipt.commands.iter().filter(|c| !c.passed()) {
        let non_empty: Vec<String> = strip_ansi(&cmd.log_tail)
            .lines()
            .map(one_line)
            .filter(|l| !l.is_empty())
            .collect();
        if non_empty.is_empty() {
            continue;
        }
        let mut selected: Vec<String> = non_empty
            .iter()
            .filter(|l| is_high_signal_line(l))
            .take(MAX_DIAG_ERROR_LINES)
            .cloned()
            .collect();
        if selected.is_empty() {
            selected =
                non_empty[non_empty.len().saturating_sub(FALLBACK_DIAG_TAIL_LINES)..].to_vec();
        }
        parts.push(format!("cmd[{}]: {}", cmd.name, selected.join(" | ")));
    }
    if parts.is_empty() {
        return None;
    }
    let joined = neutralize_untrusted_tags(&parts.join(" || "));
    Some(truncate_chars(&joined, GATE_DIAG_MAX_CHARS))
}

fn parse_summary_failure_count(text: &str) -> Option<u32> {
    let lower = text.to_ascii_lowercase();
    let mut best: Option<u32> = None;
    for line in lower.lines() {
        let tokens: Vec<&str> = line
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|s| !s.is_empty())
            .collect();
        for win in tokens.windows(2) {
            if matches!(win[1], "failed" | "errors" | "error")
                && let Ok(n) = win[0].parse::<u32>()
                && n > 0
            {
                best = Some(best.map_or(n, |cur| cur.max(n)));
            }
        }
    }
    best
}

/// Deterministic count of concrete error/failure items in `signal`, used by
/// `Ladder` to detect progress across `T3FreshContext` retries.
pub(crate) fn failure_error_count(signal: &FailureSignal) -> Option<u32> {
    if let Some(receipt) = signal.gate.as_ref() {
        let mut total: u32 = u32::try_from(
            receipt
                .static_findings
                .iter()
                .filter(|f| f.severity == Severity::Critical)
                .count(),
        )
        .unwrap_or(u32::MAX);
        total = total.saturating_add(receipt.store_compliance.critical);
        if receipt.head_lock.status == LockStatus::Blocked {
            total = total.saturating_add(
                u32::try_from(receipt.head_lock.locked_changes.len()).unwrap_or(u32::MAX),
            );
        }
        for cmd in receipt.commands.iter().filter(|c| !c.passed()) {
            let cleaned = strip_ansi(&cmd.log_tail);
            let cmd_errs = parse_summary_failure_count(&cleaned).unwrap_or_else(|| {
                let hits = cleaned.lines().filter(|l| is_high_signal_line(l)).count();
                u32::try_from(hits.max(1)).unwrap_or(u32::MAX)
            });
            total = total.saturating_add(cmd_errs);
        }
        return (total > 0).then_some(total);
    }
    if !signal.merge_conflict_files.is_empty() {
        return Some(u32::try_from(signal.merge_conflict_files.len()).unwrap_or(u32::MAX));
    }
    None
}
