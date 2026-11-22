#![allow(clippy::unwrap_used, clippy::expect_used)]
//! What of `aaf run`'s own environment reaches the agent: an allow-list,
//! not everything (Wave 3: the agent inherited tokens and `AAF_*` state of
//! the shell that started `aaf run`). Real binary; the "agent" is a shell
//! one-liner that dumps its environment.

#[path = "factory_support.rs"]
mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde_json::json;
use support::{Fixture, git};

/// Commits a config whose custom executor writes its environment to `dump`.
fn configure(fx: &Fixture, dump: &Path, extra_run: &str) {
    let dir = fx.repo.join(".autofactory");
    fs::create_dir_all(&dir).unwrap();
    let script = format!("env > '{}'; echo made > done.txt", dump.display());
    let config = format!(
        "[commands]\ntest_scoped = \"test -f done.txt\"\n\n[run]\nexecutor = \"custom\"\n\
         executor_cmd = [\"/bin/sh\", \"-c\", {}, \"{{prompt}}\"]\nrun_timeout_secs = 60\n{extra_run}",
        json!(script)
    );
    fs::write(dir.join("config.toml"), config).unwrap();
    git(&fx.repo, &["add", "-A"]);
    git(&fx.repo, &["commit", "--quiet", "-m", "configure aaf"]);
}

const INHERITED: [(&str, &str); 12] = [
    ("GITHUB_TOKEN", "gh-one"),
    ("GH_TOKEN", "gh-two"),
    ("ANTHROPIC_API_KEY", "anthropic-key"),
    ("CLAUDE_CODE_OAUTH_TOKEN", "oauth"),
    ("CLAUDE_CODE_ENTRYPOINT", "cli"),
    ("CLAUDE_CONFIG_DIR", "/somebody/elses/profile"),
    ("AAF_HEAVY_SLOT", "0:0123456789abcdef0123456789abcdef"),
    ("AAF_AUTO_MERGE", "false"),
    ("OWNER_PRIVATE", "private"),
    ("JAVA_HOME", "/opt/java"),
    ("ANDROID_HOME", "/opt/android"),
    ("LC_ALL", "C"),
];

fn run_and_dump(fx: &Fixture, dump: &Path) -> BTreeMap<String, String> {
    let id = fx.add("make the file");
    let out = fx.aaf_in(&fx.repo, &["run", "--unsandboxed"], &INHERITED);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["review"], json!([id]), "{}", out.stderr);
    fs::read_to_string(dump)
        .unwrap()
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

#[test]
fn the_agent_gets_an_allow_list_of_the_environment_not_all_of_it() {
    let fx = Fixture::new();
    let dump = fx.root().join("agent-env.txt");
    configure(&fx, &dump, "");

    let env = run_and_dump(&fx, &dump);

    for name in [
        "GITHUB_TOKEN",
        "GH_TOKEN",
        "ANTHROPIC_API_KEY",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CONFIG_DIR",
        "AAF_HEAVY_SLOT",
        "AAF_AUTO_MERGE",
        "AAF_WORKTREE_DIR",
        "OWNER_PRIVATE",
    ] {
        assert_eq!(env.get(name), None, "{name} reached the agent");
    }
    // What builds and tools need is still there.
    assert_eq!(env.get("JAVA_HOME").map(String::as_str), Some("/opt/java"));
    assert_eq!(
        env.get("ANDROID_HOME").map(String::as_str),
        Some("/opt/android")
    );
    assert_eq!(env.get("LC_ALL").map(String::as_str), Some("C"));
    assert!(env.contains_key("PATH"));
    assert_eq!(
        env.get("HOME").map(String::as_str),
        fx.root().join("home").to_str()
    );
    // What aaf sets itself: the run id, and the data dir it really uses, so
    // `aaf-hook` and `aaf slot` inside the run agree with the factory.
    assert!(env.contains_key("AAF_RUN_ID"));
    assert_eq!(
        env.get("AAF_DATA_DIR").map(String::as_str),
        fx.data.to_str()
    );
}

#[test]
fn a_variable_named_in_run_env_pass_reaches_the_agent() {
    let fx = Fixture::new();
    let dump = fx.root().join("agent-env.txt");
    configure(&fx, &dump, "env_pass = [\"OWNER_PRIVATE\"]\n");

    let env = run_and_dump(&fx, &dump);

    assert_eq!(
        env.get("OWNER_PRIVATE").map(String::as_str),
        Some("private")
    );
    assert_eq!(env.get("GITHUB_TOKEN"), None);
}

