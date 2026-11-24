//! App Store and Google Play pre-submission checks. Owned by lane `store`.
//!
//! The scanner reads the repository only; it never runs a build tool. Its
//! contract (docs/PLAN.md C.6): a `Critical` is reported only for something
//! that can be read with certainty from a file that certainly ships. Anything
//! it cannot resolve statically is listed in `unresolved` and surfaced as a
//! warning, never as a `Critical` and never silently dropped. A file that
//! cannot be read or parsed becomes such an entry instead of failing the scan.
//!
//! Rules are written from Apple and Google primary documentation; each rule in
//! `report.rs` names the guideline it maps to.

mod android;
mod detect;
mod expo;
mod gradle;
mod gradle_resolve;
mod ios;
mod kmp;
mod privacy;
mod report;
mod text;

use std::collections::HashSet;
use std::path::Path;

use aaf_domain::config::{RepoConfig, StoreMode};
use aaf_domain::gate::{Severity, StoreComplianceReport, StoreStatus};

use report::Sink;

/// Prefix attached to out-of-diff store-compliance warnings and unresolved
/// items in `GateMode::Scoped` so pre-existing repo-wide debt remains visible
/// as advisory feedback without blocking or penalizing scoped tasks.
pub const PRE_EXISTING_PREFIX: &str = "[pre-existing] ";

/// Fixed signature (docs/PLAN.md C.6).
///
/// Activation: `off` gives `Skipped`. `auto` and `always` both decide from
/// the repository content, never from `project.profile`: a profile is a
/// one-line config value, and letting it say "not a mobile app" would switch
/// the scan off for a project that ships to a store. No Android, iOS,
/// Flutter, React Native, Expo or Unity project found gives `NotApplicable`.
///
/// `release` does not change the rules: the same tree gives the same report
/// in every gate mode, and the gate decides what a non-`Clear` status means
/// for a scoped run.
pub fn scan(repo_root: &Path, cfg: &RepoConfig, release: bool) -> StoreComplianceReport {
    // Kept in the signature for the gate; see the doc comment above.
    let _ = release;
    if cfg.gate.store_compliance == StoreMode::Off {
        return StoreComplianceReport::skipped();
    }

    let inventory = detect::walk(repo_root);
    let mut sink = Sink::default();
    for (path, problem) in &inventory.problems {
        let shown = if path.is_empty() { "." } else { path };
        sink.unreadable(shown, problem);
    }
    if detect::platforms(repo_root, &inventory).is_empty() {
        // Keep the reason when the tree itself could not be read.
        return StoreComplianceReport {
            status: StoreStatus::NotApplicable,
            critical: 0,
            warnings: 0,
            unresolved: sink.unresolved,
            findings: Vec::new(),
        };
    }

    let min_sdk = cfg.gate.min_android_target_sdk;
    android::scan(repo_root, &inventory, &mut sink);
    gradle::scan(repo_root, &inventory, min_sdk, &mut sink);
    ios::scan(repo_root, &inventory, &mut sink);
    privacy::scan(repo_root, &inventory, &mut sink);
    sink.platform = Some("expo");
    expo::scan(repo_root, &inventory, min_sdk, &mut sink);

    StoreComplianceReport::from_findings(sink.findings, sink.unresolved)
}

fn normalize_rel_path(path: &str) -> String {
    let mut norm = path.replace('\\', "/");
    while let Some(stripped) = norm.strip_prefix("./") {
        norm = stripped.to_string();
    }
    norm
}

fn unresolved_path(entry: &str) -> Option<&str> {
    let stripped = entry.strip_prefix(PRE_EXISTING_PREFIX).unwrap_or(entry);
    let (_, rest) = stripped.split_once(' ')?;
    let (location, _) = rest.split_once(": ")?;
    let path = match location.rsplit_once(':') {
        Some((p, line)) if !line.is_empty() && line.bytes().all(|b| b.is_ascii_digit()) => p,
        _ => location,
    };
    Some(path)
}

/// Filters a full repository [`StoreComplianceReport`] for `GateMode::Scoped`:
/// only `Critical` findings in `changed_paths` remain blocking (`Critical`);
/// pre-existing out-of-diff findings become advisory `Warn` entries prefixed
/// with [`PRE_EXISTING_PREFIX`], and out-of-diff `unresolved` entries are
/// similarly tagged so mechanical scoring does not penalize untouched debt.
pub fn scope_to_diff(
    report: StoreComplianceReport,
    changed_paths: &[String],
) -> StoreComplianceReport {
    if matches!(
        report.status,
        StoreStatus::Skipped | StoreStatus::NotApplicable
    ) {
        return report;
    }
    let touched: HashSet<String> = changed_paths
        .iter()
        .map(|p| normalize_rel_path(p))
        .collect();
    let findings = report
        .findings
        .into_iter()
        .map(|mut finding| {
            let in_diff = touched.contains(&normalize_rel_path(&finding.file));
            if !in_diff {
                if finding.severity == Severity::Critical {
                    finding.severity = Severity::Warn;
                }
                if !finding.message.starts_with(PRE_EXISTING_PREFIX) {
                    finding.message = format!("{PRE_EXISTING_PREFIX}{}", finding.message);
                }
            }
            finding
        })
        .collect();
    let unresolved = report
        .unresolved
        .into_iter()
        .map(|entry| {
            let in_diff =
                unresolved_path(&entry).is_some_and(|p| touched.contains(&normalize_rel_path(p)));
            if in_diff || entry.starts_with(PRE_EXISTING_PREFIX) {
                entry
            } else {
                format!("{PRE_EXISTING_PREFIX}{entry}")
            }
        })
        .collect();
    StoreComplianceReport::from_findings(findings, unresolved)
}

/// Runs [`scan`] and scopes the resulting report to `changed_paths` via
/// [`scope_to_diff`].
pub fn scan_scoped(
    repo_root: &Path,
    cfg: &RepoConfig,
    changed_paths: &[String],
) -> StoreComplianceReport {
    scope_to_diff(scan(repo_root, cfg, false), changed_paths)
}
