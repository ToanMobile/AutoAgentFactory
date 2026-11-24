#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cell::Cell;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use aaf_hook::{Decision, HookEnv, decide};
use serde_json::json;

fn percentile(sorted: &[Duration], percent: usize) -> Duration {
    sorted[(sorted.len() * percent / 100).min(sorted.len() - 1)]
}

/// The fast path: a command no guard cares about is decided in-process in
/// well under a millisecond and never asks git for the repository root.
#[test]
fn a_non_matching_command_is_decided_in_under_a_millisecond() {
    let dir = tempfile::tempdir().unwrap();
    let lookups = Cell::new(0u32);
    let lookup = |_: &Path| -> Option<PathBuf> {
        lookups.set(lookups.get() + 1);
        None
    };
    let config_lookups = Cell::new(0u32);
    let config = |_: &Path| -> Option<String> {
        config_lookups.set(config_lookups.get() + 1);
        Some(String::new())
    };
    let env = HookEnv {
        repo_root: &lookup,
        git_config: &config,
        data_dir: Some(dir.path().join("data")),
        config_dir: Some(dir.path().join("config")),
        home: Some(dir.path().join("home")),
        cwd: None,
        in_factory_run: true,
        trusted_aaf_bins: Vec::new(),
    };
    let payloads: Vec<Vec<u8>> = [
        "cargo test -p aaf-hook --all-targets -- --nocapture",
        "ls -la src && grep -rn \"TODO\" src | head -n 20",
        "git status --short && git log --oneline -n 5",
        "rm -rf build && mkdir -p build && cp -r assets build/",
    ]
    .iter()
    .map(|command| {
        json!({"tool_name": "Bash", "tool_input": {"command": command}, "cwd": dir.path()})
            .to_string()
            .into_bytes()
    })
    .collect();

    let mut samples = Vec::new();
    for round in 0..2_000 {
        let payload = &payloads[round % payloads.len()];
        let start = Instant::now();
        let decision = decide(payload, &env);
        samples.push(start.elapsed());
        assert_eq!(decision, Decision::Allow);
    }
    samples.sort();
    let median = percentile(&samples, 50);
    assert!(median < Duration::from_millis(1), "median {median:?}");
    assert_eq!(lookups.get(), 0, "the fast path looked up the repo root");
    assert_eq!(
        config_lookups.get(),
        0,
        "a builtin git command read the git configuration"
    );
}

/// Wall-clock cost of one hook invocation including process spawn.
/// Measured, not asserted:
/// `cargo test --release -p aaf-hook --test perf -- --ignored --nocapture`
#[test]
#[ignore = "benchmark: prints spawn latency, asserts nothing about it"]
fn print_spawn_latency() {
    let dir = tempfile::tempdir().unwrap();
    let payload = json!({
        "tool_name": "Bash",
        "tool_input": {"command": "cargo test -p aaf-hook --all-targets"},
        "cwd": dir.path(),
    })
    .to_string();
    let mut samples = Vec::new();
    for _ in 0..300 {
        let start = Instant::now();
        let mut child = Command::new(env!("CARGO_BIN_EXE_aaf-hook"))
            .env("AAF_DATA_DIR", dir.path().join("data"))
            .env("AAF_CONFIG_DIR", dir.path().join("config"))
            .env("HOME", dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        samples.push(start.elapsed());
        assert_eq!(output.status.code(), Some(0));
    }
    samples.sort();
    println!(
        "aaf-hook spawn latency over {} runs: p50 {:?}, p99 {:?}, max {:?}",
        samples.len(),
        percentile(&samples, 50),
        percentile(&samples, 99),
        samples[samples.len() - 1],
    );
}
