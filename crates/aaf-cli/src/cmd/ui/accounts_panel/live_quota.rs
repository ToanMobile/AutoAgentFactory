//! Live quota probes for non-Claude providers (`agy -p=/usage --output-format json`
//! for Google / Antigravity and PTY `/usage` for Grok CLI) with a home-keyed TTL
//! cache, serialized refresh, and process-group cleanup.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::ports::AccountStore;
use aaf_engine::db::Db;
use aaf_engine::executors::agy::AgyRunner;
use aaf_engine::executors::registry::{find_descriptor, resolve_binary_with_home};
use serde_json::{Value, json};

#[cfg(test)]
pub(crate) use super::live_quota_parse::parse_rfc3339_utc_ms;
pub(crate) use super::live_quota_parse::{parse_agy_usage_output, parse_grok_pty_usage};
use super::live_quota_parse::parse_grok_usage_json;
use crate::cmd::ui::extras::read_regular_bounded;
use crate::ctx::Ctx;

const FALLBACK_EXHAUSTED_COOLDOWN_SECS: i64 = 3600;
const LIVE_QUOTA_TTL: Duration = Duration::from_secs(60);
const AGY_USAGE_TIMEOUT: Duration = Duration::from_secs(15);
const GROK_USAGE_TIMEOUT: Duration = Duration::from_secs(5);
const GROK_PTY_USAGE_TIMEOUT: Duration = Duration::from_secs(12);
const MAX_STDOUT_BYTES: u64 = 512 * 1024;
const MAX_STDERR_BYTES: u64 = 64 * 1024;
const MAX_GOOGLE_ACCOUNTS_BYTES: u64 = 64 * 1024;

const GROK_PTY_SCRIPT: &str = r#"
import fcntl, os, pty, re, select, signal, struct, subprocess, sys, termios, time
def _on_term(_s, _f):
    raise SystemExit(124)
signal.signal(signal.SIGTERM, _on_term)
bin_path = sys.argv[1]
master, slave = pty.openpty()
p = None
buf = bytearray()
got_limit = False
try:
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
    env = os.environ.copy()
    env["TERM"] = "xterm-256color"
    p = subprocess.Popen(
        [bin_path, "--no-alt-screen", "--no-memory", "--minimal"],
        stdin=slave, stdout=slave, stderr=slave, env=env, close_fds=True, start_new_session=True
    )
    os.close(slave)
    slave = -1
    t0 = time.monotonic()
    sent = False
    while time.monotonic() - t0 < 9.5 and len(buf) < 262144:
        r, _, _ = select.select([master], [], [], 0.15)
        if r:
            try:
                chunk = os.read(master, 8192)
                if not chunk:
                    break
                buf.extend(chunk)
                if b"\x1b[6n" in chunk:
                    os.write(master, b"\x1b[1;1R")
                if b"\x1b[c" in chunk or b"\x1b[0c" in chunk:
                    os.write(master, b"\x1b[?62;c")
                if sent:
                    nr = buf.find(b"Next reset:")
                    if (nr != -1 and b"\n" in buf[nr:]) or (b"Weekly limit:" in buf and time.monotonic() - t0 > 5.8):
                        got_limit = True
                        break
            except OSError:
                break
        if not sent and time.monotonic() - t0 > 2.8:
            try:
                os.write(master, b"/usage\r")
                sent = True
            except OSError:
                break
finally:
    if slave != -1:
        try: os.close(slave)
        except OSError: pass
    try: os.write(master, b"\x03\x03")
    except OSError: pass
    if p is not None:
        try: os.killpg(p.pid, signal.SIGTERM)
        except OSError: pass
        try: p.wait(timeout=1.0)
        except Exception:
            try: os.killpg(p.pid, signal.SIGKILL)
            except OSError: pass
            try: p.wait(timeout=1.0)
            except Exception: pass
    try: os.close(master)
    except OSError: pass
