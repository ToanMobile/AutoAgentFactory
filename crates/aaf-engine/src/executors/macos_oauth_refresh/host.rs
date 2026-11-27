//! macOS host-side OAuth refresh execution under minimal Seatbelt + file locks.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::{ExecEnv, claude_keychain_service, parse_claude_oauth_expiry_ms, sha256_prefix_hex};
use crate::executors::sandbox;
use crate::process::run_with_timeout;

/// Refresh Claude OAuth proactively when the Keychain access token expires
/// within 5 minutes (matching Claude CLI's own refresh window).
const CLAUDE_REFRESH_LEAD_MS: i64 = 5 * 60 * 1000;

/// Minimum remaining lifetime required after a host refresh to treat it as valid.
const CLAUDE_REFRESH_MIN_VALID_MS: i64 = 60 * 1000;

/// Refresh Antigravity (`agy`) OAuth proactively at most once every 40 minutes
/// across processes (Google OAuth access tokens expire every 60 minutes).
const AGY_REFRESH_INTERVAL_SECS: i64 = 40 * 60;

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
const SECURITY_BIN: &str = "/usr/bin/security";
const SECURITY_TIMEOUT: Duration = Duration::from_secs(5);
const CLAUDE_REFRESH_TIMEOUT: Duration = Duration::from_secs(25);
const AGY_REFRESH_TIMEOUT: Duration = Duration::from_secs(15);
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

static TEMP_SEQ: AtomicU64 = AtomicU64::new(1);

struct EmptyTempDir(PathBuf);

