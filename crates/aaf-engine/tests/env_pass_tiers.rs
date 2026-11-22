//! Owner decision (SECURITY.md §10): credential-shaped names and
//! `NODE_OPTIONS` reach the agent only when the machine config
//! (`~/.config/autofactory/config.toml`, tier 1) names them. The repository
//! config and a hand-built environment may not, and every other refusal holds
//! for the machine config too.

use aaf_domain::ports::PortError;
use aaf_engine::executors::{ExecEnv, agent_env, build_runner_with};

fn custom_run(env_pass: &[&str]) -> aaf_domain::config::RunConfig {
    aaf_domain::config::RunConfig {
        executor: "custom".to_string(),
        executor_cmd: vec!["/bin/echo".to_string(), "{prompt}".to_string()],
        env_pass: names(env_pass),
        ..aaf_domain::config::RunConfig::default()
    }
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|name| name.to_string()).collect()
}

fn machine(list: &[&str]) -> ExecEnv {
    ExecEnv {
        machine_env_pass: names(list),
        ..ExecEnv::default()
    }
}

#[test]
fn the_machine_config_may_name_credentials_and_node_options_and_nothing_else_more() {
    let accepted = [
        "GITHUB_TOKEN",
        "ANTHROPIC_API_KEY",
        "DB_PASSWORD",
        "NODE_OPTIONS",
    ];
    assert!(build_runner_with(&custom_run(&[]), machine(&accepted)).is_ok());
    // What it names reaches the agent.
    let env = ExecEnv {
        parent_env: vec![("GITHUB_TOKEN".to_string(), "gh".to_string())],
        ..machine(&["GITHUB_TOKEN"])
    };
    assert!(agent_env(&env, &[]).contains(&("GITHUB_TOKEN".to_string(), "gh".to_string())));

    // Every other refusal holds for the machine config too.
    for name in [
        "GIT_DIR",
        "GIT_SSH_COMMAND",
        "DYLD_INSERT_LIBRARIES",
        "AAF_DATA_DIR",
        "PATH",
        "HOME",
        "PYTHONSTARTUP",
        "BASH_ENV",
        "JAVA_TOOL_OPTIONS",
        "PYTHONPATH",
        "NODE_PATH",
    ] {
        let err = build_runner_with(&custom_run(&[]), machine(&[name]))
            .err()
            .unwrap_or_else(|| panic!("machine config {name:?} was accepted"));
        let PortError::Invalid(message) = err else {
            panic!("{name:?}: {err:?}");
        };
        assert!(message.contains("machine config"), "{message}");
    }
}

#[test]
fn the_repository_config_and_a_hand_built_runner_may_not_name_them() {
    for name in [
        "GITHUB_TOKEN",
        "ANTHROPIC_API_KEY",
        "NODE_OPTIONS",
        "node_options",
    ] {
        // Tier 2 (the committed repository config).
        let err = build_runner_with(&custom_run(&[name]), ExecEnv::default())
            .err()
            .unwrap_or_else(|| panic!("repository config {name:?} was accepted"));
        assert!(
            format!("{err:?}").contains("only the machine config"),
            "{err:?}"
        );
        // Named by whoever built the environment, not as the machine's.
        let by_hand = ExecEnv {
            env_pass: names(&[name]),
            ..ExecEnv::default()
        };
        assert!(
            build_runner_with(&custom_run(&[]), by_hand).is_err(),
            "{name:?}"
        );
    }
}