#[test]
fn a_variable_named_in_the_machine_config_reaches_the_agent() {
    let fx = Fixture::new();
    let dump = fx.root().join("agent-env.txt");
    configure(&fx, &dump, "");
    // Tier 1: the operator's own file, outside every repository and task.
    fs::write(
        fx.root().join("home/.config/autofactory/config.toml"),
        "[machine]\nmax_load_percent = 0\n\n[run]\nenv_pass = [\"OWNER_PRIVATE\"]\n",
    )
    .unwrap();

    let env = run_and_dump(&fx, &dump);

    assert_eq!(
        env.get("OWNER_PRIVATE").map(String::as_str),
        Some("private")
    );
    assert_eq!(env.get("GITHUB_TOKEN"), None);
}

/// Owner decision: a credential (and `NODE_OPTIONS`) reaches the agent when
/// the operator names it in the machine config, and only from there.
#[test]
fn a_credential_named_in_the_machine_config_reaches_the_agent() {
    let fx = Fixture::new();
    let dump = fx.root().join("agent-env.txt");
    configure(&fx, &dump, "");
    fs::write(
        fx.root().join("home/.config/autofactory/config.toml"),
        "[machine]\nmax_load_percent = 0\n\n[run]\nenv_pass = [\"GITHUB_TOKEN\", \"NODE_OPTIONS\"]\n",
    )
    .unwrap();

    let id = fx.add("make the file");
    let out = fx.aaf_in(
        &fx.repo,
        &["run", "--unsandboxed"],
        &[
            ("GITHUB_TOKEN", "gh-one"),
            ("GH_TOKEN", "gh-two"),
            ("NODE_OPTIONS", "--max-old-space-size=64"),
        ],
    );

    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["review"], json!([id]), "{}", out.stderr);
    let text = fs::read_to_string(&dump).unwrap();
    assert!(text.lines().any(|l| l == "GITHUB_TOKEN=gh-one"), "{text}");
    assert!(
        text.lines()
            .any(|l| l == "NODE_OPTIONS=--max-old-space-size=64"),
        "{text}"
    );
    // Only the names the operator gave.
    assert!(!text.contains("GH_TOKEN"), "{text}");
}

#[test]
fn the_repository_config_cannot_name_a_credential_for_the_agent() {
    let fx = Fixture::new();
    let dump = fx.root().join("agent-env.txt");
    // Tier 2 is committed: a task can propose this line.
    configure(&fx, &dump, "env_pass = [\"GITHUB_TOKEN\"]\n");
    let id = fx.add("make the file");

    let out = fx.aaf_in(&fx.repo, &["run", "--unsandboxed"], &INHERITED);

    assert_eq!(out.code, 1, "{}", out.stderr);
    let error = out.json["error"].as_str().unwrap_or_default();
    assert!(error.contains("GITHUB_TOKEN"), "{}", out.stderr);
    assert!(!dump.exists(), "an agent was started");
    assert_eq!(fx.state(&id), "ready");
}

#[test]
fn the_local_file_cannot_widen_what_reaches_the_agent() {
    let fx = Fixture::new();
    let dump = fx.root().join("agent-env.txt");
    configure(&fx, &dump, "");
    fs::write(
        fx.repo.join(".autofactory/config.local.toml"),
        // One name the committed file could have set, one it could not:
        // from this tier neither is taken.
        "[run]\nenv_pass = [\"OWNER_PRIVATE\", \"GITHUB_TOKEN\"]\n",
    )
    .unwrap();
    fs::write(
        fx.repo.join(".git/info/exclude"),
        ".autofactory/config.local.toml\n",
    )
    .unwrap();

    let id = fx.add("make the file");
    let out = fx.aaf_in(&fx.repo, &["run", "--unsandboxed"], &INHERITED);

    assert_eq!(out.json["review"], json!([id]), "{}", out.stderr);
    let warnings = out.json["warnings"].to_string();
    assert!(warnings.contains("run.env_pass"), "{warnings}");
    let text = fs::read_to_string(&dump).unwrap();
    assert!(!text.contains("GITHUB_TOKEN"), "{text}");
    assert!(!text.contains("OWNER_PRIVATE"), "{text}");
}
