#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Oracle (Phase 0 — RED): cross-audit evidence-verification contract.
//!
//! These three cases define the NEW contract and MUST stay unedited after
//! Phase 0. Run on base `92367ec` they all fail at assertion (not compile
//! error) because the production code does not yet enforce the contract.
//!
//! NEVER edit this file after Phase 0.

use std::sync::Mutex;

use aaf_domain::ports::{AgentOutcome, AgentRequest, AgentRunner, PortResult, SandboxPolicy};
use aaf_domain::task::ImmutableSpec;
use aaf_engine::cross_audit::{
    AuditInput, AuditVerdict, build_checklist, parse_verdict, run_audit,
};

// ---------------------------------------------------------------------------
// Minimal fakes
// ---------------------------------------------------------------------------

struct FixedRunner {
    text: String,
    seen: Mutex<u32>,
}

impl FixedRunner {
    fn new(text: impl Into<String>) -> Self {
        FixedRunner {
            text: text.into(),
            seen: Mutex::new(0),
        }
    }
}

impl AgentRunner for FixedRunner {
    fn run(&self, _request: &AgentRequest) -> PortResult<AgentOutcome> {
        *self.seen.lock().unwrap() += 1;
        Ok(AgentOutcome {
            exit_code: Some(0),
            timed_out: false,
            final_text: self.text.clone(),
            tokens_in: 10,
            tokens_out: 5,
            limit_hit: None,
            infra: None,
            log_tail: String::new(),
        })
    }
}

fn template() -> AgentRequest {
    AgentRequest {
        run_id: "oracle-g2".to_string(),
        prompt: "placeholder".to_string(),
        cwd: "/work".to_string(),
        account: None,
        timeout_secs: 60,
        sandbox: SandboxPolicy {
            enabled: false,
            read_only: false,
            writable_paths: vec![],
            read_denied_paths: vec![],
        },
        json_schema: None,
    }
}

fn spec_one_ac() -> ImmutableSpec {
    ImmutableSpec {
        description: "Add retry logic".to_string(),
        acceptance: vec!["Retries on transient errors".to_string()],
    }
}

/// A minimal unified diff with one real hunk covering `src/retry.rs` lines 1-5.
fn sample_diff() -> String {
    "diff --git a/src/retry.rs b/src/retry.rs\n\
     --- a/src/retry.rs\n\
     +++ b/src/retry.rs\n\
     @@ -1,3 +1,5 @@\n\
      fn retry() {\n\
     +    let token = \"hardcoded_token_value\";\n\
     +    do_retry();\n\
          finish();\n\
      }\n"
    .to_string()
}

/// Build a JSON answer where `yes_id` is answered \"yes\" with the given evidence.
fn answer_with_evidence(spec: &ImmutableSpec, yes_id: &str, evidence: &str) -> String {
    let checklist = build_checklist(spec, false);
    let answers: Vec<serde_json::Value> = checklist
        .iter()
        .map(|item| {
            let hit = item.id == yes_id;
            serde_json::json!({
                "id": item.id,
                "answer": if hit { "yes" } else { "no" },
                "evidence": if hit { evidence } else { "" },
            })
        })
        .collect();
    serde_json::json!({ "answers": answers }).to_string()
}

// ---------------------------------------------------------------------------
// Case 1: blocking "yes" with EMPTY evidence → Inconclusive
// (currently returns Fail → RED at assertion)
// ---------------------------------------------------------------------------
#[test]
fn case1_blocking_yes_empty_evidence_is_inconclusive() {
    let spec = spec_one_ac();
    let checklist = build_checklist(&spec, false);

    // "SECRET" blocking, answered "yes" but evidence is empty string.
    let text = answer_with_evidence(&spec, "SECRET", "");
    let verdict = parse_verdict(&text, &checklist);

    // NEW contract: empty evidence on a blocking "yes" → Inconclusive.
    // On base 92367ec this assertion FAILS because verdict is Fail.
    assert!(
        matches!(verdict, AuditVerdict::Inconclusive { .. }),
        "expected Inconclusive for blocking yes with empty evidence, got {verdict:?}"
    );
}

// ---------------------------------------------------------------------------
// Case 2: blocking "yes" with valid file:line inside diff hunk → Fail
// (currently returns Fail → GREEN at assertion even on base, stays GREEN)
// ---------------------------------------------------------------------------
#[test]
fn case2_blocking_yes_with_valid_file_line_in_diff_is_fail() {
    let spec = spec_one_ac();
    let diff = sample_diff();

    // "SECRET" blocking, evidence cites src/retry.rs:2 which IS in the diff hunk.
    let answer = answer_with_evidence(&spec, "SECRET", "src/retry.rs:2");
    let runner = FixedRunner::new(answer);
    let input = AuditInput {
        spec,
        diff,
        mobile: false,
    };

    let report = run_audit(&input, &runner, &template()).unwrap();
    // This should be Fail both before and after the fix.
    assert!(
        matches!(report.verdict, AuditVerdict::Fail { .. }),
        "expected Fail for blocking yes with valid file:line in diff, got {:?}",
        report.verdict
    );
}

// ---------------------------------------------------------------------------
// Case 3: blocking "yes" citing nonexistent.rs:999 outside diff → Inconclusive
// (currently returns Fail → RED at assertion)
// ---------------------------------------------------------------------------
#[test]
fn case3_blocking_yes_hallucinated_file_line_is_inconclusive() {
    let spec = spec_one_ac();
    let diff = sample_diff();

    // "SECRET" blocking, evidence cites a file that is NOT in the diff.
    let answer = answer_with_evidence(&spec, "SECRET", "nonexistent.rs:999");
    let runner = FixedRunner::new(answer);
    let input = AuditInput {
        spec,
        diff,
        mobile: false,
    };

    let report = run_audit(&input, &runner, &template()).unwrap();
    // NEW contract: hallucinated file:line → Inconclusive.
    // On base 92367ec this assertion FAILS because verdict is Fail.
    assert!(
        matches!(report.verdict, AuditVerdict::Inconclusive { .. }),
        "expected Inconclusive for blocking yes with hallucinated file:line, got {:?}",
        report.verdict
    );
}
