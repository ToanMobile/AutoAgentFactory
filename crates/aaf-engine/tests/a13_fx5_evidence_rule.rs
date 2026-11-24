#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The evidence rule of the cross-audit is one fail-closed rule: every "yes"
//! must cite a `file:line` in the diff, an item the previous reply did not
//! clearly answer "no" is re-answered "yes" WITH a citation, and a blocking
//! "yes" without (diff-anchored) evidence never turns into a pass.

use aaf_domain::task::ImmutableSpec;
use aaf_engine::cross_audit::prompt::build_format_repair_prompt;
use aaf_engine::cross_audit::{
    AuditInput, AuditVerdict, build_checklist, build_prompt, parse_diff_hunks,
    parse_scored_verdict, parse_verdict, validate_verdict_against_diff,
};

const DIFF: &str = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n\
@@ -1,1 +1,2 @@\n fn a() {}\n+fn b() {}\n";

fn spec() -> ImmutableSpec {
    ImmutableSpec {
        description: "Add b".into(),
        acceptance: vec!["b exists".into()],
    }
}

fn input(mobile: bool) -> AuditInput {
    AuditInput {
        spec: spec(),
        diff: DIFF.into(),
        mobile,
    }
}

/// Every item "no" except `id`, answered "yes" with `evidence`.
fn reply(mobile: bool, id: &str, evidence: &str) -> String {
    let answers: Vec<serde_json::Value> = build_checklist(&spec(), mobile)
        .iter()
        .map(|item| {
            if item.id == id {
                serde_json::json!({ "id": item.id, "answer": "yes", "evidence": evidence })
            } else {
                serde_json::json!({ "id": item.id, "answer": "no", "evidence": "" })
            }
        })
        .collect();
    serde_json::json!({ "answers": answers }).to_string()
}

#[test]
fn main_prompt_applies_the_evidence_rule_to_every_answer() {
    let prompt = build_prompt(&input(false));
    assert!(!prompt.contains("When unsure"), "{prompt}");
    assert!(
        prompt.contains("The evidence rule applies to every answer"),
        "main prompt does not scope the evidence rule to every answer"
    );
    assert!(
        prompt.contains("a \"yes\" on a blocking item without such a citation"),
        "main prompt does not say an unbacked blocking yes is inconclusive"
    );
}

#[test]
fn repair_prompt_fails_closed_with_a_cited_yes_not_a_contradiction() {
    let prompt = build_format_repair_prompt(&input(false), false, "bad", "prose");
    assert!(!prompt.contains("When unsure"), "{prompt}");
    // The old wording only said "answer yes", which the evidence rule then
    // turned back into "no" (a pass) for an item the auditor was unsure of.
    assert!(
        prompt.contains(
            "For any checklist item not clearly answered \"no\" in your previous response, \
             answer \"yes\" and cite in \"evidence\" the `file:line` in the diff above"
        ),
        "repair prompt does not require a cited yes: {prompt}"
    );
    assert!(
        prompt.contains("never a pass"),
        "repair prompt does not say an unbacked yes cannot pass"
    );
}

#[test]
fn blocking_yes_without_evidence_is_never_a_pass() {
    let checklist = build_checklist(&spec(), false);
    for evidence in ["", "looks wrong", "see the diff"] {
        let verdict = parse_verdict(&reply(false, "STUB", evidence), &checklist);
        assert!(
            matches!(verdict, AuditVerdict::Inconclusive { .. }),
            "{evidence:?} gave {verdict:?}"
        );
    }
}

/// Advisory items only warn: an uncited advisory "yes" is ignored (kept as
/// main's G2 behaviour, so non-blocking noise never stalls an unattended
/// run), while a cited one stays a warning on a pass.
#[test]
fn advisory_yes_only_warns_when_cited() {
    let checklist = build_checklist(&spec(), true);
    let verdict = parse_verdict(&reply(true, "STORE-PLACEHOLDER", ""), &checklist);
    assert!(
        matches!(&verdict, AuditVerdict::Pass { warnings } if warnings.is_empty()),
        "{verdict:?}"
    );
    let hunks = parse_diff_hunks(DIFF);
    let parsed = parse_scored_verdict(
        &reply(true, "STORE-SIWA", "src/lib.rs:2"),
        &checklist,
        false,
    );
    let checked = validate_verdict_against_diff(parsed, &hunks);
    assert!(
        matches!(&checked.verdict, AuditVerdict::Pass { warnings } if warnings.len() == 1),
        "{:?}",
        checked.verdict
    );
}
