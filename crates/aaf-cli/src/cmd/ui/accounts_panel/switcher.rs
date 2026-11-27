//! Optional `cc-switcher-mcp.js` subprocess integration for live Anthropic OAuth
//! 5h/7d quota reads and `.claude/settings.local.json` repo assignment sync.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::cmd::ui::extras::{canonical_under_allowed_roots, write_repo_local_account_native};
use crate::ctx::Ctx;

const CC_READ_TIMEOUT: Duration = Duration::from_secs(45);
const CC_REFRESH_TIMEOUT: Duration = Duration::from_secs(60);
const CC_CACHE_TTL: Duration = Duration::from_secs(30);
const MAX_CC_STDOUT_BYTES: usize = 512 * 1024;
const MAX_CC_STDERR_BYTES: usize = 4 * 1024;
static CC_WRITE_LOCK: Mutex<()> = Mutex::new(());
static CC_READ_CACHE: Mutex<(u64, Option<(Instant, Value)>)> = Mutex::new((0, None));

fn is_valid_ident(s: &str) -> bool {
    let bytes = s.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 48
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

pub(super) fn find_cc_switcher_script(ctx: &Ctx) -> Option<PathBuf> {
    for env_key in ["AAF_CC_SWITCHER_BIN", "AGENT_OFFICE_CC_SWITCHER"] {
        if let Some((_, v)) = ctx.aaf_env.iter().find(|(k, _)| k == env_key) {
            let p = PathBuf::from(v.trim());
            if p.is_file() {
                return Some(p);
            }
        }
        if let Ok(v) = std::env::var(env_key) {
            let p = PathBuf::from(v.trim());
            if p.is_file() {
                return Some(p);
            }
        }
    }
    let cfg_candidate = ctx.config_dir.join("cc-switcher-mcp.js");
    if cfg_candidate.is_file() {
        return Some(cfg_candidate);
    }
    None
}

fn find_node_bin(ctx: &Ctx) -> Option<PathBuf> {
    for dir in std::env::split_paths(&ctx.path_var) {
        let p = dir.join("node");
        if p.is_file() {
            return Some(p);
        }
    }
    for cand in [
        "/opt/homebrew/bin/node",
        "/usr/local/bin/node",
        "/usr/bin/node",
    ] {
        let p = PathBuf::from(cand);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn invalidate_read_cache() {
    let mut guard = CC_READ_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    guard.0 = guard.0.wrapping_add(1);
    guard.1 = None;
}

fn run_cc_switcher(ctx: &Ctx, args: &[&str]) -> Result<Value, String> {
    let timeout = if args.contains(&"--refresh") {
        CC_REFRESH_TIMEOUT
    } else {
        CC_READ_TIMEOUT
    };
    run_cc_switcher_with_timeout(ctx, args, timeout)
}

fn run_cc_switcher_with_timeout(
    ctx: &Ctx,
    args: &[&str],
    timeout: Duration,
) -> Result<Value, String> {
    let script = find_cc_switcher_script(ctx)
        .ok_or_else(|| "not found: cc-switcher-mcp is not installed on this machine".to_string())?;
    let node = find_node_bin(ctx)
        .ok_or_else(|| "not found: node runtime not found on PATH".to_string())?;
    let mut cmd = Command::new(node);
    cmd.arg(&script)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("PATH", &ctx.path_var)
        .env("HOME", &ctx.home)
        .env("CC_SWITCHER_QUIET", "1");
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cc-switcher spawn failed: {e}"))?;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("cc-switcher stdout unavailable".into());
    };
    let stderr_handle = child.stderr.take().map(|err_pipe| {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = err_pipe
                .take((MAX_CC_STDERR_BYTES + 1) as u64)
                .read_to_end(&mut buf);
            buf
        })
    });
    let reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout
            .take((MAX_CC_STDOUT_BYTES + 1) as u64)
            .read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + timeout;
    let mut exit_status: Option<ExitStatus> = None;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(st)) => {
                exit_status = Some(st);
                break;
            }
            Ok(None) => thread::sleep(Duration::from_millis(30)),
            Err(_) => break,
        }
    }
    let Some(status) = exit_status else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!(
            "cc-switcher timed out after {}s",
            timeout.as_secs()
        ));
    };
    let bytes = reader.join().unwrap_or_default();
    let err_bytes = stderr_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    let last_line = text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    let val: Value = serde_json::from_str(last_line).map_err(|_| {
        let err_tail = String::from_utf8_lossy(&err_bytes);
        let detail = err_tail
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("");
        if detail.is_empty() {
            format!("cc-switcher returned invalid JSON (status {status})")
        } else {
            format!("cc-switcher failed ({status}): {}", detail.trim())
        }
    })?;
    if status.code() == Some(2) {
        let msg = val["error"].as_str().unwrap_or("invalid cc-switcher input");
        return Err(format!("invalid cc-switcher: {msg}"));
    }
    if !status.success() {
        let msg = val["error"]
            .as_str()
            .unwrap_or("cc-switcher command failed");
        return Err(msg.to_string());
    }
    Ok(val)
}

