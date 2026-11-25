//! Accounts panel backend: native AAF account pool + optional CC Switcher
//! live 5h/7d quota, profile switching, and Repo <-> Account (`.claude/settings.local.json`)
//! assignment with safe native fallback.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use aaf_domain::account::AccountStatus;
use aaf_domain::ports::Clock;
use aaf_engine::accounts::{DiscoveryEnv, Router, binary_on_path_in, discover};
use aaf_engine::db::Db;
use serde_json::{Value, json};

use super::extras::{
    canonical_under_allowed_roots, known_repos, read_repo_local_account,
    write_repo_local_account_native,
};
use crate::ctx::{Ctx, SystemClock};

const CC_READ_TIMEOUT: Duration = Duration::from_secs(45);
const CC_REFRESH_TIMEOUT: Duration = Duration::from_secs(60);
const CC_CACHE_TTL: Duration = Duration::from_secs(30);
const MAX_CC_STDOUT_BYTES: usize = 512 * 1024;
const MAX_CC_STDERR_BYTES: usize = 4 * 1024;
static CC_WRITE_LOCK: Mutex<()> = Mutex::new(());
static CC_READ_CACHE: Mutex<Option<(Instant, Value)>> = Mutex::new(None);

fn is_valid_ident(s: &str) -> bool {
    let bytes = s.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 48
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

fn find_cc_switcher_script(ctx: &Ctx) -> Option<PathBuf> {
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
    *guard = None;
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

fn native_repo_assignments(db: Option<&Db>, scoped_repo: Option<&Path>) -> Vec<Value> {
    let canon_scope = scoped_repo.map(|s| fs::canonicalize(s).unwrap_or_else(|_| s.to_path_buf()));
    known_repos(db, scoped_repo)
        .into_iter()
        .filter_map(|v| {
            let path_str = v["repo_root"].as_str()?.to_string();
            let p = Path::new(&path_str);
            if let Some(ref sc) = canon_scope {
                let cp = fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
                if cp != *sc {
                    return None;
                }
            }
            let name = v["name"].as_str().unwrap_or(&path_str).to_string();
            let account = read_repo_local_account(p);
            Some(json!({
                "name": name,
                "path": path_str,
                "account": account.clone().unwrap_or_default(),
                "pinnedInFile": account.is_some(),
            }))
        })
        .collect()
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
        .as_ref()
        .map(|(_, v)| v.clone());
    if cached.is_some() {
        return cached;
    }
    run_cc_switcher(ctx, &["--panel-json", "--no-sync"])
        .ok()
        .map(normalize_switcher_value)
}

fn fetch_switcher_state(
    ctx: &Ctx,
    read_only: bool,
    refresh: bool,
) -> (bool, Option<Value>, String) {
    if find_cc_switcher_script(ctx).is_none() {
        return (false, None, String::new());
    }
    if !refresh {
        let guard = CC_READ_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, ref cached)) = *guard
            && at.elapsed() < CC_CACHE_TTL
        {
            return (true, Some(cached.clone()), String::new());
        }
    }
    let args: &[&str] = if read_only {
        &["--panel-json", "--no-sync"]
    } else if refresh {
        &["--panel-json", "--refresh"]
    } else {
        // Match CC Switcher's smart TTL sync (instant when fresh, auto-syncs when stale).
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
            *cache = Some((Instant::now(), normalized.clone()));
            (true, Some(normalized), String::new())
        }
        Err(e) => {
            let fallback = if refresh {
                fallback_switcher_snapshot(ctx)
            } else {
                CC_READ_CACHE
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .map(|(_, v)| v.clone())
            };
            (true, fallback, e)
        }
    }
}