clean = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[()][0-9A-Za-z]|\x1b[@-Z\\-_=>]|\r", "", bytes(buf).decode("utf-8", "replace"))
sys.stdout.write(clean)
sys.exit(0 if got_limit or "Weekly limit:" in clean else 1)
"#;

struct CacheEntry {
    seq: u64,
    fetched_at: Instant,
    payload: Value,
}

static LIVE_QUOTA_CACHE: OnceLock<Mutex<HashMap<PathBuf, CacheEntry>>> = OnceLock::new();
static LIVE_QUOTA_PROBE_LOCK: Mutex<()> = Mutex::new(());
static LIVE_QUOTA_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static LIVE_QUOTA_SEQ: AtomicU64 = AtomicU64::new(1);

struct InFlightGuard;
impl Drop for InFlightGuard {
    fn drop(&mut self) {
        LIVE_QUOTA_IN_FLIGHT.store(false, Ordering::Release);
    }
}

fn cache() -> &'static Mutex<HashMap<PathBuf, CacheEntry>> {
    LIVE_QUOTA_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(unix)]
fn signal_process_group(pid: u32, sig: &str) {
    let pgid = format!("-{pid}");
    let _ = Command::new("/bin/kill")
        .args([sig, &pgid])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn terminate_child(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id();
        signal_process_group(pid, "-TERM");
        let grace = Instant::now() + Duration::from_millis(300);
        while Instant::now() < grace {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        signal_process_group(pid, "-KILL");
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn run_cmd_bounded(
    ctx: &Ctx,
    bin: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<String, String> {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("HOME", &ctx.home)
        .env("PATH", &ctx.path_var);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", bin.display()))?;
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let out_reader = stdout_pipe.map(|pipe| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.take(MAX_STDOUT_BYTES + 1).read_to_end(&mut buf);
            buf
        })
    });
    let err_reader = stderr_pipe.map(|pipe| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.take(MAX_STDERR_BYTES + 1).read_to_end(&mut buf);
            buf
        })
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if Instant::now() >= deadline => {
                terminate_child(&mut child);
                if let Some(h) = out_reader {
                    let _ = h.join();
                }
                if let Some(h) = err_reader {
                    let _ = h.join();
                }
                return Err(format!("timed out after {}s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(40)),
            Err(e) => {
                terminate_child(&mut child);
                if let Some(h) = out_reader {
                    let _ = h.join();
                }
                if let Some(h) = err_reader {
                    let _ = h.join();
                }
                return Err(format!("wait failed: {e}"));
            }
        }
    };
    let out_bytes = out_reader.and_then(|h| h.join().ok()).unwrap_or_default();
    let err_bytes = err_reader.and_then(|h| h.join().ok()).unwrap_or_default();
    let out = String::from_utf8_lossy(&out_bytes).into_owned();
    let err = String::from_utf8_lossy(&err_bytes).into_owned();
    if status.success() {
        Ok(out)
    } else {
        let detail = if !err.trim().is_empty() {
            err.trim()
        } else {
            out.trim()
        };
        Err(detail.chars().take(240).collect())
    }
}

fn read_active_google_email(home: &Path) -> Option<String> {
    let gemini_dir = home.join(".gemini");
    let dmeta = std::fs::symlink_metadata(&gemini_dir).ok()?;
    if dmeta.file_type().is_symlink() || !dmeta.file_type().is_dir() {
        return None;
    }
    let raw = read_regular_bounded(
        &gemini_dir.join("google_accounts.json"),
        MAX_GOOGLE_ACCOUNTS_BYTES,
    )
    .ok()?;
    let val: Value = serde_json::from_str(&raw).ok()?;
    let email = val["active"].as_str()?.trim();
    if email.is_empty() || email.len() > 254 || email.chars().any(char::is_control) {
        return None;
    }
    Some(email.to_string())
}

fn resolve_agy_bin(ctx: &Ctx) -> Option<PathBuf> {
    AgyRunner::resolve_binary(&ctx.path_var).or_else(|| {
        let fallback = ctx.home.join(".local/bin/agy");
        fallback.is_file().then_some(fallback)
    })
}

fn probe_antigravity_quota(ctx: &Ctx) -> Value {
    let fetched_ms = now_ms();
    let Some(bin) = resolve_agy_bin(ctx) else {
        return json!({ "live": false, "reason": "not_installed" });
    };
    let args = ["-p=/usage", "--mode", "plan", "--output-format", "json"];
    let mut out_val = match run_cmd_bounded(ctx, &bin, &args, AGY_USAGE_TIMEOUT) {
        Ok(out) => parse_agy_usage_output(&out, fetched_ms).unwrap_or_else(|| {
            json!({ "live": false, "reason": "parse_failed", "fetchedAtMs": fetched_ms })
        }),
        Err(err) => {
            json!({ "live": false, "reason": "error", "error": err, "fetchedAtMs": fetched_ms })
        }
    };
    if out_val["live"].as_bool() == Some(true)
        && let Some(email) = read_active_google_email(&ctx.home)
        && let Some(obj) = out_val.as_object_mut()
    {
        obj.insert("accountEmail".into(), json!(email));
    }
    out_val
}

fn find_python3(ctx: &Ctx) -> Option<PathBuf> {
    for dir in std::env::split_paths(&ctx.path_var) {
        let p = dir.join("python3");
        if p.is_file() {
            return Some(p);
        }
    }
    for cand in [
        "/usr/bin/python3",
        "/opt/homebrew/bin/python3",
        "/usr/local/bin/python3",
    ] {
        let p = PathBuf::from(cand);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn probe_grok_quota(ctx: &Ctx) -> Value {
    let fetched_ms = now_ms();
    let bin = find_descriptor("grok")
        .and_then(|d| resolve_binary_with_home(d, &ctx.path_var, Some(&ctx.home)));
    let Some(bin) = bin else {
        return json!({ "live": false, "reason": "not_installed" });
    };
    if let Ok(out) = run_cmd_bounded(ctx, &bin, &["usage", "--json"], GROK_USAGE_TIMEOUT)
        && let Some(parsed) = parse_grok_usage_json(&out, fetched_ms)
    {
        return parsed;
    }
    if let Some(py) = find_python3(ctx) {
        let bin_str = bin.to_string_lossy().into_owned();
        if let Ok(out) = run_cmd_bounded(
            ctx,
            &py,
            &["-c", GROK_PTY_SCRIPT, &bin_str],
            GROK_PTY_USAGE_TIMEOUT,
        ) && let Some(parsed) = parse_grok_pty_usage(&out, fetched_ms)
        {
            return parsed;
        }
    }
    json!({ "live": false, "reason": "tui_only", "fetchedAtMs": fetched_ms })
}

fn probe_and_store(ctx: &Ctx) -> Value {
    let _probe_guard = LIVE_QUOTA_PROBE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let seq = LIVE_QUOTA_SEQ.fetch_add(1, Ordering::AcqRel);
    let ctx_grok = ctx.clone();
    let grok_handle = std::thread::Builder::new()
        .name("aaf-grok-quota".into())
        .spawn(move || probe_grok_quota(&ctx_grok));
    let agy = probe_antigravity_quota(ctx);
    let grok = match grok_handle {
        Ok(h) => h
            .join()
            .unwrap_or_else(|_| json!({ "live": false, "reason": "unavailable" })),
        Err(_) => probe_grok_quota(ctx),
    };
    let payload = json!({
        "antigravity": agy,
        "grok": grok,
    });
    let mut map = cache().lock().unwrap_or_else(|e| e.into_inner());
    let should_store = map.get(&ctx.home).is_none_or(|cur| seq >= cur.seq);
    if should_store {
        map.insert(
            ctx.home.clone(),
            CacheEntry {
                seq,
                fetched_at: Instant::now(),
                payload: payload.clone(),
            },
        );
    }
    payload
}

/// Fetches cached or freshly probed live `/usage` quotas for Google
/// (Antigravity / Gemini) and Grok CLI. Never spawns subprocesses in `read_only`
/// mode; on cold non-refresh reads, warms the cache asynchronously so HTTP
/// latency stays near-zero; on `force_refresh`, probes `agy` and `grok` in parallel.
pub(super) fn fetch_provider_quotas(ctx: &Ctx, read_only: bool, force_refresh: bool) -> Value {
    let (warm, stale_snapshot) = {
        let map = cache().lock().unwrap_or_else(|e| e.into_inner());
        match map.get(&ctx.home) {
            Some(e) => (e.fetched_at.elapsed() < LIVE_QUOTA_TTL, Some(e.payload.clone())),
            None => (false, None),
        }
    };
    if !force_refresh && warm {
        return stale_snapshot.unwrap_or_else(|| json!({}));
    }
    if read_only {
        return stale_snapshot.unwrap_or_else(|| json!({}));
    }
    if !force_refresh {
        if LIVE_QUOTA_IN_FLIGHT
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let bg_ctx = ctx.clone();
            if std::thread::Builder::new()
                .name("aaf-quota-warm".into())
                .spawn(move || {
                    let _guard = InFlightGuard;
                    let _ = probe_and_store(&bg_ctx);
                })
                .is_err()
            {
                LIVE_QUOTA_IN_FLIGHT.store(false, Ordering::Release);
            }
        }
        return stale_snapshot.unwrap_or_else(|| json!({}));
    }
    probe_and_store(ctx)
}

/// Syncs a live-exhausted non-Claude provider quota (`remainingPct <= 0` or
/// `weeklyRemainingPct <= 0`) into `aaf.db` cooldowns without shortening longer ones.
pub(super) fn sync_live_provider_cooldown(
    db: &Db,
    discovered: &[AccountProfile],
    provider: ProviderKind,
    quota: Option<&Value>,
    now_secs: i64,
) -> usize {
    let Some(q) = quota.filter(|v| v["live"].as_bool() == Some(true)) else {
        return 0;
    };
    let rem5_zero = q["remainingPct"].as_i64().is_some_and(|v| v <= 0);
    let rem7_zero = q["weeklyRemainingPct"].as_i64().is_some_and(|v| v <= 0);
    if !rem5_zero && !rem7_zero {
        return 0;
    }
    let now_ms = now_secs.saturating_mul(1000);
    let window_until = |exhausted: bool, reset_ms: i64| -> i64 {
        if !exhausted {
            return 0;
        }
        if reset_ms > now_ms {
            reset_ms.saturating_add(999) / 1000
        } else if reset_ms <= 0 {
            now_secs.saturating_add(FALLBACK_EXHAUSTED_COOLDOWN_SECS)
        } else {
            0
        }
    };
    let u5 = window_until(rem5_zero, q["resetAtMs"].as_i64().unwrap_or(0));
    let u7 = window_until(rem7_zero, q["weeklyResetAtMs"].as_i64().unwrap_or(0));
    let until_secs = u5.max(u7);
    if until_secs <= now_secs {
        return 0;
    }
    let Ok(accounts) = db.list_accounts() else {
        return 0;
    };
    let existing: HashMap<String, Option<i64>> = accounts
        .into_iter()
        .map(|s| (s.profile.id, s.cooldown_until))
        .collect();
    let mut synced = 0usize;
    for acct in discovered.iter().filter(|a| a.provider == provider) {
        let cur_until = existing.get(&acct.id).copied().flatten().unwrap_or(0);
        if until_secs > cur_until && db.set_cooldown(&acct.id, Some(until_secs)).is_ok() {
            synced += 1;
        }
    }
    synced
}
