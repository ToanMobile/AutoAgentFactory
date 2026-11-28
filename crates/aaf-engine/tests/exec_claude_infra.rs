#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Claude Code runs that failed for the account or the network, not for the
//! task (`AgentOutcome::infra`).
//!
//! Evidence used, and how sure it is:
//! - `claude_not_logged_in.stream.jsonl` is REAL output of Claude Code
//!   2.1.330 with no login: an assistant message flagged
//!   `"is_api_error_message": true` with `"error": "authentication_failed"`,
//!   then an error result.
//! - The other values of that `error` field (`oauth_org_not_allowed`,
//!   `account_on_hold`, `verification_required`, `billing_error`,
//!   `cloud_credential_error`) and the connection wording ("Unable to
//!   connect to API", "Can't reach the API server", ...) are read from the
//!   2.1.330 binary's tables; a run that produced them was never observed,
//!   so the network events below are hand-written in the real shape.

use std::path::{Path, PathBuf};

use aaf_domain::ports::{AgentRequest, AgentRunner, InfraKind, SandboxPolicy};
use aaf_engine::executors::ExecEnv;
use aaf_engine::executors::claude::{ClaudeCodeRunner, parse_stream};

#[path = "../../aaf-fp/tests/exec_support.rs"]
mod exec_support;
use exec_support::write_executable;

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/exec")
        .join(name)
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(fixture_path(name)).unwrap()
}

/// An API error message as Claude Code writes it (shape of the real
/// not-logged-in fixture).
fn api_error(error: &str, text: &str) -> String {
    serde_json::json!({
        "type": "assistant",
        "message": {"model": "<synthetic>", "content": [{"type": "text", "text": text}]},
        "error": error,
        "is_api_error_message": true,
    })
    .to_string()
}

/// An ordinary assistant message: the agent's own words.
fn said(text: &str) -> String {
    serde_json::json!({
        "type": "assistant",
        "message": {"content": [{"type": "text", "text": text}]},
    })
    .to_string()
}

fn result(is_error: bool, text: &str, status: Option<i64>) -> String {
    serde_json::json!({
        "type": "result", "is_error": is_error, "result": text, "api_error_status": status,
    })
    .to_string()
}

fn stream(lines: &[String]) -> String {
    lines.join("\n")
}

#[test]
fn the_real_not_logged_in_run_is_an_auth_failure() {
    let summary = parse_stream(&fixture("claude_not_logged_in.stream.jsonl"));
    let infra = summary.infra.expect("an auth failure");
    assert_eq!(infra.kind, InfraKind::Auth);
    assert_eq!(infra.raw, "Not logged in · Please run /login");
    assert_eq!(summary.limit_hit, None);
}

#[test]
fn every_account_error_of_the_cli_is_an_auth_failure() {
    for error in [
        "authentication_failed",
        "oauth_org_not_allowed",
        "account_on_hold",
        "verification_required",
        "billing_error",
        "cloud_credential_error",
    ] {
        let text = stream(&[
            api_error(error, "Failed to authenticate"),
            result(true, "Failed to authenticate", None),
        ]);
        let infra = parse_stream(&text).infra;
        assert_eq!(infra.map(|i| i.kind), Some(InfraKind::Auth), "{error}");
    }
}

#[test]
fn a_401_or_403_from_the_api_is_an_auth_failure() {
    for status in [401, 403] {
        let text = result(true, "API Error: 401 invalid x-api-key", Some(status));
        let infra = parse_stream(&text).infra;
        assert_eq!(infra.map(|i| i.kind), Some(InfraKind::Auth), "{status}");
    }
}

#[test]
fn a_connection_failure_of_the_cli_is_a_network_failure() {
    for text in [
        "API Error: Unable to connect to API (ENOTFOUND)",
        "Unable to connect to API. Check your internet connection",
        "Can't reach the API server \u{2014} check your internet or DNS (EAI_AGAIN)",
        "No internet route \u{2014} check your connection or VPN (ENETUNREACH)",
        "Connection refused \u{2014} a firewall or proxy may be blocking it (ECONNREFUSED)",
        "Couldn't connect through your proxy (ERR_PROXY_TUNNEL)",
    ] {
        let lines = stream(&[api_error("unknown", text), result(true, text, None)]);
        let infra = parse_stream(&lines).infra.expect(text);
        assert_eq!(infra.kind, InfraKind::Network, "{text}");
        assert_eq!(infra.raw, text);
    }
}

