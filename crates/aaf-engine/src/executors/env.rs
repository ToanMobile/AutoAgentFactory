//! The environment an agent starts with: an allow-list of the environment
//! `aaf run` was started in, plus the names `run.env_pass` adds, minus the
//! names nobody may add.

use std::path::{Component, Path, PathBuf};

use aaf_domain::ports::{PortError, PortResult};

use super::{DEFAULT_WORKTREES_REL, ExecEnv, sandbox};

/// What an agent needs of the environment `aaf run` was started in: where
/// things are, the locale, the toolchain homes builds look for, and how this
/// machine reaches the network (proxy and extra CA certificates; without
/// them Claude Code cannot connect at all behind a corporate proxy).
/// Anything else (tokens, `CLAUDE_CODE_*`, `ANTHROPIC_API_KEY`,
/// `SSH_AUTH_SOCK`, `AAF_*` state of the caller) stays behind unless
/// `run.env_pass` names it.
const ENV_ALLOW: [&str; 32] = [
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
    "GOPATH",
    "GOMODCACHE",
    "PNPM_HOME",
    "BUN_INSTALL",
    "PUB_CACHE",
    "FLUTTER_ROOT",
    "XDG_CACHE_HOME",
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

/// Toolchain directory variables (subset of [`ENV_ALLOW`]) safe to forward into
/// a launchd service plist so headless background gates find the Android/Java
/// SDK without exposing secrets or proxy credentials.
pub const TOOLCHAIN_HOME_ENV: [&str; 4] = [
    "JAVA_HOME",
    "ANDROID_HOME",
    "ANDROID_SDK_ROOT",
    "GRADLE_USER_HOME",
];

/// True when `value` is a clean absolute path to an existing local directory
/// (rejecting control chars, relative paths, and URL schemes; permits Homebrew
/// versioned paths such as `/opt/homebrew/opt/openjdk@17/...`).
pub fn is_valid_toolchain_dir(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed != value
        || trimmed.chars().any(char::is_control)
        || trimmed.contains("://")
    {
        return false;
    }
    let path = Path::new(trimmed);
    path.is_absolute() && path.is_dir()
}

/// Non-interactive defaults injected into confined PTY runs unless overridden
/// by `run.env_pass` / `machine_env_pass`.
pub const NON_INTERACTIVE_ENV: [(&str, &str); 5] = [
    ("CI", "true"),
    ("NONINTERACTIVE", "1"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("DEBIAN_FRONTEND", "noninteractive"),
    ("NO_UPDATE_NOTIFIER", "1"),
];

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

/// Credential-shaped variable names aligned with [`super::secret_mask`]:
/// only the machine config (tier 1) may hand these on.
const CREDENTIAL_EXACT: [&str; 2] = ["TOKEN", "SECRET"];
const CREDENTIAL_SUBSTRINGS: [&str; 10] = [
    "_TOKEN",
    "_KEY",
    "_SECRET",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "DATABASE_URL",
    "AUTH_HEADER",
    "BEARER",
    "COOKIE",
];
const CREDENTIAL_EXCLUDED_EXACT: [&str; 2] = ["SSH_AUTH_SOCK", "KEYMAP"];
const CREDENTIAL_EXCLUDED_SUBSTRINGS: [&str; 2] = ["PUBLIC_KEY", "KEYMAP"];

fn is_credential_env_name(upper: &str) -> bool {
    if CREDENTIAL_EXCLUDED_EXACT.contains(&upper)
        || CREDENTIAL_EXCLUDED_SUBSTRINGS
            .iter()
            .any(|needle| upper.contains(needle))
    {
        return false;
    }
    CREDENTIAL_EXACT.contains(&upper)
        || CREDENTIAL_SUBSTRINGS
            .iter()
            .any(|needle| upper.contains(needle))
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
    if is_credential_env_name(&upper) {
        return Some(
            "its name has the shape of a credential; \
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

/// Collects non-empty runtime values for variables named in `env.env_pass`
/// (and `env.machine_env_pass`) so [`super::stream_log::StreamLog`] can scrub
/// them before writing to disk.
pub fn env_pass_secrets(env: &ExecEnv) -> Vec<String> {
    let mut values = Vec::new();
    for name in env.env_pass.iter().chain(&env.machine_env_pass) {
        if let Some((_, val)) = env.parent_env.iter().find(|(k, _)| k == name)
            && !val.is_empty()
        {
            values.push(val.clone());
        }
        if let Ok(val) = std::env::var(name)
            && !val.is_empty()
        {
            values.push(val);
        }
    }
    values
}

/// Resolves an absolute custom toolchain home from an explicit [`ExecEnv`]
/// field or from `parent_env`.
pub(super) fn toolchain_home(
    explicit: Option<&str>,
    parent_env: &[(String, String)],
    var: &str,
) -> Option<String> {
    explicit
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .or_else(|| {
            parent_env
                .iter()
                .rev()
                .find(|(k, v)| k == var && !v.is_empty())
                .map(|(_, v)| v.clone())
        })
        .filter(|v| Path::new(v).is_absolute())
}

/// Appends write-denied exact and prefix rules beneath `base`, deduplicating.
pub(super) fn push_denied_entries(
    base: &str,
    entries: &[&str],
    write_denied: &mut Vec<String>,
    write_denied_prefix: &mut Vec<String>,
) {
    for entry in entries {
        let (target, rel) = match entry.strip_suffix('*') {
            Some(prefix) => (&mut *write_denied_prefix, prefix),
            None => (&mut *write_denied, *entry),
        };
        let path = Path::new(base).join(rel).to_string_lossy().into_owned();
        if !target.contains(&path) {
            target.push(path);
        }
    }
}

/// Injects [`NON_INTERACTIVE_ENV`] defaults into `spec_env` unless already set
/// there or explicitly forwarded via `env_pass`/`machine_env_pass` from `parent_env`.
pub(super) fn inject_non_interactive_env(env: &ExecEnv, spec_env: &mut Vec<(String, String)>) {
    for (key, val) in NON_INTERACTIVE_ENV {
        let overridden_by_pass = named(env, key) && env.parent_env.iter().any(|(k, _)| k == key);
        let already_set = spec_env.iter().any(|(k, _)| k == key);
        if !overridden_by_pass && !already_set {
            spec_env.push((key.to_string(), val.to_string()));
        }
    }
}

const SENSITIVE_HOME_DIRS: [&str; 6] = [
    ".ssh",
    ".gnupg",
    ".aws",
    ".kube",
    ".config/autofactory",
    ".local/share/opencode",
];
const SENSITIVE_HOME_PREFIXES: [&str; 4] = [".claude", ".codex", ".gemini", ".grok"];

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn expand_tilde(raw: &str, home: Option<&str>) -> PortResult<String> {
    if raw == "~" || raw.starts_with("~/") {
        let Some(h) = home.filter(|s| !s.is_empty()) else {
            return Err(PortError::Invalid(format!(
                "machine.writable_caches entry {raw:?} requires HOME to be set"
            )));
        };
        let rest = raw.strip_prefix("~/").unwrap_or("");
        return Ok(if rest.is_empty() {
            h.to_string()
        } else {
            format!("{}/{rest}", h.trim_end_matches('/'))
        });
    }
    Ok(raw.to_string())
}

fn raw_and_canon(raw: &str) -> Vec<PathBuf> {
    let lex = lexical_normalize(Path::new(raw));
    let mut out = vec![lex.clone()];
    if let Ok(canon) = sandbox::canonical(raw)
        && canon != lex
    {
        out.push(canon);
    }
    out
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a == b || a.starts_with(b) || b.starts_with(a)
}

fn is_sensitive_home_prefix(cand: &Path, home: &Path) -> bool {
    let Ok(rel) = cand.strip_prefix(home) else {
        return false;
    };
    let Some(Component::Normal(first)) = rel.components().next() else {
        return false;
    };
    first
        .to_str()
        .is_some_and(|name| SENSITIVE_HOME_PREFIXES.iter().any(|p| name.starts_with(p)))
}

/// Validates Tier-1 `machine.writable_caches` entries and returns their
/// expanded absolute paths.
///
/// Rejects any entry whose raw or canonical path equals or is an ancestor of
/// `/`, `HOME`, `AAF_DATA_DIR`, `worktrees_root`, any `PATH` directory, or
/// sensitive directories (`.ssh`, `.gnupg`, `.aws`, `.kube`,
/// `.config/autofactory`, `.claude*`, `.codex*`, `.gemini*`,
/// `.local/share/opencode`), as well as any path inside those sensitive roots.
pub fn validate_writable_caches(caches: &[String], env: &ExecEnv) -> PortResult<Vec<String>> {
    let home_forms: Vec<PathBuf> = env
        .home
        .as_deref()
        .filter(|h| !h.is_empty())
        .map(raw_and_canon)
        .unwrap_or_default();
    let mut forbidden_overlap: Vec<(String, PathBuf)> = Vec::new();
    if let Some(data) = env.data_dir.as_deref().filter(|s| !s.is_empty()) {
        for p in raw_and_canon(data) {
            forbidden_overlap.push(("AAF_DATA_DIR".to_string(), p));
        }
    }
    let default_wt = env
        .home
        .as_deref()
        .filter(|h| !h.is_empty())
        .map(|h| format!("{}/{DEFAULT_WORKTREES_REL}", h.trim_end_matches('/')));
    for wt in [env.worktrees_root.as_deref(), default_wt.as_deref()]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
    {
        for p in raw_and_canon(wt) {
            forbidden_overlap.push(("worktrees_root".to_string(), p));
        }
    }
    for (_, value) in env.parent_env.iter().filter(|(k, _)| k == "PATH") {
        for dir in std::env::split_paths(value).filter(|d| d.is_absolute()) {
            let text = dir.to_string_lossy();
            for p in raw_and_canon(&text) {
                forbidden_overlap.push((format!("PATH dir {}", dir.display()), p));
            }
        }
    }
    for home in &home_forms {
        for rel in SENSITIVE_HOME_DIRS {
            let joined = home.join(rel);
            let joined_str = joined.to_string_lossy();
            for p in raw_and_canon(&joined_str) {
                forbidden_overlap.push((format!("sensitive dir {rel}"), p));
            }
        }
    }

    let mut resolved = Vec::with_capacity(caches.len());
    for raw in caches {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.as_bytes().contains(&0) {
            return Err(PortError::Invalid(format!(
                "machine.writable_caches: invalid empty or NUL path {raw:?}"
            )));
        }
        let expanded = expand_tilde(trimmed, env.home.as_deref())?;
        let exp_path = Path::new(&expanded);
        if !exp_path.is_absolute()
            || exp_path
                .components()
                .any(|c| matches!(c, Component::ParentDir))
        {
            return Err(PortError::Invalid(format!(
                "machine.writable_caches: {raw:?} must be a clean absolute path without '..'"
            )));
        }
        let canon = sandbox::canonical(&expanded)?;
        let lex = lexical_normalize(exp_path);
        for cand in [&lex, &canon] {
            if cand.as_path() == Path::new("/") {
                return Err(PortError::Invalid(format!(
                    "machine.writable_caches: {raw:?} cannot open root '/'"
                )));
            }
            for home in &home_forms {
                if cand == home || home.starts_with(cand) {
                    return Err(PortError::Invalid(format!(
                        "machine.writable_caches: {raw:?} equals or is an ancestor of HOME"
                    )));
                }
                if is_sensitive_home_prefix(cand, home) {
                    return Err(PortError::Invalid(format!(
                        "machine.writable_caches: {raw:?} targets a sensitive agent config dir"
                    )));
                }
            }
            for (label, protected) in &forbidden_overlap {
                if overlaps(cand, protected) {
                    return Err(PortError::Invalid(format!(
                        "machine.writable_caches: {raw:?} overlaps protected {label}"
                    )));
                }
            }
        }
        if !resolved.contains(&expanded) {
            resolved.push(expanded);
        }
    }
    Ok(resolved)
}
