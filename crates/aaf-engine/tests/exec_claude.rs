#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Claude Code runner: stream-json parsing and command construction.
//!
//! Fixtures in `tests/fixtures/exec`:
//! - `claude_ok.stream.jsonl`: REAL output of Claude Code 2.1.330 for
//!   `claude -p 'Reply with the single word ok' --output-format stream-json
//!   --verbose`, with ids, paths, timestamps and the tool/plugin/skill lists
//!   scrubbed.
//! - `claude_not_logged_in.stream.jsonl`: REAL output of the same version run
//!   with `--bare` and no API key (an error result that is not a limit).
//! - `claude_limit_hit.synthetic.jsonl`: NOT from a real run. A limit cannot
//!   be triggered on demand; the events are hand-written from the shapes of
//!   the real fixtures plus the status enum, error enum and message wording
//!   found in the 2.1.330 binary.

use std::path::{Path, PathBuf};

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::ports::{AgentRequest, AgentRunner, PortError, SandboxPolicy};
use aaf_engine::executors::ExecEnv;
use aaf_engine::executors::claude::{ClaudeCodeRunner, StreamParser, parse_stream};

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

fn request(prompt: &str, cwd: &str) -> AgentRequest {
    AgentRequest {
        run_id: format!("test-{}", uuid::Uuid::new_v4()),
        prompt: prompt.to_string(),
        cwd: cwd.to_string(),
        account: None,
        timeout_secs: 60,
        sandbox: SandboxPolicy::default(),
        json_schema: None,
        max_turns: None,
    }
}

fn account(provider: ProviderKind, config_dir: &str) -> AccountProfile {
    AccountProfile {
        id: "acct-1".to_string(),
        provider,
        config_dir: config_dir.to_string(),
        name: "acct".to_string(),
    }
}

#[test]
fn real_fixture_yields_text_and_tokens() {
    let summary = parse_stream(&fixture("claude_ok.stream.jsonl"));
    assert_eq!(summary.final_text, "ok");
    // input 2 + cache creation 17781; the 11842 cache-read tokens are not counted.
    assert_eq!(summary.tokens_in, 17_783);
    assert_eq!(summary.tokens_out, 4);
    assert_eq!(summary.limit_hit, None);
    assert!(summary.saw_result && !summary.is_error);
}

#[test]
fn real_error_fixture_is_an_error_but_not_a_limit() {
    let summary = parse_stream(&fixture("claude_not_logged_in.stream.jsonl"));
    assert!(summary.saw_result && summary.is_error);
    assert_eq!(summary.final_text, "Not logged in · Please run /login");
    assert_eq!(summary.limit_hit, None);
    assert_eq!((summary.tokens_in, summary.tokens_out), (0, 0));
}

#[test]
fn synthetic_limit_fixture_is_a_limit_hit_with_reset_time() {
    let summary = parse_stream(&fixture("claude_limit_hit.synthetic.jsonl"));
    let hit = summary.limit_hit.expect("limit detected");
    assert_eq!(hit.reset_at, Some(1_795_292_400));
    assert_eq!(hit.raw, "You've hit your session limit");
    assert!(summary.is_error);
}

#[test]
fn garbage_lines_are_skipped_without_panicking() {
    let mut parser = StreamParser::default();
    for line in [
        "",
        "   ",
        "plain text from a wrapper script",
        "\u{1b}[31mANSI noise\u{1b}[0m",
        "{\"type\":\"result\",\"result\":\"cut off",
        "[1,2,3]",
        "42",
        "null",
        "\"result\"",
        "{}",
        "{\"type\":7}",
        "{\"type\":\"result\",\"result\":{\"not\":\"a string\"},\"usage\":\"nope\",\"is_error\":\"yes\"}",
        "{\"type\":\"assistant\",\"message\":\"not an object\"}",
        "{\"type\":\"assistant\",\"message\":{\"content\":\"text\",\"usage\":{\"input_tokens\":-5,\"output_tokens\":1.5}}}",
        "{\"type\":\"rate_limit_event\",\"rate_limit_info\":[\"rejected\"]}",
        "{\"type\":\"unknown_future_event\",\"result\":\"ignored\"}",
    ] {
        parser.feed_line(line);
    }
    let summary = parser.summary();
    // The malformed result object still counts as "a result arrived".
    assert!(summary.saw_result);
    assert_eq!(summary.final_text, "");
    assert_eq!((summary.tokens_in, summary.tokens_out), (0, 0));
    assert_eq!(summary.limit_hit, None);
}