/// The allowed neighbours: failures of the task itself, and runs that were
/// served in the end, stay ordinary.
#[test]
fn task_failures_limits_and_served_runs_are_not_infra() {
    let cases = [
        // The real successful run.
        fixture("claude_ok.stream.jsonl"),
        // A limit is handled as a limit.
        fixture("claude_limit_hit.synthetic.jsonl"),
        stream(&[
            api_error("rate_limit", "You've hit your session limit"),
            result(true, "You've hit your session limit", Some(429)),
        ]),
        // The agent quoting the wording is not the CLI failing to connect.
        stream(&[
            said("The test failed: Unable to connect to API (ENOTFOUND)"),
            result(true, "The test failed: Unable to connect to API", None),
        ]),
        // Ordinary failed runs stay non-infra.
        result(true, "compile error in main.rs", None),
        // An error on the way, then a successful result.
        stream(&[
            api_error("authentication_failed", "Not logged in"),
            result(false, "done", None),
        ]),
    ];
    for text in cases {
        assert_eq!(parse_stream(&text).infra, None, "{text}");
    }

    // Structured HTTP 5xx / overloaded errors classify as transient Network infra (v3.2).
    for transient in [
        result(true, "server error", Some(500)),
        stream(&[
            api_error("overloaded", "API Error: Overloaded"),
            result(true, "API Error: Overloaded", Some(529)),
        ]),
    ] {
        let infra = parse_stream(&transient).infra;
        assert_eq!(
            infra.as_ref().map(|i| i.kind),
            Some(InfraKind::Network),
            "{transient}"
        );
    }
}

fn fake_claude(dir: &Path, fixture_name: &str, exit_code: i32) -> String {
    let script = format!(
        "#!/bin/sh\ncat '{}'\nexit {exit_code}\n",
        fixture_path(fixture_name).display()
    );
    let path = dir.join("claude");
    write_executable(&path, script);
    path.to_string_lossy().into_owned()
}

fn request(cwd: &str) -> AgentRequest {
    AgentRequest {
        run_id: format!("test-{}", uuid::Uuid::new_v4()),
        prompt: "p".to_string(),
        cwd: cwd.to_string(),
        account: None,
        timeout_secs: 60,
        sandbox: SandboxPolicy::default(),
        json_schema: None,
        max_turns: None,
    }
}

/// A fake `claude` that prints the real not-logged-in output and exits 1,
/// as the real one does.
#[test]
fn a_run_whose_cli_is_not_logged_in_reports_an_auth_failure() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_claude(dir.path(), "claude_not_logged_in.stream.jsonl", 1);
    let outcome = ClaudeCodeRunner::new(bin, ExecEnv::default())
        .run(&request(dir.path().to_str().unwrap()))
        .unwrap();
    assert_eq!(outcome.exit_code, Some(1));
    assert!(!outcome.succeeded());
    assert_eq!(outcome.limit_hit, None);
    let infra = outcome.infra.expect("an auth failure");
    assert_eq!(infra.kind, InfraKind::Auth);
}

#[test]
fn a_run_that_succeeds_has_no_infra_failure() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_claude(dir.path(), "claude_ok.stream.jsonl", 0);
    let outcome = ClaudeCodeRunner::new(bin, ExecEnv::default())
        .run(&request(dir.path().to_str().unwrap()))
        .unwrap();
    assert!(outcome.succeeded());
    assert_eq!(outcome.infra, None);
}

#[test]
fn multi_result_monitor_run_with_trailing_empty_result_preserves_cumulative_tokens() {
    use aaf_engine::executors::claude::parse_stream;

    // Reproduces Claude Code 2.1.x `Monitor` wakeups: multiple per-segment
    // `result` events, followed by a trailing `result` without `usage`, plus
    // an unfinished assistant turn before timeout.
    let stream = [
        serde_json::json!({
            "type": "assistant",
            "message": {
                "id": "msg_1",
                "content": [{"type": "text", "text": "waiting on gradle"}],
                "usage": {"input_tokens": 1000, "cache_creation_input_tokens": 2000, "cache_read_input_tokens": 5000, "output_tokens": 300}
            }
        })
        .to_string(),
        serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": "seg1",
            "usage": {"input_tokens": 1100, "cache_creation_input_tokens": 2100, "cache_read_input_tokens": 5500, "output_tokens": 400}
        })
        .to_string(),
        serde_json::json!({
            "type": "assistant",
            "message": {
                "id": "msg_2",
                "content": [{"type": "text", "text": "monitor wakeup"}],
                "usage": {"input_tokens": 500, "cache_creation_input_tokens": 100, "cache_read_input_tokens": 4000, "output_tokens": 80}
            }
        })
        .to_string(),
        serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": "seg2",
            "usage": {"input_tokens": 500, "cache_creation_input_tokens": 100, "cache_read_input_tokens": 4000, "output_tokens": 80}
        })
        .to_string(),
        // Trailing `result` with no `usage` field must NOT zero prior totals.
        serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": ""
        })
        .to_string(),
        // Unfinished turn before timeout is added on top of settled segments.
        serde_json::json!({
            "type": "assistant",
            "message": {
                "id": "msg_3",
                "content": [{"type": "text", "text": "still running"}],
                "usage": {"input_tokens": 200, "cache_creation_input_tokens": 50, "cache_read_input_tokens": 1000, "output_tokens": 25}
            }
        })
        .to_string(),
    ]
    .join("\n");

    let summary = parse_stream(&stream);
    assert!(summary.saw_result);
    assert!(!summary.is_error);
    assert_eq!(summary.final_text, "seg2");
    assert_eq!(summary.infra, None);
    assert_eq!(summary.limit_hit, None);
    assert_eq!(summary.tokens_in, (1100 + 2100) + (500 + 100) + (200 + 50));
    assert_eq!(summary.tokens_out, 400 + 80 + 25);
    assert_eq!(summary.cache_read_tokens, 5500 + 4000 + 1000);
    assert_eq!(summary.peak_context_tokens, 1000 + 2000 + 5000);
}