impl EmptyTempDir {
    fn new(env: &ExecEnv) -> Option<Self> {
        let base = host_state_dir(env, "oauth-refresh-tmp")?;
        for _ in 0..8 {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let token = sha256_prefix_hex(&format!("{}:{seq}:{nanos}", std::process::id()), 12);
            let dir = base.join(format!("r-{token}"));
            match DirBuilder::new().recursive(false).mode(0o700).create(&dir) {
                Ok(()) => return Some(Self(dir)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => return None,
            }
        }
        None
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for EmptyTempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct CrossProcessLock(File);

impl CrossProcessLock {
    fn acquire(env: &ExecEnv, name: &str) -> Option<Self> {
        let dir = host_state_dir(env, "oauth-refresh")?;
        let path = dir.join(name);
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            return None;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .ok()?;
        file.lock().ok()?;
        Some(Self(file))
    }

    fn read_stamp(&mut self) -> Option<i64> {
        self.0.seek(SeekFrom::Start(0)).ok()?;
        let mut s = String::new();
        std::io::Read::by_ref(&mut self.0)
            .take(64)
            .read_to_string(&mut s)
            .ok()?;
        s.trim().parse::<i64>().ok()
    }

    fn write_stamp(&mut self, ts: i64) {
        if self.0.set_len(0).is_ok() && self.0.seek(SeekFrom::Start(0)).is_ok() {
            let _ = writeln!(self.0, "{ts}");
            let _ = self.0.flush();
        }
    }
}

fn host_state_dir(env: &ExecEnv, subdir: &str) -> Option<PathBuf> {
    let root = env
        .data_dir
        .as_deref()
        .filter(|d| !d.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = root.join(subdir);
    if fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink()) {
        return None;
    }
    aaf_fp::fs_private::create_dir(&dir).ok()?;
    Some(dir)
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

fn matches_real_host_home(env: &ExecEnv) -> Option<PathBuf> {
    let env_home = env.home.as_deref().filter(|h| !h.trim().is_empty())?;
    let proc_home = std::env::var_os("HOME")?;
    let c_env = fs::canonicalize(env_home).ok()?;
    let c_proc = fs::canonicalize(&proc_home).ok()?;
    (c_env == c_proc).then_some(c_env)
}

fn validate_host_bin(bin: &str, env: &ExecEnv, task_cwd: &str) -> bool {
    let path = Path::new(bin);
    if !path.is_absolute() || !path.is_file() {
        return false;
    }
    let Ok(canon) = fs::canonicalize(path) else {
        return false;
    };
    if !task_cwd.is_empty()
        && let Ok(c_cwd) = fs::canonicalize(task_cwd)
        && canon.starts_with(&c_cwd)
    {
        return false;
    }
    for tmp in &env.temp_dirs {
        if let Ok(c_tmp) = fs::canonicalize(tmp)
            && canon.starts_with(&c_tmp)
        {
            return false;
        }
    }
    true
}

fn sbpl_quote(path: &Path) -> Option<String> {
    let canon = sandbox::canonical(&path.to_string_lossy()).ok()?;
    let s = canon.to_str()?;
    if s.contains(['"', '\\', '\n', '\r', '\0']) {
        return None;
    }
    Some(format!("\"{s}\""))
}

pub(super) fn build_seatbelt_profile(
    empty_cwd: &Path,
    keychain_dir: &Path,
    data_dir: Option<&Path>,
    ssh_dir: Option<&Path>,
    task_cwd: Option<&Path>,
) -> Option<String> {
    let cwd_q = sbpl_quote(empty_cwd)?;
    let kc_q = sbpl_quote(keychain_dir)?;
    let mut p = format!(
        "(version 1)\n(allow default)\n(deny file-write*)\n\
         (allow file-write* (literal \"/dev/null\") (literal \"/dev/zero\") \
         (literal \"/dev/tty\") (literal \"/dev/dtracehelper\") \
         (regex #\"^/dev/fd/[0-9]+$\"))\n\
         (allow file-write* (subpath {cwd_q}))\n\
         (allow file-write* (subpath {kc_q}))\n"
    );
    for denied in [data_dir, ssh_dir, task_cwd].into_iter().flatten() {
        if let Some(q) = sbpl_quote(denied) {
            p.push_str(&format!("(deny file-read* file-write* (subpath {q}))\n"));
        }
    }
    Some(p)
}

fn zeroize_bytes(bytes: &mut [u8]) {
    for b in bytes.iter_mut() {
        *b = 0;
    }
    std::sync::atomic::compiler_fence(Ordering::SeqCst);
}

fn keychain_account_name(env: &ExecEnv) -> Option<String> {
    let user = lookup_env(env, "USER")?;
    if !user.is_empty()
        && user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        Some(user)
    } else {
        Some("claude-code-user".to_string())
    }
}

fn read_claude_expiry_ms(env: &ExecEnv, service: &str) -> Option<(i64, bool)> {
    let query = |with_acct: Option<&str>| -> Option<(i64, bool)> {
        let mut cmd = Command::new(SECURITY_BIN);
        cmd.arg("find-generic-password");
        if let Some(acct) = with_acct {
            cmd.args(["-a", acct]);
        }
        cmd.args(["-s", service, "-w"]);
        let mut fin = run_with_timeout(cmd, SECURITY_TIMEOUT, true).ok()?;
        let ok = fin.status.is_some_and(|st| st.success());
        let parsed = if ok {
            std::str::from_utf8(&fin.stdout)
                .ok()
                .and_then(parse_claude_oauth_expiry_ms)
        } else {
            None
        };
        zeroize_bytes(&mut fin.stdout);
        zeroize_bytes(&mut fin.stderr);
        parsed
    };
    if let Some(acct) = keychain_account_name(env)
        && let Some(hit) = query(Some(&acct))
    {
        return Some(hit);
    }
    query(None)
}

fn lookup_env(env: &ExecEnv, key: &str) -> Option<String> {
    if let Some((_, val)) = env.parent_env.iter().rev().find(|(k, _)| k == key) {
        return Some(val.clone());
    }
    if env.parent_env.is_empty() {
        return std::env::var(key).ok();
    }
    None
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

fn build_host_sandbox_cmd(
    bin: &str,
    args: &[&str],
    env: &ExecEnv,
    home: &Path,
    task_cwd: &str,
    empty_cwd: &Path,
) -> Option<Command> {
    let kc_dir = home.join("Library/Keychains");
    let ssh_dir = home.join(".ssh");
    let data_dir = env
        .data_dir
        .as_deref()
        .filter(|d| !d.trim().is_empty())
        .map(Path::new);
    let task_path = (!task_cwd.trim().is_empty()).then(|| Path::new(task_cwd));
    let profile = build_seatbelt_profile(empty_cwd, &kc_dir, data_dir, Some(&ssh_dir), task_path)?;
    let mut cmd = Command::new(SANDBOX_EXEC);
    apply_clean_host_env(&mut cmd, env, empty_cwd, home);
    cmd.current_dir(empty_cwd)
        .args(["-p", &profile, "--", bin])
        .args(args);
    Some(cmd)
}

fn run_claude_host_ping(
    claude_bin: &str,
    env: &ExecEnv,
    home: &Path,
    task_cwd: &str,
    config_dir: Option<&str>,
) -> bool {
    if !validate_host_bin(claude_bin, env, task_cwd) {
        return false;
    }
    let Some(tmp) = EmptyTempDir::new(env) else {
        return false;
    };
    let Some(mut cmd) = build_host_sandbox_cmd(
        claude_bin,
        &[
            "-p",
            "--model",
            "haiku",
            "--max-turns",
            "1",
            "--tools",
            "",
            "--strict-mcp-config",
            "--setting-sources",
            "",
            "--disable-slash-commands",
            "--permission-mode",
            "plan",
            "--no-session-persistence",
            "Reply OK",
        ],
        env,
        home,
        task_cwd,
        tmp.path(),
    ) else {
        return false;
    };
    cmd.env("CLAUDE_CODE_SAFE_MODE", "1")
        .env("DISABLE_AUTOUPDATER", "1")
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");
    if let Some(dir) = config_dir.filter(|s| !s.trim().is_empty()) {
        cmd.env("CLAUDE_CONFIG_DIR", dir);
    }
    run_with_timeout(cmd, CLAUDE_REFRESH_TIMEOUT, false)
        .ok()
        .is_some_and(|fin| fin.status.is_some_and(|st| st.success()))
}

pub(super) fn preflight_claude_impl(
    claude_bin: &str,
    env: &ExecEnv,
    task_cwd: &str,
    config_dir: Option<&str>,
) -> Option<i64> {
    let home = matches_real_host_home(env)?;
    if config_dir.is_none() && !home.join(".claude").is_dir() {
        return None;
    }
    let service = claude_keychain_service(config_dir);
    let (expires_at, has_refresh) = read_claude_expiry_ms(env, &service)?;
    if !has_refresh || expires_at > now_ms().saturating_add(CLAUDE_REFRESH_LEAD_MS) {
        return Some(expires_at);
    }
    let lock_name = format!("claude-{}.lock", sha256_prefix_hex(&service, 8));
    let _lock = CrossProcessLock::acquire(env, &lock_name)?;
    if let Some((exp, true)) = read_claude_expiry_ms(env, &service) {
        if exp > now_ms().saturating_add(CLAUDE_REFRESH_LEAD_MS) {
            return Some(exp);
        }
        let _ = run_claude_host_ping(claude_bin, env, &home, task_cwd, config_dir);
    }
    read_claude_expiry_ms(env, &service).map(|(exp, _)| exp)
}

pub(super) fn refresh_claude_now_impl(
    claude_bin: &str,
    env: &ExecEnv,
    task_cwd: &str,
    config_dir: Option<&str>,
    pre_run_expiry_ms: Option<i64>,
) -> bool {
    let Some(home) = matches_real_host_home(env) else {
        return false;
    };
    let service = claude_keychain_service(config_dir);
    let lock_name = format!("claude-{}.lock", sha256_prefix_hex(&service, 8));
    let Some(_lock) = CrossProcessLock::acquire(env, &lock_name) else {
        return false;
    };
    let current = read_claude_expiry_ms(env, &service);
    let min_valid = now_ms().saturating_add(CLAUDE_REFRESH_MIN_VALID_MS);
    let lead_cut = now_ms().saturating_add(CLAUDE_REFRESH_LEAD_MS);
    if let Some((exp, _)) = current
        && exp > lead_cut
    {
        // Only treat as recovered if a peer strictly advanced expiry since pre-run.
        return pre_run_expiry_ms.is_some_and(|before| exp > before);
    }
    if !run_claude_host_ping(claude_bin, env, &home, task_cwd, config_dir) {
        return false;
    }
    read_claude_expiry_ms(env, &service)
        .is_some_and(|(exp, _)| exp > min_valid && pre_run_expiry_ms.is_none_or(|b| exp > b))
}

fn is_real_agy_keychain_env(env: &ExecEnv) -> Option<PathBuf> {
    let home = matches_real_host_home(env)?;
    if !home
        .join(".gemini/antigravity-cli/installation_id")
        .is_file()
    {
        return None;
    }
    // Metadata-only Keychain presence check (no `-w` secret read).
    let mut cmd = Command::new(SECURITY_BIN);
    cmd.args(["find-generic-password", "-s", "gemini", "-a", "antigravity"]);
    let ok = run_with_timeout(cmd, SECURITY_TIMEOUT, false)
        .ok()
        .is_some_and(|fin| fin.status.is_some_and(|st| st.success()));
    ok.then_some(home)
}

fn run_agy_models_host(agy_bin: &str, env: &ExecEnv, home: &Path, task_cwd: &str) -> bool {
    if !validate_host_bin(agy_bin, env, task_cwd) {
        return false;
    }
    let Some(tmp) = EmptyTempDir::new(env) else {
        return false;
    };
    let Some(mut cmd) =
        build_host_sandbox_cmd(agy_bin, &["models"], env, home, task_cwd, tmp.path())
    else {
        return false;
    };
    cmd.env("AGY_CLI_DISABLE_AUTO_UPDATE", "1");
    run_with_timeout(cmd, AGY_REFRESH_TIMEOUT, false)
        .ok()
        .is_some_and(|fin| fin.status.is_some_and(|st| st.success()))
}

pub(super) fn preflight_agy_impl(agy_bin: &str, env: &ExecEnv, task_cwd: &str) {
    let Some(mut lock) = CrossProcessLock::acquire(env, "agy.lock") else {
        return;
    };
    let now = now_secs();
    if lock
        .read_stamp()
        .is_some_and(|ts| now.saturating_sub(ts) < AGY_REFRESH_INTERVAL_SECS)
    {
        return;
    }
    let Some(home) = is_real_agy_keychain_env(env) else {
        return;
    };
    if run_agy_models_host(agy_bin, env, &home, task_cwd) {
        lock.write_stamp(now_secs());
    }
}

pub(super) fn refresh_agy_now_impl(agy_bin: &str, env: &ExecEnv, task_cwd: &str) -> bool {
    let Some(mut lock) = CrossProcessLock::acquire(env, "agy.lock") else {
        return false;
    };
    let Some(home) = is_real_agy_keychain_env(env) else {
        return false;
    };
    if run_agy_models_host(agy_bin, env, &home, task_cwd) {
        lock.write_stamp(now_secs());
        return true;
    }
    false
}