fn assistant(
    id: &str,
    text: &str,
    input: u64,
    cache_new: u64,
    cache_read: u64,
    out: u64,
) -> String {
    serde_json::json!({
        "type": "assistant",
        "message": {
            "id": id,
            "content": [{"type": "text", "text": text}],
            "usage": {
                "input_tokens": input,
                "cache_creation_input_tokens": cache_new,
                "cache_read_input_tokens": cache_read,
                "output_tokens": out,
            },
        },
    })
    .to_string()
}

#[test]
fn without_a_result_event_assistant_usage_is_summed_once_per_message() {
    // What a timed-out run leaves: message A streamed as two events with the
    // same usage, then message B.
    let stream = [
        assistant("msg_a", "thinking", 10, 100, 9_000, 5),
        assistant("msg_a", "still A", 10, 100, 9_000, 7),
        assistant("msg_b", "partial answer", 1, 20, 9_100, 30),
    ]
    .join("\n");
    let summary = parse_stream(&stream);
    assert!(!summary.saw_result);
    assert_eq!(summary.tokens_in, 10 + 100 + 1 + 20);
    assert_eq!(summary.tokens_out, 7 + 30);
    assert_eq!(summary.final_text, "partial answer");
    assert_eq!(summary.limit_hit, None);
}

#[test]
fn result_totals_win_over_assistant_usage() {
    let stream = [
        assistant("msg_a", "a", 10, 100, 0, 5),
        serde_json::json!({
            "type": "result", "is_error": false, "result": "done",
            "usage": {"input_tokens": 3, "cache_creation_input_tokens": 4,
                      "cache_read_input_tokens": 999, "output_tokens": 6},
        })
        .to_string(),
    ]
    .join("\r\n");
    let summary = parse_stream(&stream);
    assert_eq!((summary.tokens_in, summary.tokens_out), (7, 6));
    assert_eq!(summary.final_text, "done");
}

fn result(is_error: bool, text: &str, status: Option<i64>) -> String {
    serde_json::json!({
        "type": "result", "is_error": is_error, "result": text, "api_error_status": status,
    })
    .to_string()
}

fn rate_event(status: &str, resets_at: i64) -> String {
    serde_json::json!({
        "type": "rate_limit_event",
        "rate_limit_info": {"status": status, "resetsAt": resets_at},
    })
    .to_string()
}

#[test]
fn limit_wording_in_an_error_result_is_a_limit_hit() {
    for (text, reset_at) in [
        (
            "Claude AI usage limit reached|1760000000",
            Some(1_760_000_000),
        ),
        ("Usage limit reached", None),
        ("You've hit your monthly spend limit.", None),
        ("You're out of usage credits. Switch to another model", None),
        // Not ten digits after the bar: not taken for a timestamp.
        ("usage limit reached|soon", None),
        ("usage limit reached|123", None),
    ] {
        let hit = parse_stream(&result(true, text, None))
            .limit_hit
            .unwrap_or_else(|| panic!("{text:?} not detected"));
        assert_eq!(hit.reset_at, reset_at, "{text:?}");
        assert_eq!(hit.raw, text);
    }
}

#[test]
fn http_429_or_rate_limit_error_flag_is_a_limit_hit() {
    let hit = parse_stream(&result(true, "API Error: too many requests", Some(429))).limit_hit;
    assert_eq!(hit.unwrap().reset_at, None);

    let flagged = serde_json::json!({
        "type": "assistant", "error": "rate_limit",
        "message": {"id": "m", "content": [{"type": "text", "text": "limit text"}]},
    })
    .to_string();
    let hit = parse_stream(&flagged).limit_hit.unwrap();
    assert_eq!((hit.reset_at, hit.raw.as_str()), (None, "limit text"));
}

#[test]
fn rejected_rate_event_counts_only_when_the_run_failed() {
    // Killed before any result: the rejected window is the evidence.
    let hit = parse_stream(&rate_event("rejected", 1_795_000_000)).limit_hit;
    assert_eq!(hit.unwrap().reset_at, Some(1_795_000_000));

    // Served anyway (overage): not a limit.
    let served = [
        rate_event("rejected", 1_795_000_000),
        result(false, "ok", None),
    ]
    .join("\n");
    assert_eq!(parse_stream(&served).limit_hit, None);

    // The status seen in every normal run.
    for status in ["allowed", "allowed_warning"] {
        let stream = [
            rate_event(status, 1_795_000_000),
            result(true, "boom", None),
        ]
        .join("\n");
        assert_eq!(parse_stream(&stream).limit_hit, None, "{status}");
    }
}

