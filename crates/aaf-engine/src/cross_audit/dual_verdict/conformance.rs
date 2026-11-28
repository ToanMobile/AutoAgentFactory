//! Deterministic PlanContract vs diff conformance analysis for Blind Dual-Review.
//!
//! Computes exact missing planned files and unplanned changed files once in Rust
//! so both blind reviewers share identical facts (avoiding redundant repo scans)
//! and hard out-of-scope / unplanned security-control-plane edits fail closed
//! deterministically without relying on LLM discretion.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use aaf_domain::pipeline::{IssueSeverity, PlanContract, ReviewIssue};
use aaf_domain::prompt_fence::normalize_repo_path;

use super::SCOPE_HARD_PREFIX;
use super::conformance_rules::{
    control_plane_globset, custom_hard_lock_globset, has_invisible_or_control_char,
    is_allowlisted_lockfile_or_root_generated, is_hard_security_path, is_in_dedicated_test_dir,
    is_invisible_or_control, path_in_scope,
};
use crate::cross_audit::diff_hunks::parse_diff_hunks;
use crate::cross_audit::prompt::neutralise;
use crate::gate::static_checks::{is_doc_path, is_test_path};

pub const CONFORMANCE_BEGIN: &str = "<<<AAF_UNTRUSTED_CONFORMANCE_BEGIN>>>";
pub const CONFORMANCE_END: &str = "<<<AAF_UNTRUSTED_CONFORMANCE_END>>>";
const MAX_SUMMARY_ITEMS: usize = 30;

/// Classification of one changed diff path not listed in the converged [`PlanContract`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnplannedFile {
    pub path: String,
    pub first_line: u32,
    pub is_test: bool,
    pub is_doc: bool,
    pub is_lockfile_or_generated: bool,
    pub invalid_path: bool,
    pub out_of_task_scope: bool,
    pub high_risk: bool,
    pub locked_or_agent_config: bool,
    pub same_dir_as_planned: bool,
}

/// Deterministic comparison between a converged [`PlanContract`] and the actual diff.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanConformance {
    pub missing_planned: Vec<String>,
    pub unplanned_changes: Vec<UnplannedFile>,
}

fn parent_dir(path: &str) -> Option<&str> {
    let (dir, _) = path.rsplit_once('/')?;
    (!dir.is_empty()).then_some(dir)
}

fn sanitize_display_path(raw: &str) -> String {
    let mut escaped = String::with_capacity(raw.len());
    for c in raw.chars() {
        if is_invisible_or_control(c) {
            let _ = write!(escaped, "{}", c.escape_default());
        } else {
            escaped.push(c);
        }
    }
    neutralise(&escaped)
}

/// Computes missing planned files and classified unplanned changed files from `diff`,
/// enforcing canonical control-plane/runner globs plus non-test/non-manifest custom
/// entries from `head_lock_globs`.
pub fn analyze_plan_conformance(
    diff: &str,
    plan: &PlanContract,
    scope_paths: &[String],
    head_lock_globs: &[String],
) -> PlanConformance {
    let hunks = parse_diff_hunks(diff);
    let custom_locked = custom_hard_lock_globset(head_lock_globs);
    let mut diff_files: BTreeMap<String, (u32, bool)> = BTreeMap::new();
    for (raw, spans) in &hunks {
        let (key, invalid) = if has_invisible_or_control_char(raw) {
            (raw.clone(), true)
        } else {
            match normalize_repo_path(raw) {
                Ok(norm) => (norm, false),
                Err(_) => (raw.clone(), true),
            }
        };
        let line = spans.first_line();
        diff_files
            .entry(key)
            .and_modify(|(cur_line, cur_inv)| {
                *cur_line = (*cur_line).min(line);
                *cur_inv = *cur_inv || invalid;
            })
            .or_insert((line, invalid));
    }

    let planned: BTreeSet<String> = plan
        .files_to_change
        .iter()
        .map(|f| f.path.as_str())
        .chain(plan.red_green_tests.iter().map(|t| t.test_path.as_str()))
        .filter_map(|p| normalize_repo_path(p).ok())
        .collect();
    let planned_lower: BTreeSet<String> = planned.iter().map(|p| p.to_ascii_lowercase()).collect();
    let planned_dirs: BTreeSet<String> = planned_lower
        .iter()
        .filter_map(|p| parent_dir(p).map(str::to_string))
        .collect();
    let diff_lower: BTreeSet<String> = diff_files
        .iter()
        .filter(|(_, (_, inv))| !*inv)
        .map(|(p, _)| p.to_ascii_lowercase())
        .collect();

    let missing_planned: Vec<String> = planned
        .iter()
        .filter(|p| !diff_lower.contains(&p.to_ascii_lowercase()))
        .cloned()
        .collect();

    let mut unplanned_changes = Vec::new();
    for (path, (first_line, invalid_path)) in diff_files {
        if !invalid_path && planned_lower.contains(&path.to_ascii_lowercase()) {
            continue;
        }
        let is_test = !invalid_path && is_test_path(&path);
        let is_doc = !invalid_path && is_doc_path(&path);
        let is_lockfile_or_generated =
            !invalid_path && is_allowlisted_lockfile_or_root_generated(&path);
        let out_of_task_scope = invalid_path || !path_in_scope(&path, scope_paths);
        let matches_custom = match &custom_locked {
            Ok(Some(set)) => set.is_match(&path),
            Ok(None) => false,
            Err(()) => true,
        };
        let locked_or_agent_config = invalid_path
            || control_plane_globset().is_none_or(|set| set.is_match(&path))
            || matches_custom;
        let same_dir_as_planned = !invalid_path
            && parent_dir(&path.to_ascii_lowercase()).is_some_and(|dir| planned_dirs.contains(dir));
        let check_filename_only = same_dir_as_planned && !out_of_task_scope;
        let high_risk = locked_or_agent_config
            || (!is_lockfile_or_generated && is_hard_security_path(&path, check_filename_only));
        unplanned_changes.push(UnplannedFile {
            path,
            first_line,
            is_test,
            is_doc,
            is_lockfile_or_generated,
            invalid_path,
            out_of_task_scope,
            high_risk,
            locked_or_agent_config,
            same_dir_as_planned,
        });
    }

    PlanConformance {
        missing_planned,
        unplanned_changes,
    }
}