fn is_open_design_app_config(ctx: &Ctx, raw_repo: &str) -> Option<PathBuf> {
    let trimmed = raw_repo.trim();
    if trimmed.is_empty() || trimmed.len() > 1024 || trimmed.chars().any(char::is_control) {
        return None;
    }
    let expected = ctx.home.join(
        "Library/Application Support/Open Design/namespaces/release-stable/data/app-config.json",
    );
    let given = Path::new(trimmed);
    if !given.is_absolute() || given != expected {
        return None;
    }
    let meta = fs::symlink_metadata(given).ok()?;
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return None;
    }
    Some(expected)
}

fn validate_repo_target(
    ctx: &Ctx,
    scoped_repo: Option<&Path>,
    raw_repo: &str,
) -> Result<PathBuf, String> {
    let trimmed = raw_repo.trim();
    let p = PathBuf::from(trimmed);
    if trimmed.is_empty()
        || trimmed.len() > 1024
        || trimmed.chars().any(char::is_control)
        || !p.is_absolute()
        || !p.is_dir()
    {
        return Err("invalid repo: must be an absolute existing git repository path".into());
    }
    let git_root = aaf_fp::repo_root(&p).map_err(|_| {
        "invalid repo: must be an absolute existing git repository path".to_string()
    })?;
    let canon = fs::canonicalize(&git_root).map_err(|e| e.to_string())?;
    if let Some(scoped) = scoped_repo {
        let canon_scoped = fs::canonicalize(scoped).unwrap_or_else(|_| scoped.to_path_buf());
        if canon != canon_scoped {
            return Err("refused: repo outside scoped repository".into());
        }
    }
    let forbidden = [ctx.worktrees_dir.clone(), ctx.data_dir.clone()];
    if canonical_under_allowed_roots(&canon, &forbidden).is_some() {
        return Err("refused: repo cannot be inside AAF worktrees or data directory".into());
    }
    Ok(canon)
}

fn normalize_switcher_value(v: Value) -> Value {
    v.get("state")
        .filter(|s| s.get("profiles").is_some())
        .cloned()
        .unwrap_or(v)
}

fn fallback_switcher_snapshot(ctx: &Ctx) -> Option<Value> {
    let cached = CC_READ_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .1
        .as_ref()
        .map(|(_, v)| v.clone());
    if cached.is_some() {
        return cached;
    }
    run_cc_switcher(ctx, &["--panel-json", "--no-sync"])
        .ok()
        .map(normalize_switcher_value)
}

pub(super) fn fetch_switcher_state(
    ctx: &Ctx,
    read_only: bool,
    refresh: bool,
) -> (bool, Option<Value>, String) {
    if find_cc_switcher_script(ctx).is_none() {
        return (false, None, String::new());
    }
    let start_gen = {
        let guard = CC_READ_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if !refresh
            && let Some((at, ref cached)) = guard.1
            && at.elapsed() < CC_CACHE_TTL
        {
            return (true, Some(cached.clone()), String::new());
        }
        guard.0
    };
    let args: &[&str] = if read_only {
        &["--panel-json", "--no-sync"]
    } else if refresh {
        &["--panel-json", "--refresh"]
    } else {
        &["--panel-json"]
    };
    let _write_guard = if refresh {
        match CC_WRITE_LOCK.try_lock() {
            Ok(g) => Some(g),
            Err(_) => {
                return (
                    true,
                    fallback_switcher_snapshot(ctx),
                    "another account operation is already in progress".into(),
                );
            }
        }
    } else {
        None
    };
    match run_cc_switcher(ctx, args) {
        Ok(v) => {
            let normalized = normalize_switcher_value(v);
            let mut cache = CC_READ_CACHE.lock().unwrap_or_else(|e| e.into_inner());
            if cache.0 == start_gen {
                cache.1 = Some((Instant::now(), normalized.clone()));
            }
            (true, Some(normalized), String::new())
        }
        Err(e) => {
            let fallback = if refresh {
                fallback_switcher_snapshot(ctx)
            } else {
                CC_READ_CACHE
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .1
                    .as_ref()
                    .map(|(_, v)| v.clone())
            };
            (true, fallback, e)
        }
    }
}

