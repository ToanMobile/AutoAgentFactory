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

use std::path::Path;

use aaf_domain::config::{RepoConfig, StoreMode};
use aaf_domain::gate::{StoreComplianceReport, StoreStatus};

use report::Sink;

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