/// Build combined accounts panel response for `GET /api/accounts`.
pub(super) fn read_accounts_panel(
    db: Option<&Db>,
    ctx: &Ctx,
    config_base: &Path,
    scoped_repo: Option<&Path>,
    read_only: bool,
    refresh: bool,
) -> Result<Value, String> {
    if refresh && read_only {
        return Err("refused: live quota refresh is disabled in read-only mode".into());
    }
    let cfg = ctx.load_config(config_base).map_err(|e| e.message)?;
    let lookup = |name: &str| binary_on_path_in(&ctx.path_var, name);
    let discovered = discover(
        &cfg.global.accounts,
        &DiscoveryEnv {
            home: ctx.home.clone(),
            binary_on_path: &lookup,
        },
    );
    let clock = SystemClock;
    let aaf_accounts: Vec<Value> = if let Some(db) = db {
        let router = Router::new(
            db,
            db,
            &clock,
            ctx.data_dir.join("leases"),
            cfg.global.accounts.max_concurrent_per_account,
        );
        router
            .status(clock.now())
            .map(|list| {
                list.into_iter()
                    .filter(|a| discovered.iter().any(|p| p.id == a.id))
                    .map(|a| {
                        let (st, until) = match a.status {
                            AccountStatus::Available => ("available", None),
                            AccountStatus::Cooldown { until } => ("cooldown", Some(until)),
                            AccountStatus::Leased => ("leased", None),
                        };
                        json!({
                            "id": a.id,
                            "name": a.name,
                            "provider": a.provider,
                            "state": st,
                            "cooldown_until": until,
                            "tokens_5h": a.tokens_5h,
                            "tokens_7d": a.tokens_7d,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        discovered
            .iter()
            .map(|p| {
                json!({
                    "id": p.id,
                    "name": p.name,
                    "provider": p.provider,
                    "state": "available",
                    "cooldown_until": Value::Null,
                    "tokens_5h": 0,
                    "tokens_7d": 0,
                })
            })
            .collect()
    };

    let (sw_installed, sw_val, sw_err) = fetch_switcher_state(ctx, read_only, refresh);
    Ok(json!({
        "aaf_accounts": aaf_accounts,
        "repo_assignments": native_repo_assignments(db, scoped_repo),
        "switcher_installed": sw_installed,
        "switcher": sw_val,
        "switcher_error": sw_err,
    }))
}

/// Dispatch account-switcher & repo-assignment actions from `POST /api/action`.
pub(super) fn dispatch_cc_action(
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
        "cc_switch" => {
            let target = task_id.trim();
            if !is_valid_ident(target) {
                return Err("invalid target: expected valid account id".into());
            }
            let mut args = vec!["--panel-action", "switch", "--target", target];
            if note.trim().eq_ignore_ascii_case("force") {
                args.push("--force");
            }
            run_cc_switcher(ctx, &args)
        }
        "cc_rotate" => run_cc_switcher(ctx, &["--panel-action", "rotate"]),
        "cc_unfreeze" => run_cc_switcher(ctx, &["--panel-action", "unfreeze"]),
        "cc_balance" => {
            if scoped_repo.is_some() {
                return Err("refused: cc_balance is disabled in single-repo scope".into());
            }
            let mode = match note.trim() {
                "" | "even" => "even",
                "swap" => "swap",
                _ => return Err("invalid balance mode: expected 'even' or 'swap'".into()),
            };
            run_cc_switcher(ctx, &["--panel-action", "balance", "--mode", mode])
        }
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
                if find_cc_switcher_script(ctx).is_some() {
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
                        Err(e)
                            if e.to_ascii_lowercase().contains("chưa")
                                || e.to_ascii_lowercase().contains("not found") =>
                        {
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
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    write_repo_local_account_native(&canon, Some(account))?;
                    Ok(json!({
                        "action": "cc_repo_assign",
                        "repo": repo_str,
                        "account": account,
                        "native": true,
                    }))
                }
            }
        }
        "cc_repo_remove" => {
            let canon = validate_repo_target(ctx, scoped_repo, task_id)?;
            let repo_str = canon.to_string_lossy().into_owned();
            if find_cc_switcher_script(ctx).is_some() {
                run_cc_switcher(ctx, &["--panel-action", "repo_remove", "--repo", &repo_str])
            } else {
                write_repo_local_account_native(&canon, None)?;
                Ok(json!({
                    "action": "cc_repo_remove",
                    "repo": repo_str,
                    "native": true,
                }))
            }
        }
        other => Err(format!("invalid action: {other}")),
    };
    if res.is_ok() {
        invalidate_read_cache();
    }
    res
}