#[test]
fn ordinary_answers_and_errors_are_not_limit_hits() {
    // A successful answer may talk about limits (this project does).
    let chatty = result(
        false,
        "Fixed: the router now handles 'usage limit reached'",
        None,
    );
    assert_eq!(parse_stream(&chatty).limit_hit, None);
    for text in [
        "Not logged in · Please run /login",
        "context limit reached",
        "",
    ] {
        assert_eq!(
            parse_stream(&result(true, text, None)).limit_hit,
            None,
            "{text:?}"
        );
    }
    assert_eq!(
        parse_stream(&result(true, "server error", Some(500))).limit_hit,
        None
    );
}

#[test]
fn limit_message_is_truncated_in_raw() {
    let long = format!("usage limit reached {}", "é".repeat(2_000));
    let hit = parse_stream(&result(true, &long, None)).limit_hit.unwrap();
    assert_eq!(hit.raw.chars().count(), 500);
}

fn runner() -> ClaudeCodeRunner {
    ClaudeCodeRunner::new("/opt/bin/claude", ExecEnv::default())
}

#[test]
fn command_for_an_unsandboxed_run_denies_prompts_instead_of_bypassing() {
    let spec = runner().command(&request("do the task", "/work")).unwrap();
    assert_eq!(spec.program, "/opt/bin/claude");
    assert_eq!(
        spec.args,
        [
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--permission-mode",
            "acceptEdits",
            "--permission-prompts",
            "none",
            "--",
            "do the task",
        ]
    );
    assert!(
        spec.env.is_empty(),
        "no account: CLAUDE_CONFIG_DIR is left alone"
    );
}

#[test]
fn command_for_a_sandboxed_run_bypasses_permission_prompts() {
    let mut req = request("do the task", "/work");
    req.sandbox.enabled = true;
    let args = runner().command(&req).unwrap().args;
    let mode = args.iter().position(|a| a == "--permission-mode").unwrap();
    assert_eq!(args[mode + 1], "bypassPermissions");
    assert!(!args.contains(&"--permission-prompts".to_string()));
}

#[test]
fn prompt_is_one_trailing_argument_even_when_it_looks_like_a_flag() {
    let prompt = "--dangerously-skip-permissions\n-p \"x\" $(id); rm -rf /";
    let args = runner().command(&request(prompt, "/work")).unwrap().args;
    let separator = args.iter().position(|a| a == "--").unwrap();
    assert_eq!(&args[separator + 1..], [prompt]);
}

#[test]
fn account_sets_config_dir_and_must_be_a_claude_account() {
    let mut req = request("p", "/work");
    req.account = Some(account(ProviderKind::ClaudeCode, "/cfg/claude-b"));
    let spec = runner().command(&req).unwrap();
    assert_eq!(
        spec.env,
        [("CLAUDE_CONFIG_DIR".to_string(), "/cfg/claude-b".to_string())]
    );

    req.account = Some(account(ProviderKind::Codex, "/cfg/codex"));
    assert!(matches!(runner().command(&req), Err(PortError::Invalid(_))));
    assert!(matches!(
        runner().command(&request("", "/work")),
        Err(PortError::Invalid(_))
    ));
}

#[test]
fn model_and_effort_become_flags_and_a_value_that_reads_as_a_flag_is_refused() {
    let args = runner()
        .with_model_and_effort("opus", "high")
        .command(&request("do the task", "/work"))
        .unwrap()
        .args;
    let tail = &args[args.len() - 6..];
    assert_eq!(
        tail,
        ["--model", "opus", "--effort", "high", "--", "do the task"]
    );

    let only_model = runner()
        .with_model_and_effort("claude-x[1m]", "")
        .command(&request("p", "/work"))
        .unwrap()
        .args;
    assert!(only_model.contains(&"claude-x[1m]".to_string()));
    assert!(!only_model.contains(&"--effort".to_string()));

    for (model, effort) in [
        ("--dangerously-skip-permissions", ""),
        ("opus --bare", ""),
        ("", "-x"),
        ("", "high\n"),
    ] {
        let result = runner()
            .with_model_and_effort(model, effort)
            .command(&request("p", "/work"));
        assert!(
            matches!(result, Err(PortError::Invalid(_))),
            "{model:?} {effort:?}"
        );
    }
}

