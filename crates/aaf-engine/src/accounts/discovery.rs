//! Finds agent accounts on disk by looking at directory and file *names*
//! only. Nothing in here opens a file: credentials and tokens stay unread.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use aaf_domain::account::{AccountProfile, AccountState, ProviderKind, default_account_name};
use aaf_domain::config::{AccountsConfig, Billing};

/// Everything discovery needs from the process environment, passed in so
/// tests can point it at a fake home.
pub struct DiscoveryEnv<'a> {
    pub home: PathBuf,
    pub binary_on_path: &'a dyn Fn(&str) -> bool,
}

/// Single-directory providers: home-relative config dir and the binary
/// names that prove the tool is installed (any one is enough).
const SINGLE_DIR_PROVIDERS: [(ProviderKind, &str, &[&str]); 4] = [
    (ProviderKind::Codex, ".codex", &["codex"]),
    (
        ProviderKind::Antigravity,
        ".gemini",
        &["gemini", "agy", "antigravity"],
    ),
    (ProviderKind::OpenCode, ".config/opencode", &["opencode"]),
    (ProviderKind::Grok, ".grok", &["grok"]),
];

/// Discovers accounts, sorted by provider then config dir, without duplicates.
///
/// Unreadable directories and unsupported glob shapes yield no accounts
/// rather than an error: discovery is best effort and a missing account is
/// the safe outcome.
pub fn discover(cfg: &AccountsConfig, env: &DiscoveryEnv) -> Vec<AccountProfile> {
    let aliases = resolve_aliases(cfg, &env.home);
    let mut found = Vec::new();
    for pattern in &cfg.discovery_globs {
        for dir in expand(pattern, &env.home) {
            if is_claude_config_dir(&dir) {
                found.extend(profile(ProviderKind::ClaudeCode, &dir, &aliases));
            }
        }
    }
    for (provider, rel, binaries) in SINGLE_DIR_PROVIDERS {
        let dir = env.home.join(rel);
        let has_bin = binaries.iter().any(|name| (env.binary_on_path)(name))
            || (provider == ProviderKind::Grok && is_executable(&dir.join("bin/grok")));
        if is_real_dir(&dir) && has_bin {
            found.extend(profile(provider, &dir, &aliases));
        }
    }
    found.sort_by(|a, b| {
        (a.provider.as_str(), &a.config_dir).cmp(&(b.provider.as_str(), &b.config_dir))
    });
    found.dedup_by(|a, b| a.id == b.id);
    found
}

/// The one place that reads the real environment (`HOME`, `PATH`).
pub fn discover_from_process_env(cfg: &AccountsConfig) -> Vec<AccountProfile> {
    let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) else {
        return Vec::new();
    };
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let on_path = |name: &str| binary_on_path_in(&path_var, name);
    discover(
        cfg,
        &DiscoveryEnv {
            home: PathBuf::from(home),
            binary_on_path: &on_path,
        },
    )
}

/// True when `name` is an executable file in one of the `PATH`-style
/// directories of `path_var`.
pub fn binary_on_path_in(path_var: &OsStr, name: &str) -> bool {
    if name.is_empty() || name.contains('/') {
        return false;
    }
    std::env::split_paths(path_var).any(|dir| is_executable(&dir.join(name)))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    // Following symlinks is right here: package managers install binaries
    // as links, and this is a tool lookup, not a config dir.
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file())
}

fn expand_home(text: &str, home: &Path) -> PathBuf {
    if text == "~" {
        home.to_path_buf()
    } else if let Some(rest) = text.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(text)
    }
}

/// `accounts.aliases` as (canonical config dir, display name). A key that
/// does not resolve to an existing path cannot match a discovered account
/// and is dropped, as is a blank name.
fn resolve_aliases(cfg: &AccountsConfig, home: &Path) -> Vec<(PathBuf, String)> {
    cfg.aliases
        .iter()
        .filter(|(_, name)| !name.trim().is_empty())
        .filter_map(|(dir, name)| {
            let canonical = fs::canonicalize(expand_home(dir, home)).ok()?;
            Some((canonical, name.trim().to_string()))
        })
        .collect()
}

/// Canonical config directories configured with `billing = "api"`.
/// Keys that do not exist on disk are skipped.
pub fn api_billed_dirs(cfg: &AccountsConfig, home: &Path) -> BTreeSet<PathBuf> {
    cfg.billing
        .iter()
        .filter(|(_, billing)| **billing == Billing::Api)
        .filter_map(|(dir, _)| fs::canonicalize(expand_home(dir, home)).ok())
        .collect()
}

