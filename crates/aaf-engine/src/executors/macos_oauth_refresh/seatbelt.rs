//! Minimal Seatbelt profile and host binary validation for macOS OAuth refresh.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::ExecEnv;
use crate::executors::{BUILD_CACHE_DIRS, DEFAULT_WORKTREES_REL, sandbox};

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
const MACOS_TCC_PERSONAL_DIRS: [&str; 6] = [
    "Desktop",
    "Documents",
    "Downloads",
    "Pictures",
    "Movies",
    "Music",
];
const HOST_ENV_KEYS: [&str; 18] = [
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "TERM",
    "LANG",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "NODE_EXTRA_CA_CERTS",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "XDG_CONFIG_HOME",
];

pub(super) fn lookup_env(env: &ExecEnv, key: &str) -> Option<String> {
    if let Some((_, val)) = env.parent_env.iter().rev().find(|(k, _)| k == key) {
        return Some(val.clone());
    }
    if env.parent_env.is_empty() {
        return std::env::var(key).ok();
    }
    None
}

pub(super) fn validate_host_bin(
    bin: &str,
    env: &ExecEnv,
    home: &Path,
    task_cwd: &str,
    config_dir: Option<&str>,
) -> bool {
    let path = Path::new(bin);
    if !path.is_absolute() || !path.is_file() {
        return false;
    }
    let Ok(canon) = fs::canonicalize(path) else {
        return false;
    };
    let mut forbidden: Vec<PathBuf> = vec![
        home.join(".claude"),
        home.join(".gemini"),
        home.join(DEFAULT_WORKTREES_REL),
        PathBuf::from("/tmp"),
        PathBuf::from("/var/tmp"),
        std::env::temp_dir(),
    ];
    for cache in BUILD_CACHE_DIRS {
        forbidden.push(home.join(cache));
    }
    if let Some(wt_root) = env
        .worktrees_root
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        forbidden.push(PathBuf::from(wt_root));
    }
    if !task_cwd.trim().is_empty() {
        forbidden.push(PathBuf::from(task_cwd));
    }
    if let Some(cfg) = config_dir.filter(|s| !s.trim().is_empty()) {
        forbidden.push(PathBuf::from(cfg));
    }
    for tmp in &env.temp_dirs {
        forbidden.push(PathBuf::from(tmp));
    }
    for extra in env
        .machine_writable_caches
        .iter()
        .chain(&env.sdk_dirs)
        .map(String::as_str)
        .chain(
            [
                env.cargo_home.as_deref(),
                env.rustup_home.as_deref(),
                env.gradle_user_home.as_deref(),
            ]
            .into_iter()
            .flatten(),
        )
    {
        if !extra.trim().is_empty() {
            forbidden.push(PathBuf::from(extra));
        }
    }
    for key in ["CARGO_HOME", "RUSTUP_HOME", "GRADLE_USER_HOME"] {
        if let Some(val) = lookup_env(env, key).filter(|s| !s.trim().is_empty()) {
            forbidden.push(PathBuf::from(val));
        }
    }
    for bad in forbidden {
        if let Ok(c_bad) = fs::canonicalize(&bad)
            && canon.starts_with(&c_bad)
        {
            return false;
        }
    }
    true
}

fn sbpl_clean_canon(path: &Path) -> Option<(PathBuf, String)> {
    let canon = sandbox::canonical(&path.to_string_lossy()).ok()?;
    let s = canon.to_str()?;
    if s.chars().any(|c| c == '"' || c == '\\' || c.is_control()) {
        return None;
    }
    Some((canon.clone(), s.to_string()))
}

fn overlaps_any(candidate: &Path, denied: &[PathBuf]) -> bool {
    denied
        .iter()
        .any(|d| candidate.starts_with(d) || d.starts_with(candidate))
}

/// Emits all read/write denies FIRST (failing closed on any unquotable path),
/// then reopens ONLY `empty_cwd` (which may live inside `data_dir`),
/// `keychain_dir`, and (for Claude) the 3 OAuth refresh lock paths inside
/// `claude_config_dir` LAST so SBPL's last-matching-rule semantics keep
/// sibling `data_dir` entries and all Claude config/settings files closed.
pub(super) fn build_seatbelt_profile(
    empty_cwd: &Path,
    keychain_dir: &Path,
    claude_config_dir: Option<&Path>,
    denied_dirs: &[&Path],
) -> Option<String> {
    build_seatbelt_profile_with_home(
        empty_cwd,
        keychain_dir,
        claude_config_dir,
        None,
        denied_dirs,
    )
}