#[test]
fn multi_result_preserves_prior_error_and_avoids_false_auth_and_classifies_errors_array() {
    use aaf_domain::ports::InfraKind;
    use aaf_engine::executors::claude::parse_stream;

    // 1. Prior API error must NOT be erased by a trailing empty pointer result.
    let err_then_empty = [
        serde_json::json!({
            "type": "result",
            "subtype": "error_during_execution",
            "is_error": true,
            "api_error_status": 500,
            "result": "API Error: 500 Internal Server Error"
        })
        .to_string(),
        serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": ""
        })
        .to_string(),
    ]
    .join("\n");
    let s1 = parse_stream(&err_then_empty);
    assert!(s1.is_error);
    assert_eq!(s1.infra.as_ref().map(|i| i.kind), Some(InfraKind::Network));
    assert!(s1.final_text.contains("500"));

    // 2. Multi-result run with billed tokens in segment 1 and OAuth prose in a
    // usage-less segment 2 must NOT false-positive as InfraKind::Auth.
    let oauth_prose_after_work = [
        serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": "seg1",
            "usage": {"input_tokens": 10_000, "output_tokens": 500}
        })
        .to_string(),
        serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": "Fixed handling of 'Failed to refresh OAuth token: can't save to the Keychain'"
        })
        .to_string(),
    ]
    .join("\n");
    let s2 = parse_stream(&oauth_prose_after_work);
    assert!(!s2.is_error);
    assert_eq!(s2.infra, None);
    assert!(s2.final_text.contains("Keychain"));

    // 3. `error_during_execution` with empty `result` and network error in `errors[]`
    // classifies as InfraKind::Network instead of unclassified stuck.
    let errors_arr = serde_json::json!({
        "type": "result",
        "subtype": "error_during_execution",
        "is_error": true,
        "result": "",
        "errors": ["API Error: Can't reach the API server — check your internet or DNS (ENOTFOUND)"]
    })
    .to_string();
    let s3 = parse_stream(&errors_arr);
    assert!(s3.is_error);
    assert_eq!(s3.infra.as_ref().map(|i| i.kind), Some(InfraKind::Network));

    // 4. Segment 1 cache_read preserved when result #1 omits cache_read,
    // synthetic zero-context assistant doesn't shadow result_peak_ctx, and
    // late replay of settled msg_1 does not double-count.
    let edge_usage = [
        serde_json::json!({
            "type": "assistant",
            "message": {"id": "msg_0", "model": "<synthetic>", "content": [], "usage": {"input_tokens": 0, "output_tokens": 0}}
        })
        .to_string(),
        serde_json::json!({
            "type": "assistant",
            "message": {"id": "msg_1", "content": [], "usage": {"input_tokens": 100, "cache_read_input_tokens": 5000, "output_tokens": 20}}
        })
        .to_string(),
        serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": "r1",
            "usage": {"input_tokens": 100, "cache_read_input_tokens": 0, "output_tokens": 20}
        })
        .to_string(),
        serde_json::json!({
            "type": "assistant",
            "message": {"id": "msg_1", "content": [], "usage": {"input_tokens": 100, "cache_read_input_tokens": 5000, "output_tokens": 20}}
        })
        .to_string(),
        serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": "r2",
            "usage": {"input_tokens": 200, "cache_read_input_tokens": 4000, "output_tokens": 30}
        })
        .to_string(),
    ]
    .join("\n");
    let s4 = parse_stream(&edge_usage);
    assert_eq!(s4.tokens_in, 300);
    assert_eq!(s4.tokens_out, 50);
    assert_eq!(s4.cache_read_tokens, 9000);
    assert_eq!(s4.peak_context_tokens, 5100);
}
