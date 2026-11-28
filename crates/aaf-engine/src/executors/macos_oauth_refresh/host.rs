//! macOS host-side OAuth refresh execution under minimal Seatbelt + file locks.

use std::fs::{self, DirBuilder, File, OpenOptions, TryLockError};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::seatbelt::{build_host_sandbox_cmd, lookup_env, validate_host_bin};
use super::{ExecEnv, claude_keychain_service, parse_claude_oauth_expiry_ms, sha256_prefix_hex};
use crate::process::run_with_timeout;

/// Refresh Claude OAuth proactively when the Keychain access token expires
/// within 10 minutes.
const CLAUDE_REFRESH_LEAD_MS: i64 = 10 * 60 * 1000;

/// Minimum remaining lifetime required after a host refresh to treat it as valid.
const CLAUDE_REFRESH_MIN_VALID_MS: i64 = 60 * 1000;

/// Refresh Antigravity (`agy`) OAuth proactively at most once every 40 minutes
/// across processes (Google OAuth access tokens expire every 60 minutes).
const AGY_REFRESH_INTERVAL_SECS: i64 = 40 * 60;

/// Backoff after a failed preflight attempt so outages / revoked tokens never
/// stall every worker run.
const FAILED_ATTEMPT_BACKOFF_SECS: i64 = 5 * 60;

/// Shorter backoff on reactive refresh after a failed host attempt so N
/// concurrent workers never stampede on a revoked token or outage.
const REACTIVE_ERR_BACKOFF_SECS: i64 = 60;

/// Maximum wait time when acquiring the reactive cross-process refresh lock.
const REACTIVE_LOCK_TIMEOUT: Duration = Duration::from_secs(30);

const SECURITY_BIN: &str = "/usr/bin/security";
const SECURITY_TIMEOUT: Duration = Duration::from_secs(5);
const CLAUDE_REFRESH_TIMEOUT: Duration = Duration::from_secs(25);
const AGY_REFRESH_TIMEOUT: Duration = Duration::from_secs(15);

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

#[derive(Debug, Clone, Copy)]
enum Stamp {
    Ok(i64),
    Err(i64),
}

struct CrossProcessLock(File);

impl CrossProcessLock {
    fn open_file(env: &ExecEnv, name: &str) -> Option<File> {
        use std::os::unix::fs::MetadataExt as _;

        let dir = host_state_dir(env, "oauth-refresh")?;
        let dir_uid = fs::symlink_metadata(&dir).ok()?.uid();
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
        let meta = file.metadata().ok()?;
        if !meta.is_file() || meta.uid() != dir_uid {
            return None;
        }
        Some(file)
    }

    fn acquire_with_timeout(env: &ExecEnv, name: &str, timeout: Duration) -> Option<Self> {
        let file = Self::open_file(env, name)?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match file.try_lock() {
                Ok(()) => return Some(Self(file)),
                Err(TryLockError::WouldBlock) => {
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(TryLockError::Error(_)) => return None,
            }
        }
    }

    fn try_acquire(env: &ExecEnv, name: &str) -> Option<Self> {
        let file = Self::open_file(env, name)?;
        match file.try_lock() {
            Ok(()) => Some(Self(file)),
            Err(TryLockError::WouldBlock | TryLockError::Error(_)) => None,
        }
    }

    fn read_stamp(&mut self) -> Option<Stamp> {
        self.0.seek(SeekFrom::Start(0)).ok()?;
        let mut s = String::new();
        std::io::Read::by_ref(&mut self.0)
            .take(64)
            .read_to_string(&mut s)
            .ok()?;
        let trimmed = s.trim();
        if let Some(rest) = trimmed.strip_prefix("ok:") {
            rest.parse::<i64>().ok().map(Stamp::Ok)
        } else if let Some(rest) = trimmed.strip_prefix("err:") {
            rest.parse::<i64>().ok().map(Stamp::Err)
        } else {
            trimmed.parse::<i64>().ok().map(Stamp::Ok)
        }
    }

