#![allow(clippy::unwrap_used, clippy::expect_used)]
//! G8 edge-case tests: duplicate audit JSON fail-closed, string escapes & unbalanced
//! noise, > 64 KiB GenericRunner mid-output JSON up to 256 KiB, `tokens_measured = false`,
//! trailing `[aaf]` notice stripping, and `gemini` executor wiring.

use aaf_domain::config::RunConfig;
use aaf_domain::ports::{AgentOutcome, AgentRequest, AgentRunner, SandboxPolicy};
use aaf_domain::task::ImmutableSpec;
use aaf_engine::cross_audit::{AuditVerdict, Item, build_checklist, parse_verdict};
use aaf_engine::executors::generic::GenericRunner;
use aaf_engine::executors::{ExecEnv, build_runner_with};

fn spec() -> ImmutableSpec {
    ImmutableSpec {
        description: "Edge test spec for G8".to_string(),
        acceptance: vec!["Meets all criteria".to_string()],
    }
}

fn answer(checklist: &[Item], yes: &[&str], evidence: &str) -> String {
    let answers: Vec<serde_json::Value> = checklist
        .iter()
        .map(|item| {
            let hit = yes.contains(&item.id.as_str());
            serde_json::json!({
                "id": item.id,
                "answer": if hit { "yes" } else { "no" },
                "evidence": if hit { evidence } else { "" },
            })
        })
        .collect();
    serde_json::json!({ "answers": answers }).to_string()
}

fn req(prompt: &str, cwd: &str) -> AgentRequest {
    AgentRequest {
        run_id: format!("g8-{}", uuid::Uuid::new_v4()),
        prompt: prompt.to_string(),
        cwd: cwd.to_string(),
        account: None,
        timeout_secs: 60,
        sandbox: SandboxPolicy::default(),
        json_schema: None,
    }
}

#[test]
fn conflicting_answers_and_oversized_output_fail_closed() {
    let checklist = build_checklist(&spec(), false);
    let pass = answer(&checklist, &[], "");
    let fail = answer(&checklist, &["SECRET"], "src/secret.rs:1");
    let combined = format!("banner {{init: 1}}\n{fail}\n[log] {{step: 2}}\n{pass}\n[aaf] ok\n");
    let verdict = parse_verdict(&combined, &checklist);
    assert!(
        matches!(verdict, AuditVerdict::Inconclusive { ref reason } if reason.contains("more than one")),
        "expected Inconclusive for two RawAnswers objects, got {verdict:?}"
    );
    let huge = format!("{}{pass}", "x".repeat(512 * 1024 + 1));
    assert!(matches!(
        parse_verdict(&huge, &checklist),
        AuditVerdict::Inconclusive { .. }
    ));
}

#[test]
fn string_braces_escapes_and_unbalanced_noise_parse_cleanly() {
    let checklist = build_checklist(&spec(), false);
    let fail_json = answer(
        &checklist,
        &["STUB"],
        r#"src/stub.rs:1 fn stub() { return "}\"\\"; }"#,
    );
    let noisy = format!(
        "unbalanced {{\n{{\"event\":\"status\",\"meta\":{{\"ok\":true}}}}\n\
         broken {{\"msg\": \"unclosed\n{fail_json}\ntrailing }} brace\n"
    );
    let verdict = parse_verdict(&noisy, &checklist);
    assert!(
        matches!(verdict, AuditVerdict::Fail { ref findings, .. } if findings.len() == 1 && findings[0].id == "STUB"),
        "expected Fail(STUB), got {verdict:?}"
    );
}

#[test]
fn generic_runner_marks_tokens_unmeasured_and_strips_trailing_aaf_notices() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = RunConfig {
        executor: "gemini".to_string(),
        executor_cmd: vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "printf '%s\\n[aaf] reaper: cleaned\\n' \"$1\"".to_string(),
            "sh".to_string(),
            "{prompt}".to_string(),
        ],
        ..RunConfig::default()
    };
    let runner = build_runner_with(&cfg, ExecEnv::default()).unwrap();
    let outcome = runner
        .run(&req("gemini-ok", dir.path().to_str().unwrap()))
        .unwrap();
    assert!(outcome.succeeded(), "{outcome:?}");
    assert_eq!(outcome.final_text, "gemini-ok");
    assert!(outcome.log_tail.contains("[aaf] reaper: cleaned"));
    assert_eq!((outcome.tokens_in, outcome.tokens_out), (0, 0));
    assert!(!runner.reports_tokens());

    let legacy = serde_json::json!({
        "exit_code": 0, "timed_out": false, "final_text": "ok",
        "tokens_in": 10, "tokens_out": 5, "limit_hit": null, "infra": null, "log_tail": ""
    });
    assert!(serde_json::from_value::<AgentOutcome>(legacy).is_ok());
}

#[test]
fn generic_runner_preserves_mid_output_json_above_64kib_up_to_256kib() {
    let dir = tempfile::tempdir().unwrap();
    let checklist = build_checklist(&spec(), false);
    let pass = answer(&checklist, &[], "");
    let payload_path = dir.path().join("auditor_output.txt");
    let body = format!(
        "{}{pass}\n{}",
        "prefix log {step: 1} 0123456789abcdef\n".repeat(256),
        "suffix log {step: 2} fedcba9876543210\n".repeat(2304),
    );
    assert!(body.len() > 80 * 1024 && body.len() < 200 * 1024);
    std::fs::write(&payload_path, &body).unwrap();

    let runner = GenericRunner::new(
        vec!["/bin/cat".to_string(), "{prompt}".to_string()],
        ExecEnv::default(),
    )
    .unwrap();
    let outcome = runner
        .run(&req(
            payload_path.to_str().unwrap(),
            dir.path().to_str().unwrap(),
        ))
        .unwrap();
    assert!(outcome.succeeded() && !runner.reports_tokens());
    assert!(outcome.final_text.len() > 80 * 1024);
    assert!(parse_verdict(&outcome.final_text, &checklist).is_pass());
}