/// Dispatch repository account-assignment actions from `POST /api/action`.
pub(super) fn dispatch_repo_action(
    ctx: &Ctx,
    scoped_repo: Option<&Path>,
    action: &str,
    task_id: &str,
    note: &str,
) -> Result<Value, String> {
    let _guard = CC_WRITE_LOCK
        .try_lock()
        .map_err(|_| "refused: another account operation is already running".to_string())?;
    let res = match action {
        "cc_repo_assign" => {
            let account = note.trim();
            if !is_valid_ident(account) {
                return Err("invalid account: expected valid account id".into());
            }
            if scoped_repo.is_none()
                && let Some(app_cfg) = is_open_design_app_config(ctx, task_id)
            {
                let cfg_str = app_cfg.to_string_lossy().into_owned();
                run_cc_switcher(
                    ctx,
                    &[
                        "--panel-action",
                        "repo_assign",
                        "--repo",
                        &cfg_str,
                        "--account",
                        account,
                    ],
                )
            } else {
                let canon = validate_repo_target(ctx, scoped_repo, task_id)?;
                let repo_str = canon.to_string_lossy().into_owned();
                let is_known_aaf_claude = || -> bool {
                    let Ok(cfg) = ctx.load_config(&canon) else {
                        return true;
                    };
                    let lookup =
                        |name: &str| aaf_engine::accounts::binary_on_path_in(&ctx.path_var, name);
                    let discovered: Vec<_> = aaf_engine::accounts::discover(
                        &cfg.global.accounts,
                        &aaf_engine::accounts::DiscoveryEnv {
                            home: ctx.home.clone(),
                            binary_on_path: &lookup,
                        },
                    )
                    .into_iter()
                    .filter(|a| a.provider == aaf_domain::account::ProviderKind::ClaudeCode)
                    .collect();
                    if discovered.is_empty() {
                        return true;
                    }
                    discovered.iter().any(|a| {
                        a.id.eq_ignore_ascii_case(account)
                            || a.name.eq_ignore_ascii_case(account)
                            || aaf_domain::account::default_account_name(&a.config_dir)
                                .eq_ignore_ascii_case(account)
                    })
                };
                let native_assign = || -> Result<Value, String> {
                    if !is_known_aaf_claude() {
                        return Err(format!("invalid account: unknown Claude account '{account}'"));
                    }
                    write_repo_local_account_native(&canon, Some(account))?;
                    Ok(json!({
                        "action": "cc_repo_assign",
                        "repo": repo_str,
                        "account": account,
                        "native": true,
                    }))
                };
                let has_switcher =
                    find_cc_switcher_script(ctx).is_some() && find_node_bin(ctx).is_some();
                if has_switcher {
                    match run_cc_switcher(
                        ctx,
                        &[
                            "--panel-action",
                            "repo_assign",
                            "--repo",
                            &repo_str,
                            "--account",
                            account,
                        ],
                    ) {
                        Ok(v) => Ok(v),
                        Err(e) => {
                            let low = e.to_lowercase();
                            let unregistered = low.contains("không nằm trong danh sách")
                                || low.contains("chưa")
                                || low.contains("not found");
                            if unregistered {
                                run_cc_switcher(
                                    ctx,
                                    &[
                                        "--panel-action",
                                        "repo_add",
                                        "--repo",
                                        &repo_str,
                                        "--account",
                                        account,
                                    ],
                                )
                            } else {
                                Err(e)
                            }
                        }
                    }
                } else {
                    native_assign()
                }
            }
        }
        "cc_repo_remove" => {
            let canon = validate_repo_target(ctx, scoped_repo, task_id)?;
            let repo_str = canon.to_string_lossy().into_owned();
            let native_remove = || -> Result<Value, String> {
                write_repo_local_account_native(&canon, None)?;
                Ok(json!({
                    "action": "cc_repo_remove",
                    "repo": repo_str,
                    "native": true,
                }))
            };
            let has_switcher =
                find_cc_switcher_script(ctx).is_some() && find_node_bin(ctx).is_some();
            if has_switcher {
                match run_cc_switcher(ctx, &["--panel-action", "repo_remove", "--repo", &repo_str])
                {
                    Ok(v) => Ok(v),
                    Err(e) => {
                        let low = e.to_lowercase();
                        let unregistered = low.contains("không nằm trong danh sách")
                            || low.contains("chưa")
                            || low.contains("not found");
                        if unregistered {
                            native_remove()
                        } else {
                            Err(e)
                        }
                    }
                }
            } else {
                native_remove()
            }
        }
        other => Err(format!("invalid action: {other}")),
    };
    if res.is_ok() {
        invalidate_read_cache();
    }
    res
}
