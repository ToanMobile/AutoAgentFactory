#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Generic runner and runner selection.

use std::ffi::OsString;
use std::time::{Duration, Instant};

use aaf_domain::config::RunConfig;
use aaf_domain::ports::{AgentRequest, AgentRunner, PortError, SandboxPolicy};
use aaf_engine::executors::generic::GenericRunner;
use aaf_engine::executors::{ExecEnv, build_runner_with, find_on_path, reaper};

fn template(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_string()).collect()
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

/// Prints the argument count, then each argument framed by `<` and `>`.
const SHOW_ARGS: &str = "echo \"argc=$#\"; for a in \"$@\"; do printf '<%s>\\n' \"$a\"; done";

#[test]
fn hostile_prompt_arrives_as_one_literal_argument() {
    let dir = tempfile::tempdir().unwrap();
    let prompt = "it's \"quoted\"; touch pwned-semicolon; $(touch pwned-subshell) `touch pwned-backtick` && touch pwned-and | tee pwned-pipe > pwned-redirect {prompt} *";
    let runner = GenericRunner::new(
        template(&["/bin/sh", "-c", SHOW_ARGS, "sh", "{prompt}"]),
        ExecEnv::default(),
    )
    .unwrap();
    let outcome = runner
        .run(&request(prompt, dir.path().to_str().unwrap()))
        .unwrap();

    assert!(outcome.succeeded(), "{outcome:?}");
    assert!(outcome.log_tail.contains("argc=1"), "{}", outcome.log_tail);
    assert!(
        outcome.log_tail.contains(&format!("<{prompt}>")),
        "{}",
        outcome.log_tail
    );
    let created: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
    assert!(created.is_empty(), "the prompt was executed: {created:?}");
    assert_eq!((outcome.tokens_in, outcome.tokens_out), (0, 0));
    assert_eq!(outcome.limit_hit, None);
}

#[test]
fn placeholder_inside_an_argument_stays_one_argument() {
    let runner = GenericRunner::new(
        template(&[
            "/opt/agent",
            "--task={prompt}",
            "--again",
            "{prompt}",
            "fixed",
        ]),
        ExecEnv::default(),
    )
    .unwrap();
    let spec = runner.command(&request("a b --flag", "/work"));
    assert_eq!(spec.program, "/opt/agent");
    assert_eq!(
        spec.args,
        ["--task=a b --flag", "--again", "a b --flag", "fixed"]
    );
}

#[test]
fn exit_code_and_output_are_captured() {
    let dir = tempfile::tempdir().unwrap();
    let runner = GenericRunner::new(
        template(&[
            "/bin/sh",
            "-c",
            "echo \"got $1\"; echo run=$AAF_RUN_ID; pwd; exit 3",
            "sh",
            "{prompt}",
        ]),
        ExecEnv::default(),
    )
    .unwrap();
    let req = request("the-task", dir.path().to_str().unwrap());
    let outcome = runner.run(&req).unwrap();
    assert_eq!(outcome.exit_code, Some(3));
    assert!(!outcome.succeeded() && !outcome.timed_out);
    assert!(outcome.final_text.contains("got the-task"));
    assert!(outcome.log_tail.contains(&format!("run={}", req.run_id)));
    let real = dir.path().canonicalize().unwrap();
    assert!(outcome.log_tail.contains(real.to_str().unwrap()));
}

#[test]
fn timeout_is_reported_and_the_orphan_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let runner = GenericRunner::new(
        template(&[
            "/bin/sh",
            "-c",
            "sleep 300 & echo child=$!; sleep 300",
            "sh",
            "{prompt}",
        ]),
        ExecEnv::default(),
    )
    .unwrap();
    let mut req = request("p", dir.path().to_str().unwrap());
    req.timeout_secs = 1;
    let outcome = runner.run(&req).unwrap();
    assert!(outcome.timed_out);
    assert_eq!(outcome.exit_code, None);
    let pid: u32 = outcome
        .log_tail
        .lines()
        .find_map(|l| l.trim().strip_prefix("child="))
        .unwrap()
        .parse()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while reaper::is_alive(pid).unwrap() {
        assert!(Instant::now() < deadline, "pid {pid} is still alive");
        std::thread::yield_now();
    }
}

#[test]
fn template_must_have_a_fixed_program_and_a_prompt_slot() {
    for bad in [
        template(&[]),
        template(&[""]),
        template(&["{prompt}"]),
        template(&["/bin/echo"]),
        template(&["/bin/echo", "no", "placeholder"]),
        template(&["/bin/{prompt}", "{prompt}"]),
    ] {
        let result = GenericRunner::new(bad.clone(), ExecEnv::default());
        assert!(
            matches!(result, Err(PortError::Invalid(_))),
            "{bad:?} gave {result:?}"
        );
    }
}

fn config(executor: &str, cmd: &[&str]) -> RunConfig {
    RunConfig {
        executor: executor.to_string(),
        executor_cmd: template(cmd),
        ..RunConfig::default()
    }
}

#[test]
fn build_runner_rejects_unknown_executors() {
    for name in ["", "unknown_exec", "bash", "claude-unknown"] {
        let err = build_runner_with(
            &config(name, &["/bin/echo", "{prompt}"]),
            ExecEnv::default(),
        )
        .err()
        .expect("unknown executor accepted");
        assert!(matches!(err, PortError::Invalid(_)), "{name:?}: {err:?}");
    }
}

#[test]
fn build_runner_claude_needs_the_binary() {
    let err = build_runner_with(&config("claude", &[]), ExecEnv::default())
        .err()
        .expect("built without a binary");
    assert!(matches!(err, PortError::NotFound(_)), "{err:?}");

    let env = ExecEnv {
        claude_bin: Some("/opt/bin/claude".to_string()),
        ..ExecEnv::default()
    };
    assert!(build_runner_with(&config("claude", &[]), env).is_ok());
}

#[test]
fn build_runner_generic_executors_need_a_command() {
    for name in ["codex", "antigravity", "opencode", "custom"] {
        assert!(build_runner_with(&config(name, &[]), ExecEnv::default()).is_err());
        let runner = build_runner_with(
            &config(name, &["/bin/echo", "{prompt}"]),
            ExecEnv::default(),
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let outcome = runner
            .run(&request("hello-generic", dir.path().to_str().unwrap()))
            .unwrap();
        assert!(outcome.succeeded());
        assert_eq!(outcome.final_text, "hello-generic");
    }
}

#[test]
fn find_on_path_returns_the_first_regular_file() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a"), dir.path().join("b"));
    std::fs::create_dir_all(a.join("claude")).unwrap(); // a directory, not a binary
    std::fs::create_dir(&b).unwrap();
    std::fs::write(b.join("claude"), "#!/bin/sh\n").unwrap();
    let path_var: OsString =
        std::env::join_paths([a.as_path(), b.as_path(), std::path::Path::new("relative")]).unwrap();
    assert_eq!(find_on_path(&path_var, "claude"), Some(b.join("claude")));
    assert_eq!(find_on_path(&path_var, "codex"), None);
}
