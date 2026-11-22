//! The environment an agent starts with: an allow-list of the environment
//! `aaf run` was started in, plus the names `run.env_pass` adds, minus the
//! names nobody may add.

use aaf_domain::ports::{PortError, PortResult};

use super::ExecEnv;

/// What an agent needs of the environment `aaf run` was started in: where
/// things are, the locale, the toolchain homes builds look for, and how this
/// machine reaches the network (proxy and extra CA certificates; without
/// them Claude Code cannot connect at all behind a corporate proxy).
/// Anything else (tokens, `CLAUDE_CODE_*`, `ANTHROPIC_API_KEY`,
/// `SSH_AUTH_SOCK`, `AAF_*` state of the caller) stays behind unless
/// `run.env_pass` names it.
const ENV_ALLOW: [&str; 25] = [
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "TERM",
    "TMPDIR",
    "JAVA_HOME",
    "ANDROID_HOME",
    "ANDROID_SDK_ROOT",
    "GRADLE_USER_HOME",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "UNITY_PATH",
    "DEVELOPER_DIR",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "NO_PROXY",
    "https_proxy",
    "http_proxy",
    "no_proxy",
    "NODE_EXTRA_CA_CERTS",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];
const ENV_ALLOW_PREFIX: &str = "LC_";

/// Allowed names whose value is a URL that may carry a login
/// (`http://user:password@proxy:3128`).
const PROXY_URLS: [&str; 4] = ["HTTPS_PROXY", "HTTP_PROXY", "https_proxy", "http_proxy"];

/// True when the authority part of `url` has a `user[:password]@` in front
/// of the host.
fn has_userinfo(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split(['/', '?', '#'])
        .next()
        .is_some_and(|authority| authority.contains('@'))
}

fn named(env: &ExecEnv, name: &str) -> bool {
    env.env_pass
        .iter()
        .chain(&env.machine_env_pass)
        .any(|pass| pass == name)
}

/// A proxy URL with a login in it is a credential, and credentials reach the
/// agent only when the repository owner names them.
fn withheld(env: &ExecEnv, name: &str, value: &str) -> bool {
    PROXY_URLS.contains(&name) && has_userinfo(value) && !named(env, name)
}

/// Names of the proxy variables [`agent_env`] holds back because their URL
/// carries a login. The run log says so: an agent that cannot connect is
/// otherwise a riddle.
pub fn withheld_proxies(env: &ExecEnv) -> Vec<String> {
    env.parent_env
        .iter()
        .filter(|(name, value)| withheld(env, name, value))
        .map(|(name, _)| name.clone())
        .collect()
}

/// The whole environment of an agent run: the allowed part of
/// `env.parent_env`, then `set` (what aaf sets itself), which wins.
pub fn agent_env(env: &ExecEnv, set: &[(String, String)]) -> Vec<(String, String)> {
    let allowed = |name: &str| {
        ENV_ALLOW.contains(&name) || name.starts_with(ENV_ALLOW_PREFIX) || named(env, name)
    };
    let mut out: Vec<(String, String)> = env
        .parent_env
        .iter()
        .filter(|(name, value)| allowed(name) && !withheld(env, name, value))
        .cloned()
        .collect();
    for (name, value) in set {
        out.retain(|(existing, _)| existing != name);
        out.push((name.clone(), value.clone()));
    }
    out
}

/// Why `name` may not stand in a `run.env_pass` other than the machine
/// config's, if it may not: everything [`refusal_everywhere`] refuses, plus
/// what only the operator's own file may hand on (owner decision,
/// SECURITY.md §10): `NODE_OPTIONS` and credential-shaped names. Upper and
/// lower case are the same name here: nothing legitimate depends on the
/// difference, and `github_token` is as much a credential as `GITHUB_TOKEN`.
fn refusal(name: &str) -> Option<&'static str> {
    if let Some(reason) = refusal_everywhere(name) {
        return Some(reason);
    }
    let upper = name.to_ascii_uppercase();
    if upper == "NODE_OPTIONS" {
        return Some(
            "it loads code into every Node program of the run; only the machine config \
             (~/.config/autofactory/config.toml) may name it",
        );
    }
    if upper.ends_with("_TOKEN")
        || upper.ends_with("_KEY")
        || upper.ends_with("_SECRET")
        || upper.contains("PASSWORD")
    {
        return Some(
            "its name has the shape of a credential (*_TOKEN, *_KEY, *_SECRET, *PASSWORD*); \
             only the machine config (~/.config/autofactory/config.toml) may name it",
        );
    }
    None
}

/// Why `name` may not stand in any `run.env_pass`, the machine config's
/// included.
fn refusal_everywhere(name: &str) -> Option<&'static str> {
    let well_formed = name
        .bytes()
        .enumerate()
        .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()));
    if name.is_empty() || !well_formed {
        return Some("it is not the name of an environment variable");
    }
    let upper = name.to_ascii_uppercase();
    if upper.starts_with("AAF_") {
        return Some("aaf sets and reads the AAF_* variables itself");
    }
    if upper == "PATH" || upper == "HOME" {
        return Some("aaf hands it on by itself, and nothing may replace it");
    }
    if upper.starts_with("GIT_CONFIG")
        || upper.starts_with("GIT_SSH")
        || [
            "GIT_DIR",
            "GIT_COMMON_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_EXEC_PATH",
            "GIT_TEMPLATE_DIR",
            "GIT_EXTERNAL_DIFF",
            "GIT_PROXY_COMMAND",
            "GIT_ASKPASS",
        ]
        .contains(&upper.as_str())
    {
        return Some("it redirects every git command of the run");
    }
    if upper.starts_with("DYLD_")
        || upper.starts_with("LD_")
        || [
            "SSH_ASKPASS",
            "BASH_ENV",
            "ENV",
            "ZDOTDIR",
            "NODE_PATH",
            "PYTHONSTARTUP",
            "PYTHONPATH",
            "PYTHONHOME",
            "JAVA_TOOL_OPTIONS",
            "_JAVA_OPTIONS",
            "JDK_JAVA_OPTIONS",
            "PERL5OPT",
            "PERL5LIB",
            "PERLLIB",
            "RUBYOPT",
            "RUBYLIB",
        ]
        .contains(&upper.as_str())
    {
        // Each makes an interpreter (or every JVM) load code nobody asked
        // it to run. Options read by one tool's own launcher script
        // (`GRADLE_OPTS`, `JAVA_OPTS`) stay allowed.
        return Some("it loads code into every program of the run");
    }
    None
}

/// Refuses a `run.env_pass` that names a variable no configuration may hand
/// to the agent. Checked when the runner is built and again where the
/// environment is put together.
pub fn validate_env_pass(names: &[String]) -> PortResult<()> {
    check_names(names, "run.env_pass", refusal)
}

/// [`validate_env_pass`] for the machine config's `run.env_pass` (tier 1,
/// `~/.config/autofactory/config.toml`): the operator's own file, outside
/// every repository and never merged from one, so it may also name
/// credentials and `NODE_OPTIONS`. Nothing else is let through.
pub fn validate_machine_env_pass(names: &[String]) -> PortResult<()> {
    check_names(names, "run.env_pass (machine config)", refusal_everywhere)
}

fn check_names(
    names: &[String],
    key: &str,
    refusal: fn(&str) -> Option<&'static str>,
) -> PortResult<()> {
    for name in names {
        if let Some(reason) = refusal(name) {
            return Err(PortError::Invalid(format!(
                "{key}: {name:?} cannot be handed to the agent: {reason}"
            )));
        }
    }
    Ok(())
}
