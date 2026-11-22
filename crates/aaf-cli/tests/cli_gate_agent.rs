//! `aaf gate` started by the agent of an `aaf run`. Inside the sandbox the
//! data directory is closed, and the gate ended with a bare
//! `gate: io: File exists (os error 17)`. The gate of a factory run belongs
//! to the factory; the message has to say so.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "cli_support.rs"]
mod support;

use support::{Sandbox, one_object, write};

/// A data directory the gate cannot use, as the sandbox leaves it for the
/// agent: here a plain file where the directory should be.
fn gate_with_unusable_data_dir(run_id: Option<&str>) -> (i32, String) {
    let sb = Sandbox::new();
    write(
        &sb.repo.join(".autofactory/config.toml"),
        "[commands]\ntest_scoped = \"true\"\n",
    );
    sb.commit_all("configure");
    let blocked = sb.home.join("not-a-directory");
    write(&blocked, "x");
    let mut command = sb.command(&["gate", "--json"]);
    command.env("AAF_DATA_DIR", &blocked);
    if let Some(run_id) = run_id {
        command.env("AAF_RUN_ID", run_id);
    }
    let out = command.output().unwrap();
    let json = one_object(&out);
    (
        out.status.code().unwrap(),
        json["error"].as_str().unwrap_or_default().to_string(),
    )
}

#[test]
fn a_gate_that_fails_inside_an_agent_run_says_whose_job_the_gate_is() {
    let (code, error) = gate_with_unusable_data_dir(Some("0123456789abcdef0123456789abcdef"));

    assert_eq!(code, 1, "{error}");
    assert!(error.starts_with("gate: "), "{error}");
    assert!(
        error.contains("the gate is run by the factory outside the sandbox"),
        "{error}"
    );
    assert!(error.contains("aaf slot -- <command>"), "{error}");
}

#[test]
fn the_same_failure_outside_an_agent_run_is_reported_as_it_is() {
    let (code, error) = gate_with_unusable_data_dir(None);

    assert_eq!(code, 1, "{error}");
    assert!(error.starts_with("gate: "), "{error}");
    assert!(!error.contains("factory"), "{error}");
}