    fn write_stamp(&mut self, stamp: Stamp) {
        let line = match stamp {
            Stamp::Ok(ts) => format!("ok:{ts}"),
            Stamp::Err(ts) => format!("err:{ts}"),
        };
        if self.0.set_len(0).is_ok() && self.0.seek(SeekFrom::Start(0)).is_ok() {
            let _ = writeln!(self.0, "{line}");
            let _ = self.0.flush();
        }
    }
}

fn host_state_dir(env: &ExecEnv, subdir: &str) -> Option<PathBuf> {
    let root: PathBuf = env
        .data_dir
        .as_deref()
        .filter(|d| !d.trim().is_empty())
        .map(|d| Path::new(d).components().collect())?;
    if fs::symlink_metadata(&root).is_ok_and(|m| m.file_type().is_symlink()) {
        return None;
    }
    let dir = root.join(subdir);
    if fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink()) {
        return None;
    }
    aaf_fp::fs_private::create_dir(&dir).ok()?;
    if fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink()) {
        return None;
    }
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

fn zeroize_bytes(bytes: &mut [u8]) {
    for b in bytes.iter_mut() {
        *b = 0;
    }
    std::sync::atomic::compiler_fence(Ordering::SeqCst);
    std::hint::black_box(bytes);
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

/// Reads ONLY the metadata modification timestamp (`mdat`) of the Antigravity
/// Keychain item (`svce="gemini"`, `acct="antigravity"`) without `-w` (no secret
/// bytes are ever read).
fn read_agy_keychain_mdat() -> Option<String> {
    let mut cmd = Command::new(SECURITY_BIN);
    cmd.args(["find-generic-password", "-s", "gemini", "-a", "antigravity"]);
    let fin = run_with_timeout(cmd, SECURITY_TIMEOUT, true).ok()?;
    if !fin.status.is_some_and(|st| st.success()) {
        return None;
    }
    let text = String::from_utf8_lossy(&fin.stdout);
    text.lines()
        .find(|line| line.contains("\"mdat\""))
        .map(|line| line.trim().to_string())
}

fn run_claude_host_ping(
    claude_bin: &str,
    env: &ExecEnv,
    home: &Path,
    task_cwd: &str,
    config_dir: Option<&str>,
) -> bool {
    if !validate_host_bin(claude_bin, env, home, task_cwd, config_dir) {
        return false;
    }
    let Some(tmp) = EmptyTempDir::new(env) else {
        return false;
    };
    let cfg_path = config_dir
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"));
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
            "--settings",
            r#"{"disableAllHooks":true}"#,
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
        Some(&cfg_path),
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
    let Some(mut lock) = CrossProcessLock::try_acquire(env, &lock_name) else {
        return Some(expires_at);
    };
    if let Some(Stamp::Err(ts)) = lock.read_stamp()
        && now_secs().saturating_sub(ts) < FAILED_ATTEMPT_BACKOFF_SECS
    {
        return Some(expires_at);
    }
    let before = read_claude_expiry_ms(env, &service).map(|(e, _)| e);
    if before.is_some_and(|e| e > now_ms().saturating_add(CLAUDE_REFRESH_LEAD_MS)) {
        return before;
    }
    let _ = run_claude_host_ping(claude_bin, env, &home, task_cwd, config_dir);
    let after = read_claude_expiry_ms(env, &service).map(|(e, _)| e);
    let refreshed = after.is_some_and(|e| {
        e > now_ms().saturating_add(CLAUDE_REFRESH_MIN_VALID_MS) && before.is_none_or(|b| e > b)
    });
    lock.write_stamp(if refreshed {
        Stamp::Ok(now_secs())
    } else {
        Stamp::Err(now_secs())
    });
    after
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
    let Some(mut lock) =
        CrossProcessLock::acquire_with_timeout(env, &lock_name, REACTIVE_LOCK_TIMEOUT)
    else {
        return false;
    };
    let current = read_claude_expiry_ms(env, &service);
    let lead_cut = now_ms().saturating_add(CLAUDE_REFRESH_LEAD_MS);
    if let Some((exp, _)) = current
        && exp > lead_cut
    {
        // Only treat as recovered if a peer strictly advanced expiry since pre-run.
        return pre_run_expiry_ms.is_some_and(|before| exp > before);
    }
    if let Some(Stamp::Err(ts)) = lock.read_stamp()
        && now_secs().saturating_sub(ts) < REACTIVE_ERR_BACKOFF_SECS
    {
        return false;
    }
    let baseline = pre_run_expiry_ms.or_else(|| current.map(|(e, _)| e));
    // Re-read Keychain regardless of ping exit code: Claude may have saved the
    // refreshed token before a subsequent inference call failed.
    let _ = run_claude_host_ping(claude_bin, env, &home, task_cwd, config_dir);
    let min_valid = now_ms().saturating_add(CLAUDE_REFRESH_MIN_VALID_MS);
    let refreshed = read_claude_expiry_ms(env, &service).is_some_and(|(after, _)| {
        after > min_valid && baseline.map_or(current.is_none(), |b| after > b)
    });
    lock.write_stamp(if refreshed {
        Stamp::Ok(now_secs())
    } else {
        Stamp::Err(now_secs())
    });
    refreshed
}

fn is_real_agy_keychain_env(env: &ExecEnv) -> Option<(PathBuf, String)> {
    let home = matches_real_host_home(env)?;
    if !home
        .join(".gemini/antigravity-cli/installation_id")
        .is_file()
    {
        return None;
    }
    let mdat = read_agy_keychain_mdat()?;
    Some((home, mdat))
}

fn run_agy_models_host(agy_bin: &str, env: &ExecEnv, home: &Path, task_cwd: &str) -> bool {
    if !validate_host_bin(agy_bin, env, home, task_cwd, None) {
        return false;
    }
    let Some(tmp) = EmptyTempDir::new(env) else {
        return false;
    };
    let Some(mut cmd) =
        build_host_sandbox_cmd(agy_bin, &["models"], env, home, task_cwd, tmp.path(), None)
    else {
        return false;
    };
    cmd.env("AGY_CLI_DISABLE_AUTO_UPDATE", "1");
    run_with_timeout(cmd, AGY_REFRESH_TIMEOUT, false)
        .ok()
        .is_some_and(|fin| fin.status.is_some_and(|st| st.success()))
}

pub(super) fn preflight_agy_impl(agy_bin: &str, env: &ExecEnv, task_cwd: &str) -> Option<String> {
    let (home, mdat) = is_real_agy_keychain_env(env)?;
    let Some(mut lock) = CrossProcessLock::try_acquire(env, "agy.lock") else {
        return Some(mdat);
    };
    let now = now_secs();
    match lock.read_stamp() {
        Some(Stamp::Ok(ts)) if now.saturating_sub(ts) < AGY_REFRESH_INTERVAL_SECS => {
            return Some(mdat);
        }
        Some(Stamp::Err(ts)) if now.saturating_sub(ts) < FAILED_ATTEMPT_BACKOFF_SECS => {
            return Some(mdat);
        }
        _ => {}
    }
    if run_agy_models_host(agy_bin, env, &home, task_cwd) {
        lock.write_stamp(Stamp::Ok(now_secs()));
    } else {
        lock.write_stamp(Stamp::Err(now_secs()));
    }
    read_agy_keychain_mdat().or(Some(mdat))
}

pub(super) fn refresh_agy_now_impl(
    agy_bin: &str,
    env: &ExecEnv,
    task_cwd: &str,
    pre_run_mdat: Option<&str>,
) -> bool {
    let Some(mut lock) =
        CrossProcessLock::acquire_with_timeout(env, "agy.lock", REACTIVE_LOCK_TIMEOUT)
    else {
        return false;
    };
    let Some((home, before_mdat)) = is_real_agy_keychain_env(env) else {
        return false;
    };
    if pre_run_mdat.is_some_and(|pre| pre != before_mdat) {
        return true;
    }
    if let Some(Stamp::Err(ts)) = lock.read_stamp()
        && now_secs().saturating_sub(ts) < REACTIVE_ERR_BACKOFF_SECS
    {
        return false;
    }
    let ok = run_agy_models_host(agy_bin, env, &home, task_cwd);
    let after_mdat = read_agy_keychain_mdat();
    let changed = ok && after_mdat.as_deref().is_some_and(|a| a != before_mdat);
    lock.write_stamp(if changed {
        Stamp::Ok(now_secs())
    } else {
        Stamp::Err(now_secs())
    });
    changed
}
