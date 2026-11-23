#![allow(clippy::unwrap_used, clippy::expect_used)]
//! G8 Oracle — NEVER EDIT AFTER PHASE 0.
//!
//! Test 1: noisy generic-auditor output with braces and [aaf] junk surrounding a valid
//! `{"answers":[...]}` JSON object in the middle → parse_verdict returns Pass or Fail,
//! NOT Inconclusive.
//!
//! Test 2: build_runner_with with executor = "gemini" → Ok(_).

use aaf_domain::config::RunConfig;
use aaf_domain::task::ImmutableSpec;
use aaf_engine::cross_audit::{AuditInput, AuditVerdict, build_checklist, parse_verdict};
use aaf_engine::executors::{ExecEnv, build_runner_with};

fn spec() -> ImmutableSpec {
    ImmutableSpec {
        description: "Oracle spec for G8".to_string(),
        acceptance: vec!["The fix is applied correctly".to_string()],
    }
}

/// Builds a minimal valid JSON answer for the given checklist (all "no").
fn all_no_answer(checklist: &[aaf_engine::cross_audit::Item]) -> String {
    let answers: Vec<serde_json::Value> = checklist
        .iter()
        .map(|item| {
            serde_json::json!({
                "id": item.id,
                "answer": "no",
                "evidence": "",
            })
        })
        .collect();
    serde_json::json!({ "answers": answers }).to_string()
}

/// Test 1: noisy output with banners, {braces}, code blocks, and trailing [aaf] notices
/// surrounding one valid RawAnswers JSON object → must parse as Pass or Fail (NOT Inconclusive).
#[test]
fn noisy_generic_auditor_output_still_parses() {
    let input = AuditInput {
        spec: spec(),
        diff: "diff --git a/x.rs b/x.rs".to_string(),
        mobile: false,
    };
    let checklist = build_checklist(&input.spec, input.mobile);
    let valid_json = all_no_answer(&checklist);

    // Surround the valid JSON with long noisy content that contains braces
    let long_prefix = "log line {stage: \"init\", ok: true}\n".repeat(200);
    let long_suffix = "trailing {metric: 42} output line\n".repeat(200);
    let noisy_text = format!(
        r#"[startup] gemini CLI v2.0 {{init_ok: true}}
{long_prefix}Analyzing diff for audit...
Running checks {{ step: 1 }}
Here is the output snippet from previous run:
```
fn foo() {{ let x = 1; }}
```
{valid_json}
[aaf] reaper: scan completed (0 descendants)
[aaf] GEMINI_CLI_TRUST_WORKSPACE was set to true
{long_suffix}trailing {{noise}} in logs
"#
    );

    let verdict = parse_verdict(&noisy_text, &checklist);
    assert!(
        !matches!(verdict, AuditVerdict::Inconclusive { .. }),
        "expected Pass or Fail, got Inconclusive: {verdict:?}"
    );
}

/// Test 2: build_runner_with with executor = "gemini" must succeed.
#[test]
fn gemini_executor_accepted_by_build_runner() {
    let cfg = RunConfig {
        executor: "gemini".to_string(),
        executor_cmd: vec![
            "gemini".to_string(),
            "-p".to_string(),
            "{prompt}".to_string(),
        ],
        ..RunConfig::default()
    };
    let result = build_runner_with(&cfg, ExecEnv::default());
    assert!(
        result.is_ok(),
        "gemini executor rejected: {:?}",
        result.err()
    );
}