/// `run.model` and `run.effort` of the repo config, as the started process
/// sees them.
#[test]
fn the_configured_model_and_effort_reach_the_claude_command_line() {
    use aaf_domain::config::RunConfig;
    use aaf_engine::executors::build_runner_with;

    let dir = tempfile::tempdir().unwrap();
    let env = ExecEnv {
        claude_bin: Some(fake_claude(dir.path(), "claude_ok.stream.jsonl", 0)),
        ..ExecEnv::default()
    };
    let mut req = request("p", dir.path().to_str().unwrap());
    req.account = Some(account(
        ProviderKind::ClaudeCode,
        dir.path().to_str().unwrap(),
    ));
    let seen = |cfg: &RunConfig| {
        let runner = build_runner_with(cfg, env.clone()).unwrap();
        assert!(runner.run(&req).unwrap().succeeded());
        std::fs::read_to_string(dir.path().join("seen")).unwrap()
    };

    let plain = seen(&RunConfig::default());
    assert!(
        !plain.contains("--model") && !plain.contains("--effort"),
        "{plain}"
    );

    let set = seen(&RunConfig {
        model: "sonnet".to_string(),
        effort: "low".to_string(),
        ..RunConfig::default()
    });
    assert!(
        set.contains("arg=--model\narg=sonnet\narg=--effort\narg=low\narg=--\n"),
        "{set}"
    );
}

/// A stand-in `claude` that records what it was given and replays a fixture.
fn fake_claude(dir: &Path, fixture_name: &str, exit_code: i32) -> String {
    let script = format!(
        "#!/bin/sh\n{{ echo \"run=$AAF_RUN_ID\"; for a in \"$@\"; do printf 'arg=%s\\n' \"$a\"; done; }} > \"$CLAUDE_CONFIG_DIR/seen\"\necho 'warning: not json'\ncat '{}'\nexit {exit_code}\n",
        fixture_path(fixture_name).display()
    );
    let path = dir.join("claude");
    write_executable(&path, script);
    path.to_string_lossy().into_owned()
}

#[test]
fn run_parses_the_stream_and_passes_run_id_config_dir_and_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let (work, config) = (dir.path().join("work"), dir.path().join("config"));
    std::fs::create_dir(&work).unwrap();
    std::fs::create_dir(&config).unwrap();
    let bin = fake_claude(dir.path(), "claude_ok.stream.jsonl", 0);
    let prompt = "say \"ok\"; $(touch pwned) `id`";

    let mut req = request(prompt, work.to_str().unwrap());
    req.account = Some(account(ProviderKind::ClaudeCode, config.to_str().unwrap()));
    let outcome = ClaudeCodeRunner::new(bin, ExecEnv::default())
        .run(&req)
        .unwrap();

    assert!(outcome.succeeded(), "{outcome:?}");
    assert_eq!(outcome.final_text, "ok");
    assert_eq!((outcome.tokens_in, outcome.tokens_out), (17_783, 4));
    assert!(outcome.log_tail.contains("warning: not json"));

    let seen = std::fs::read_to_string(config.join("seen")).unwrap();
    assert!(seen.starts_with(&format!("run={}\n", req.run_id)), "{seen}");
    assert!(seen.ends_with(&format!("arg=--\narg={prompt}\n")), "{seen}");
    assert!(!work.join("pwned").exists());
}

#[test]
fn run_reports_a_limit_hit_and_the_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_claude(dir.path(), "claude_limit_hit.synthetic.jsonl", 1);
    let mut req = request("p", dir.path().to_str().unwrap());
    req.account = Some(account(
        ProviderKind::ClaudeCode,
        dir.path().to_str().unwrap(),
    ));
    let outcome = ClaudeCodeRunner::new(bin, ExecEnv::default())
        .run(&req)
        .unwrap();
    assert_eq!(outcome.exit_code, Some(1));
    assert!(!outcome.succeeded());
    assert_eq!(outcome.limit_hit.unwrap().reset_at, Some(1_795_292_400));
}