/// Account ids from `accounts` eligible for default USD estimation:
/// Claude Code accounts whose canonical `config_dir` appears in `dirs`.
pub fn api_billed_account_ids(
    dirs: &BTreeSet<PathBuf>,
    accounts: &[AccountState],
) -> BTreeSet<String> {
    if dirs.is_empty() {
        return BTreeSet::new();
    }
    accounts
        .iter()
        .filter(|acct| acct.profile.provider == ProviderKind::ClaudeCode)
        .filter(|acct| {
            let path = Path::new(&acct.profile.config_dir);
            let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            dirs.contains(&canonical)
        })
        .map(|acct| acct.profile.id.clone())
        .collect()
}

/// Expands a leading `~` and a `*` at the very end of the pattern. Any other
/// wildcard position is unsupported and matches nothing.
fn expand(pattern: &str, home: &Path) -> Vec<PathBuf> {
    let path = expand_home(pattern, home);
    if !path.is_absolute() {
        return Vec::new();
    }
    let Some(text) = path.to_str() else {
        return Vec::new();
    };
    let Some(stem) = text.strip_suffix('*') else {
        return if text.contains('*') {
            Vec::new()
        } else {
            vec![path]
        };
    };
    if stem.contains('*') {
        return Vec::new();
    }
    // `stem` ends either with `/` (match every entry) or with a name prefix.
    let (parent, prefix) = match stem.rsplit_once('/') {
        Some((parent, prefix)) if !parent.is_empty() => (Path::new(parent), prefix),
        Some((_, prefix)) => (Path::new("/"), prefix),
        None => return Vec::new(),
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(prefix))
        })
        .map(|entry| entry.path())
        .collect()
}

/// A directory that is not a symlink. `symlink_metadata` never follows the
/// last component, so a link to a real config dir is rejected.
fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

fn is_real_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

/// Markers every real Claude Code config dir carries: a `projects` entry
/// plus its own `.claude.json` or `settings.json`. Look-alikes such as a
/// tool's own `~/.claude-something` state dir have neither.
///
/// Profile switchers link `projects` (and `settings.json`) of every extra
/// profile to the shared ones, so a `projects` link is accepted by name; it
/// is never followed. At least one regular file of the dir's own is still
/// required: a dir made only of links is another account's data, not a
/// second account.
fn is_claude_config_dir(dir: &Path) -> bool {
    is_real_dir(dir)
        && fs::symlink_metadata(dir.join("projects")).is_ok_and(|m| m.is_dir() || m.is_symlink())
        && (is_real_file(&dir.join(".claude.json")) || is_real_file(&dir.join("settings.json")))
}

fn profile(
    provider: ProviderKind,
    dir: &Path,
    aliases: &[(PathBuf, String)],
) -> Option<AccountProfile> {
    // `dir` itself was checked not to be a symlink; canonicalising only
    // resolves links in its ancestors (e.g. /var -> /private/var) so the id
    // is the same however the home path was spelled.
    let canonical = fs::canonicalize(dir).ok()?;
    let config_dir = canonical.to_str()?.to_string();
    let name = aliases
        .iter()
        .find(|(alias_dir, _)| *alias_dir == canonical)
        .map_or_else(
            || default_account_name(&config_dir),
            |(_, name)| name.clone(),
        );
    Some(AccountProfile {
        id: account_id(provider, &canonical),
        provider,
        config_dir,
        name,
    })
}

/// `<provider>-<12 hex of sha256(canonical dir)>`: stable across runs and
/// made only of `[a-z0-9_-]`.
pub fn account_id(provider: ProviderKind, canonical_dir: &Path) -> String {
    let mut hash = aaf_fp::sha256_hex(canonical_dir.as_os_str().as_encoded_bytes());
    hash.truncate(12);
    format!("{}-{hash}", provider.as_str())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod grok_discovery_tests {
    use super::*;

    #[test]
    fn discovers_grok_dir_only_when_grok_binary_exists() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".grok")).unwrap();
        let cfg = AccountsConfig::default();

        let with_bin = discover(
            &cfg,
            &DiscoveryEnv {
                home: dir.path().to_path_buf(),
                binary_on_path: &|bin| bin == "grok",
            },
        );
        assert_eq!(with_bin.len(), 1);
        assert_eq!(with_bin[0].provider, ProviderKind::Grok);
        assert_eq!(with_bin[0].name, "grok");
        assert!(with_bin[0].id.starts_with("grok-"));

        let without_bin = discover(
            &cfg,
            &DiscoveryEnv {
                home: dir.path().to_path_buf(),
                binary_on_path: &|_| false,
            },
        );
        assert!(without_bin.is_empty());
    }
}