pub(super) fn build_seatbelt_profile_with_home(
    empty_cwd: &Path,
    keychain_dir: &Path,
    claude_config_dir: Option<&Path>,
    home_dir: Option<&Path>,
    denied_dirs: &[&Path],
) -> Option<String> {
    let (_, cwd_s) = sbpl_clean_canon(empty_cwd)?;
    let (_, kc_s) = sbpl_clean_canon(keychain_dir)?;
    let mut p = String::from(
        "(version 1)\n(allow default)\n(deny file-write*)\n\
         (allow file-write* (literal \"/dev/null\") (literal \"/dev/zero\") \
         (literal \"/dev/tty\") (literal \"/dev/dtracehelper\") \
         (regex #\"^/dev/fd/[0-9]+$\"))\n",
    );
    let mut denied_canon = Vec::with_capacity(denied_dirs.len());
    for denied in denied_dirs {
        let (canon, s) = sbpl_clean_canon(denied)?;
        p.push_str(&format!(
            "(deny file-read* file-write* (subpath \"{s}\"))\n"
        ));
        denied_canon.push(canon);
    }
    p.push_str(&format!(
        "(allow file-read* file-write* (subpath \"{cwd_s}\"))\n\
         (deny file-write* (literal \"{cwd_s}\"))\n\
         (allow file-write* (subpath \"{kc_s}\"))\n"
    ));
    if let Some(cfg) = claude_config_dir {
        // Resolve symlinks (e.g. `~/.claude -> ~/dotfiles/claude`) before
        // checking overlaps so dotfiles layouts refresh cleanly while symlinks
        // pointing at denied dirs or `$HOME` still fail closed.
        let (cfg_canon, cfg_s) = sbpl_clean_canon(cfg)?;
        let sibling_lock = PathBuf::from(format!("{cfg_s}.lock"));
        if cfg_canon.parent().is_none()
            || overlaps_any(&cfg_canon, &denied_canon)
            || overlaps_any(&sibling_lock, &denied_canon)
        {
            return None;
        }
        if let Some(home) = home_dir {
            let (home_canon, _) = sbpl_clean_canon(home)?;
            if cfg_canon == home_canon || home_canon.starts_with(&cfg_canon) {
                return None;
            }
        }
        let cfg_re = sandbox::regex_escape(&cfg_s);
        // Deny Claude's own `plugins/` and `hooks/` AFTER `overlaps_any(&cfg_canon)`
        // so passing `Some(cfg)` never self-conflicts while preventing a host refresh
        // ping from reading or running plugin/hook code inside `cfg`.
        p.push_str(&format!(
            "(deny file-read* file-write* \
             (subpath \"{cfg_s}/plugins\") \
             (subpath \"{cfg_s}/hooks\"))\n\
             (allow file-write* \
             (subpath \"{cfg_s}/.oauth_refresh.lock\") \
             (subpath \"{cfg_s}/.storage-write.lock\") \
             (subpath \"{cfg_s}.lock\") \
             (regex #\"^{cfg_re}/\\.oauth_refresh\\.lock\\.owner(\\.|$)\"))\n"
        ));
    }
    Some(p)
}

fn apply_clean_host_env(cmd: &mut Command, env: &ExecEnv, empty_cwd: &Path, home: &Path) {
    cmd.env_clear();
    for key in HOST_ENV_KEYS {
        if let Some(val) = lookup_env(env, key) {
            cmd.env(key, val);
        }
    }
    cmd.env("HOME", home);
    cmd.env("TMPDIR", empty_cwd);
}

pub(super) fn build_host_sandbox_profile(
    env: &ExecEnv,
    home: &Path,
    task_cwd: &str,
    empty_cwd: &Path,
    claude_config_dir: Option<&Path>,
) -> Option<String> {
    let kc_dir = home.join("Library/Keychains");
    let mut owned_denied: Vec<PathBuf> = vec![
        home.join(".ssh"),
        home.join(".aws"),
        home.join(".gnupg"),
        home.join(".kube"),
        home.join(".config/autofactory/ui-service.token"),
        home.join(".local/share/autofactory/ui-service.token"),
    ];
    for tcc in MACOS_TCC_PERSONAL_DIRS {
        owned_denied.push(home.join(tcc));
    }
    if claude_config_dir.is_some() {
        owned_denied.push(home.join(".gemini"));
    } else {
        owned_denied.push(home.join(".claude"));
        owned_denied.push(home.join(".claude.json"));
    }
    if let Some(d) = env.data_dir.as_deref().filter(|d| !d.trim().is_empty()) {
        owned_denied.push(PathBuf::from(d));
    }
    if !task_cwd.trim().is_empty() {
        owned_denied.push(PathBuf::from(task_cwd));
    }
    let denied: Vec<&Path> = owned_denied.iter().map(PathBuf::as_path).collect();
    build_seatbelt_profile_with_home(empty_cwd, &kc_dir, claude_config_dir, Some(home), &denied)
}

pub(super) fn build_host_sandbox_cmd(
    bin: &str,
    args: &[&str],
    env: &ExecEnv,
    home: &Path,
    task_cwd: &str,
    empty_cwd: &Path,
    claude_config_dir: Option<&Path>,
) -> Option<Command> {
    let profile = build_host_sandbox_profile(env, home, task_cwd, empty_cwd, claude_config_dir)?;
    let mut cmd = Command::new(SANDBOX_EXEC);
    apply_clean_host_env(&mut cmd, env, empty_cwd, home);
    cmd.current_dir(empty_cwd)
        .args(["-p", &profile, "--", bin])
        .args(args);
    Some(cmd)
}