#[test]
fn run_rejects_bad_requests_before_spawning() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_claude(dir.path(), "claude_ok.stream.jsonl", 0);
    let runner = ClaudeCodeRunner::new(bin, ExecEnv::default());
    let cwd = dir.path().to_str().unwrap();

    let mut bad_id = request("p", cwd);
    bad_id.run_id = "id with space".to_string();
    assert!(matches!(runner.run(&bad_id), Err(PortError::Invalid(_))));

    let mut no_timeout = request("p", cwd);
    no_timeout.timeout_secs = 0;
    assert!(matches!(
        runner.run(&no_timeout),
        Err(PortError::Invalid(_))
    ));

    let missing = request("p", "/nonexistent/aaf-worktree");
    assert!(matches!(runner.run(&missing), Err(PortError::NotFound(_))));
}

/// Smoke test against the installed CLI, run by hand:
/// `cargo test -p aaf-engine --test exec_claude -- --ignored`.
/// `--bare` reads no login, so the run ends with "Not logged in" before any
/// API call and costs nothing; what it proves is that the real binary starts
/// under the pty and the Seatbelt profile and that its stream is parsed.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "needs the real claude binary"]
fn real_cli_streams_json_under_pty_and_sandbox() {
    use aaf_engine::executors::pty::run_pty;
    use aaf_engine::executors::sandbox::{SandboxPaths, seatbelt_profile, wrap};
    use std::sync::{Arc, Mutex};

    let env = ExecEnv::from_process();
    let bin = env.claude_bin.clone().expect("claude on PATH");
    let dir = tempfile::tempdir().unwrap();
    let mut req = request("--version", dir.path().to_str().unwrap());
    req.sandbox.enabled = true;
    req.account = Some(account(
        ProviderKind::ClaudeCode,
        dir.path().to_str().unwrap(),
    ));
    let mut spec = ClaudeCodeRunner::new(bin, env.clone())
        .command(&req)
        .unwrap();
    spec.args.insert(0, "--bare".to_string());
    spec.cwd = Some(req.cwd.clone());

    let mut writable = env.temp_dirs.clone();
    writable.push(req.cwd.clone());
    let extra = SandboxPaths {
        writable,
        ..SandboxPaths::default()
    };
    let profile = seatbelt_profile(&req.sandbox, &extra).unwrap();
    let parser = Arc::new(Mutex::new(StreamParser::default()));
    let feed = Arc::clone(&parser);
    let out = run_pty(
        &wrap(&spec, &profile).unwrap(),
        std::time::Duration::from_secs(120),
        64 * 1024,
        Some(Box::new(move |line| feed.lock().unwrap().feed_line(line))),
    )
    .unwrap();
    let summary = parser.lock().unwrap().summary();
    assert!(!out.timed_out, "{}", out.tail);
    assert!(summary.saw_result, "{}", out.tail);
    assert!(
        out.tail
            .contains("\"permissionMode\":\"bypassPermissions\"")
    );
    assert!(summary.final_text.contains("Not logged in"), "{summary:?}");
}

#[test]
fn read_only_command_uses_slim_flags_without_stripping_inspection_tools() {
    let mut ro = request("audit the diff", "/work");
    ro.sandbox.enabled = true;
    ro.sandbox.read_only = true;
    let args = runner().command(&ro).unwrap().args;

    let pos = |flag: &str| args.iter().position(|a| a == flag);
    let mode_idx = pos("--permission-mode").expect("permission mode");
    assert_eq!(args[mode_idx + 1], "plan");
    assert!(
        args.contains(&"--no-session-persistence".to_string()),
        "read-only runs must skip session persistence: {args:?}"
    );
    assert!(
        args.contains(&"--strict-mcp-config".to_string()),
        "read-only runs must fail-closed without external MCP schemas: {args:?}"
    );
    let disallowed_idx = pos("--disallowedTools").expect("disallowedTools");
    let disallowed = &args[disallowed_idx + 1];
    for blocked in ["Edit", "Write", "NotebookEdit", "WebFetch", "WebSearch"] {
        assert!(
            disallowed.contains(blocked),
            "missing {blocked}: {disallowed}"
        );
    }
    assert!(
        disallowed.contains("Agent") && disallowed.contains("Bash"),
        "Agent and Bash must be disallowed on read-only runs: {disallowed}"
    );
    assert!(
        !args.contains(&"--tools".to_string()),
        "never strip read-only inspection tools via --tools: {args:?}"
    );

    let mut rw = request("implement feature", "/work");
    rw.sandbox.enabled = true;
    let rw_args = runner().command(&rw).unwrap().args;
    assert!(!rw_args.contains(&"--strict-mcp-config".to_string()));
    assert!(!rw_args.contains(&"--no-session-persistence".to_string()));
    assert!(!rw_args.contains(&"--disallowedTools".to_string()));
}
