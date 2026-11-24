#![allow(clippy::unwrap_used, clippy::expect_used)]
//! What of the caller's environment an agent receives: the network settings
//! Claude Code cannot work without, no credentials, and no name in
//! `run.env_pass` that would redirect git, load code or leak a secret.

use aaf_domain::config::RunConfig;
use aaf_domain::ports::{AgentRequest, AgentRunner, PortError, SandboxPolicy};
use aaf_engine::executors::generic::GenericRunner;
use aaf_engine::executors::{ExecEnv, agent_env, build_runner_with};

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_string()).collect()
}

const NETWORK: [(&str, &str); 9] = [
    ("HTTPS_PROXY", "http://proxy.corp:3128"),
    ("HTTP_PROXY", "http://proxy.corp:3128/"),
    ("NO_PROXY", "localhost,127.0.0.1,.corp"),
    ("https_proxy", "proxy.corp:3128"),
    ("http_proxy", "http://[fe80::1]:3128"),
    ("no_proxy", "localhost"),
    ("NODE_EXTRA_CA_CERTS", "/etc/corp/ca.pem"),
    ("SSL_CERT_FILE", "/etc/corp/ca.pem"),
    ("SSL_CERT_DIR", "/etc/corp/certs"),
];

const CREDENTIALS: [(&str, &str); 5] = [
    ("CLAUDE_CODE_OAUTH_TOKEN", "t"),
    ("ANTHROPIC_API_KEY", "k"),
    ("SSH_AUTH_SOCK", "/tmp/agent.sock"),
    ("GITHUB_TOKEN", "g"),
    ("ALL_PROXY", "socks5://user:pw@proxy.corp:1080"),
];

#[test]
fn network_settings_reach_the_agent_and_credentials_do_not() {
    let env = ExecEnv {
        parent_env: pairs(&[NETWORK.as_slice(), CREDENTIALS.as_slice()].concat()),
        ..ExecEnv::default()
    };

    let handed = agent_env(&env, &[]);

    assert_eq!(handed, pairs(&NETWORK));
}

#[test]
fn a_proxy_url_with_a_login_is_withheld_unless_the_owner_names_it() {
    let with_login = [
        ("HTTPS_PROXY", "http://user:secret@proxy.corp:3128"),
        ("HTTP_PROXY", "http://user@proxy.corp:3128/path@x"),
        ("https_proxy", "user:secret@proxy.corp:3128"),
        ("http_proxy", "http://proxy.corp:3128/a@b?c@d#e@f"),
        // Not a URL: a host list never carries a login.
        ("NO_PROXY", "user@host"),
    ];
    let mut env = ExecEnv {
        parent_env: pairs(&with_login),
        ..ExecEnv::default()
    };

    // An `@` after the authority is part of the path, not a login.
    assert_eq!(agent_env(&env, &[]), pairs(&[with_login[3], with_login[4]]));

    env.env_pass = names(&["HTTPS_PROXY"]);
    assert_eq!(
        agent_env(&env, &[]),
        pairs(&[with_login[0], with_login[3], with_login[4]])
    );
}

fn request(cwd: &str) -> AgentRequest {
    AgentRequest {
        run_id: format!("test-{}", uuid::Uuid::new_v4()),
        prompt: "unused".to_string(),
        cwd: cwd.to_string(),
        account: None,
        timeout_secs: 60,
        sandbox: SandboxPolicy::default(),
        json_schema: None,
    }
}

fn env_printer(env: ExecEnv) -> GenericRunner {
    GenericRunner::new(
        names(&["/bin/sh", "-c", "/usr/bin/env; echo {prompt}"]),
        env,
    )
    .unwrap()
}

#[test]
fn the_run_log_says_which_proxy_setting_was_withheld() {
    let dir = tempfile::tempdir().unwrap();
    let env = ExecEnv {
        parent_env: pairs(&[
            ("HTTPS_PROXY", "http://user:secret@proxy.corp:3128"),
            ("HTTP_PROXY", "http://proxy.corp:3128"),
        ]),
        ..ExecEnv::default()
    };

    let outcome = env_printer(env)
        .run(&request(dir.path().to_str().unwrap()))
        .unwrap();

    let log = &outcome.log_tail;
    assert!(log.contains("HTTP_PROXY=http://proxy.corp:3128"), "{log}");
    assert!(!log.contains("secret"), "{log}");
    assert!(
        log.contains("[aaf] HTTPS_PROXY was not handed to the agent"),
        "{log}"
    );
    assert!(log.contains("run.env_pass"), "{log}");
}

const REFUSED: [&str; 27] = [
    "AAF_RUN_ID",
    "AAF_DATA_DIR",
    "AAF_HEAVY_SLOT",
    "GIT_CONFIG",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_COUNT",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_EXEC_PATH",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "LD_PRELOAD",
    "PATH",
    "HOME",
    "GITHUB_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "AWS_SECRET_ACCESS_KEY",
    "CLIENT_SECRET",
    "DB_PASSWORD",
    "PASSWORD",
    "github_token",
    "",
    "A=B",
    "1ST",
];

const ACCEPTED: [&str; 7] = [
    "OWNER_PRIVATE",
    "GRADLE_OPTS",
    "HTTPS_PROXY",
    "SSH_AUTH_SOCK",
    "GIT_AUTHOR_NAME",
    "KEYBOARD",
    "_x9",
];

fn config(env_pass: &[&str]) -> RunConfig {
    RunConfig {
        executor: "custom".to_string(),
        executor_cmd: names(&["/bin/echo", "{prompt}"]),
        env_pass: names(env_pass),
        ..RunConfig::default()
    }
}

#[test]
fn env_pass_refuses_names_that_redirect_git_load_code_or_carry_credentials() {
    for name in REFUSED {
        let err = build_runner_with(&config(&["OWNER_PRIVATE", name]), ExecEnv::default())
            .err()
            .unwrap_or_else(|| panic!("{name:?} was accepted"));
        let PortError::Invalid(message) = err else {
            panic!("{name:?}: {err:?}");
        };
        assert!(message.contains("run.env_pass"), "{message}");
        assert!(message.contains(&format!("{name:?}")), "{message}");
    }
    assert!(build_runner_with(&config(&ACCEPTED), ExecEnv::default()).is_ok());
}

#[test]
fn a_runner_built_by_hand_does_not_start_with_a_refused_name() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ran");
    let runner = GenericRunner::new(
        names(&["/usr/bin/touch", marker.to_str().unwrap(), "{prompt}"]),
        ExecEnv {
            parent_env: pairs(&[("GIT_DIR", "/agent/made")]),
            env_pass: names(&["GIT_DIR"]),
            ..ExecEnv::default()
        },
    )
    .unwrap();

    let err = runner
        .run(&request(dir.path().to_str().unwrap()))
        .unwrap_err();

    assert!(matches!(err, PortError::Invalid(_)), "{err:?}");
    assert!(!marker.exists(), "the agent was started");
}