/// Formats a neutralised, single-line-per-entry `Plan Conformance` summary for Dual-Review prompts.
pub fn format_conformance_summary(conf: &PlanConformance) -> String {
    let mut body = String::new();
    if conf.missing_planned.is_empty() && conf.unplanned_changes.is_empty() {
        body.push_str(
            "All planned files appear in the diff and no unplanned files were changed.\n",
        );
    } else {
        if conf.missing_planned.is_empty() {
            body.push_str("Missing planned files: none\n");
        } else {
            body.push_str("Missing planned files (absent from diff):\n");
            for p in conf.missing_planned.iter().take(MAX_SUMMARY_ITEMS) {
                let _ = writeln!(body, "- {}", sanitize_display_path(p));
            }
            if conf.missing_planned.len() > MAX_SUMMARY_ITEMS {
                let _ = writeln!(
                    body,
                    "- (+{} more)",
                    conf.missing_planned.len() - MAX_SUMMARY_ITEMS
                );
            }
        }
        if conf.unplanned_changes.is_empty() {
            body.push_str("Unplanned changed files: none\n");
        } else {
            body.push_str("Unplanned changed files (present in diff but not in PlanContract):\n");
            for f in conf.unplanned_changes.iter().take(MAX_SUMMARY_ITEMS) {
                let mut tags = Vec::new();
                if f.invalid_path {
                    tags.push("INVALID-PATH");
                }
                if f.locked_or_agent_config {
                    tags.push("LOCKED/AGENT-CONFIG");
                } else if f.high_risk {
                    tags.push("HIGH-RISK");
                }
                if f.out_of_task_scope {
                    tags.push("OUT-OF-TASK-SCOPE");
                }
                if f.is_lockfile_or_generated {
                    tags.push("lockfile/generated");
                }
                if f.is_test {
                    tags.push("test");
                }
                if f.is_doc {
                    tags.push("doc");
                }
                if f.same_dir_as_planned {
                    tags.push("same-dir-as-planned");
                }
                if tags.is_empty() {
                    tags.push("in-scope");
                }
                let _ = writeln!(
                    body,
                    "- {}:{} [{}]",
                    sanitize_display_path(&f.path),
                    f.first_line,
                    tags.join(", ")
                );
            }
            if conf.unplanned_changes.len() > MAX_SUMMARY_ITEMS {
                let _ = writeln!(
                    body,
                    "- (+{} more)",
                    conf.unplanned_changes.len() - MAX_SUMMARY_ITEMS
                );
            }
        }
    }
    format!("{CONFORMANCE_BEGIN}\n{body}{CONFORMANCE_END}")
}

/// Emits deterministic, non-rebuttable P1 violations for hard scope breaches:
/// - Any unplanned path with control/invisible characters or failing normalization.
/// - Any unplanned file matching canonical `AGENT_CONFIG_GLOBS`, `RUNNER_CONFIG_GLOBS`,
///   security control-plane globs, or custom non-test/non-manifest `head_lock_globs`
///   (including symlink directory prefixes like `.claude` / `.github/workflows`).
/// - Any unplanned production file outside `task.scope_paths` (only dedicated test dirs,
///   docs, and allowlisted lockfiles are exempt from `scope_paths`).
/// - Any unplanned non-test, non-doc, non-lockfile production file matching an exact
///   security/sandbox path token (including CamelCase names like `AuthService.kt`).
pub fn deterministic_scope_violations(conf: &PlanConformance) -> Vec<ReviewIssue> {
    let mut out = Vec::new();
    for f in &conf.unplanned_changes {
        let mut reasons = Vec::new();
        if f.invalid_path {
            reasons.push("unnormalized or control/invisible-character path");
        } else if f.locked_or_agent_config {
            reasons.push("matches agent-config, runner-config, or security control-plane path");
        }
        if !f.invalid_path && !f.is_doc && !f.is_lockfile_or_generated {
            let dedicated_test = f.is_test && is_in_dedicated_test_dir(&f.path);
            if f.out_of_task_scope && !dedicated_test {
                reasons.push("outside task scope_paths");
            }
            if !f.is_test && f.high_risk && !f.locked_or_agent_config {
                reasons.push("touches unplanned high-risk security/sandbox code");
            }
        }
        if reasons.is_empty() {
            continue;
        }
        let idx = out.len() + 1;
        let safe_path = sanitize_display_path(&f.path);
        out.push(ReviewIssue {
            id: format!("{SCOPE_HARD_PREFIX}{idx}"),
            severity: IssueSeverity::P1,
            category: super::PLAN_SCOPE_ID.to_string(),
            location: format!("{safe_path}:{}", f.first_line),
            description: format!(
                "Unplanned changed file `{safe_path}` violates hard scope boundary ({})",
                reasons.join("; ")
            ),
            proposed_fix: format!(
                "Revert unplanned changes to `{safe_path}` or update the task scope/plan before modifying it"
            ),
        });
    }
    out
}
