//! `aaf doctor`: checks the installation on this machine and in this repo.
//!
//! Read-only: it creates nothing, so it is safe to run anywhere. Exit 0 when
//! every required check passes, 2 otherwise. Advisory checks (agent binary,
//! sandbox, accounts) are reported but do not fail it: the gate and the hook
//! work without them. Outside a git work tree it is the doctor of the
//! machine (docs/PLAN.md E.5): the checks of a repository (its root, its
//! hook) are reported as not applicable and decide nothing.

mod accounts;
mod dual;
mod head_lock;
mod heavy;
mod hosts;
mod seed;
mod stale_defaults;

use std::fs;
use std::io;
use std::path::{Component, Path};

use aaf_domain::config::MachineConfig;
use aaf_engine::config::{EffectiveConfig, effective_profile};
use aaf_engine::executors::find_on_path;
use aaf_engine::skills::inspect_skills_dir;
use serde::Serialize;
use serde_json::json;

use crate::cmd::init::settings;
use crate::ctx::{Ctx, RepoState};
use crate::output::{CmdOutput, CmdResult};

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
const KEY_FILE: &str = "receipt.key";

#[derive(Debug, clap::Args)]
pub struct DoctorArgs {
    /// Drop old-init `auto_merge = false` / `max_parallel = 1` (outside a repo: every known repo).
    #[arg(long)]
    pub fix: bool,
}

#[derive(Debug, Clone, Serialize)]
struct Check {
    name: &'static str,
    ok: bool,
    /// A failed required check makes the command exit 2.
    required: bool,
    /// False when there was nothing to check here (a repository check
    /// outside a repository); such a check is neither passed nor failed.
    applicable: bool,
    detail: String,
}

/// A check of the repository, asked for outside one.
fn not_applicable(name: &'static str) -> Check {
    Check {
        name,
        ok: true,
        required: false,
        applicable: false,
        detail: "not applicable: not inside a git work tree".to_string(),
    }
}

fn required(name: &'static str, result: Result<String, String>) -> Check {
    check(name, true, result)
}

fn advisory(name: &'static str, result: Result<String, String>) -> Check {
    check(name, false, result)
}

fn check(name: &'static str, required: bool, result: Result<String, String>) -> Check {
    let (ok, detail) = match result {
        Ok(detail) => (true, detail),
        Err(detail) => (false, detail),
    };
    Check {
        name,
        ok,
        required,
        applicable: true,
        detail,
    }
}

fn on_path(ctx: &Ctx, binary: &str) -> Result<String, String> {
    find_on_path(&ctx.path_var, binary)
        .map(|path| path.display().to_string())
        .ok_or_else(|| format!("{binary} is not on PATH"))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The hook entry and the binary it points at, as two results.
fn hook_checks(
    ctx: &Ctx,
    repo_root: &Path,
    tracking: settings::Tracking,
) -> (Result<String, String>, Result<String, String>) {
    let skipped = || Err("not checked: the hook is not registered".to_string());
    let path = repo_root.join(".claude/settings.json");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let missing = format!("{} does not exist; run aaf init", path.display());
            return (Err(missing), skipped());
        }
        Err(err) => {
            return (
                Err(format!("cannot read {}: {err}", path.display())),
                skipped(),
            );
        }
    };
    // The same rule the factory applies before it starts an agent.
    let hook = match settings::covering_hook(&text) {
        Ok(hook) => hook,
        Err(reason) => {
            let reason = format!("{}: {reason}; run aaf init", path.display());
            return (Err(reason), skipped());
        }
    };
    // Claude Code reads the local file on top, and its keys win.
    let local_rel = settings::LOCAL_SETTINGS_REL;
    let local = match fs::read_to_string(repo_root.join(local_rel)) {
        Ok(text) => settings::check_local(&text),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("cannot be read ({err})")),
    };
    if let Err(problem) = local {
        let reason = format!(
            "{} registers the hook, but {local_rel}: {problem}; remove that from the local file",
            path.display()
        );
        return (Err(reason), skipped());
    }
    let mut detail = format!("matcher {:?} in {}", hook.matcher, path.display());
    if let Some(problem) = tracking.problem() {
        // Registered here; `hook_committed` says what that leaves open.
        detail.push_str(&format!("; warning: not committed (the file {problem})"));
    }
    let registered = Ok(detail);

    // Resolved the way `aaf run` resolves it before it starts an agent,
    // against the PATH of this process.
    let binary = settings::sole_program(&hook.command)
        .and_then(|program| settings::resolve_program(&program, &ctx.path_var, None))
        .map(|binary| binary.display().to_string())
        .map_err(|problem| {
            format!("{problem}; run aaf init, and start aaf run with the same PATH as this doctor")
        });
    (registered, binary)
}

/// git 2.34 is the first with `git add --sparse`, which the tree fingerprint
/// uses.
const MIN_GIT: (u32, u32) = (2, 34);

/// `(major, minor)` from the output of `git --version`.
pub(crate) fn git_version(output: &str) -> Option<(u32, u32)> {
    let version = output.trim().strip_prefix("git version ")?;
    let mut parts = version.split(|c: char| !c.is_ascii_digit());
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

fn git_check(ctx: &Ctx) -> Result<String, String> {
    // The git every command of aaf runs, which is not always the first on
    // PATH (see `aaf_fp::program`).
    let path = aaf_fp::program("git")
        .map_err(|err| err.to_string())?
        .display()
        .to_string();
    let output = aaf_fp::git(&ctx.cwd, &["--version"], &[], aaf_fp::GIT_TIMEOUT)
        .map_err(|err| format!("{path}: git --version failed ({err})"))?;
    match git_version(&output) {
        Some(found) if found >= MIN_GIT => Ok(format!("{path} ({output})")),
        Some(_) => Err(format!(
            "{path} is {output}; aaf needs git {}.{} or newer (the gate's fingerprint uses \
             `git add --sparse`)",
            MIN_GIT.0, MIN_GIT.1
        )),
        None => Err(format!(
            "{path}: cannot read a version from {output:?}; aaf needs git {}.{} or newer",
            MIN_GIT.0, MIN_GIT.1
        )),
    }
}

/// Whether the agents of `aaf run` get the hook: they work in worktrees cut
/// from a commit, where the settings file is what HEAD has, not what the
/// working tree has.
fn hook_committed_check(repo_root: &Path, tracking: settings::Tracking) -> Result<String, String> {
    let rel = settings::SETTINGS_REL;
    if !repo_root.join(rel).is_file() {
        return Err(format!("{rel} does not exist; run aaf init and commit it"));
    }
    match tracking.problem() {
        None => Ok(format!("{rel} is committed as it is")),
        Some(problem) => Err(format!(
            "{rel} {problem}: task worktrees of aaf run are cut from a commit and would run \
             agents without this hook registration. Commit the file: \
             git add {rel} && git commit"
        )),
    }
}

/// Writable is proven by creating and removing one empty file; a data dir
/// that does not exist yet is fine, the first gate creates it.
fn data_dir_check(data_dir: &Path) -> Result<String, String> {
    match fs::metadata(data_dir) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(format!(
            "{} does not exist yet; the first gate creates it",
            data_dir.display()
        )),
        Err(err) => Err(format!("{}: {err}", data_dir.display())),
        Ok(meta) if !meta.is_dir() => Err(format!("{} is not a directory", data_dir.display())),
        Ok(_) => {
            let probe = data_dir.join(format!(".doctor-probe-{}", std::process::id()));
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&probe)
                .and_then(|_| fs::remove_file(&probe))
                .map(|()| format!("{} is writable", data_dir.display()))
                .map_err(|err| format!("{} is not writable: {err}", data_dir.display()))
        }
    }
}

/// Looks at the key's metadata only; its bytes are never read here.
fn receipt_key_check(data_dir: &Path) -> Result<String, String> {
    use std::os::unix::fs::PermissionsExt;
    let path = data_dir.join(KEY_FILE);
    match fs::symlink_metadata(&path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            Ok("not created yet; the first gate creates it with mode 0600".to_string())
        }
        Err(err) => Err(format!("{}: {err}", path.display())),
        Ok(meta) if !meta.is_file() => Err(format!("{} is not a regular file", path.display())),
        Ok(meta) => {
            let mode = meta.permissions().mode() & 0o777;
            if mode == 0o600 {
                Ok(format!("{} has mode 0600", path.display()))
            } else {
                Err(format!(
                    "{} has mode {mode:04o}, expected 0600; run chmod 600 on it",
                    path.display()
                ))
            }
        }
    }
}

fn sandbox_check() -> Result<String, String> {
    if cfg!(target_os = "linux") {
        return hosts::linux_sandbox_check();
    }
    if !cfg!(target_os = "macos") {
        return Err("no sandbox on this OS; aaf run needs --unsandboxed".to_string());
    }
    if is_executable(Path::new(SANDBOX_EXEC)) {
        Ok(SANDBOX_EXEC.to_string())
    } else {
        Err(format!(
            "{SANDBOX_EXEC} is missing; aaf run needs --unsandboxed"
        ))
    }
}

fn heavy_ceiling(machine: &MachineConfig) -> u32 {
    if machine.max_heavy_parallel > 0 {
        return machine.max_heavy_parallel;
    }
    // PLAN C.11: automatic = a quarter of the CPUs, at least 1.
    let cpus = std::thread::available_parallelism().map_or(1, usize::from);
    u32::try_from(cpus / 4).unwrap_or(u32::MAX).max(1)
}

fn machine_detail(machine: &MachineConfig) -> String {
    format!(
        "max_heavy_parallel={} (effective {}), max_load_percent={}, max_cpu_percent={}, heavy_wait_secs={}, low_priority={}",
        machine.max_heavy_parallel,
        heavy_ceiling(machine),
        machine.max_load_percent,
        machine.max_cpu_percent,
        machine.heavy_wait_secs,
        machine.low_priority
    )
}

fn skills_sync_check(repo_root: Option<&Path>, profile: Option<&str>) -> Check {
    let (Some(root), Some(profile)) = (repo_root, profile) else {
        return not_applicable("skills_sync");
    };
    let present: Vec<&str> = [".claude/skills", ".agents/skills", ".agent/skills"]
        .into_iter()
        .filter(|rel| root.join(rel).is_dir())
        .collect();
    if present.is_empty() {
        return advisory(
            "skills_sync",
            Ok("no .claude/skills, .agents/skills, or .agent/skills directory; run aaf skills sync".to_string()),
        );
    }
    let (mut ok_parts, mut warnings) = (Vec::new(), Vec::new());
    for rel in present {
        let audit = inspect_skills_dir(&root.join(rel), profile);
        let total = audit.synced.len() + audit.missing.len() + audit.modified.len();
        if audit.missing.is_empty() && audit.modified.is_empty() {
            ok_parts.push(format!(
                "{rel} ({profile}): {}/{total} synced",
                audit.synced.len()
            ));
            continue;
        }
        let mut detail = format!("{rel} ({profile}):");
        if !audit.missing.is_empty() {
            detail.push_str(&format!(" missing=[{}]", audit.missing.join(", ")));
        }
        if !audit.modified.is_empty() {
            detail.push_str(&format!(" modified=[{}]", audit.modified.join(", ")));
        }
        warnings.push(detail);
    }
    let res = if warnings.is_empty() {
        Ok(ok_parts.join("; "))
    } else {
        Err(format!("{}; run aaf skills sync", warnings.join("; ")))
    };
    advisory("skills_sync", res)
}

fn proof_readiness_check(
    repo_root: Option<&Path>,
    cfg: &EffectiveConfig,
    profile: Option<&str>,
) -> Check {
    let (Some(root), Some(profile)) = (repo_root, profile) else {
        return not_applicable("proof_readiness");
    };
    let mobile = matches!(
        profile,
        "android" | "ios" | "flutter" | "react-native" | "expo" | "unity"
    );
    if !cfg.repo.proof.required && !mobile {
        return advisory("proof_readiness", Ok(format!("not required for {profile}")));
    }
    let mut issues = Vec::new();
    let denylist = cfg.repo.proof.denylist_file.trim();
    let rel = Path::new(denylist);
    let valid_rel = !denylist.is_empty()
        && !rel.is_absolute()
        && rel
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if !valid_rel {
        issues.push(format!("invalid proof.denylist_file {denylist:?}"));
    } else if !root.join(rel).is_file() {
        issues.push(format!("missing {denylist}"));
    }
    if cfg.repo.proof.allowed_tools.is_empty() {
        issues.push("proof.allowed_tools is empty (fail-closed)".to_string());
    }
    if let Err(err) = hosts::check_mcp_mobile(root) {
        issues.push(err);
    }
    let res = if issues.is_empty() {
        Ok(format!(
            "profile={profile}: {denylist} and mobile-mcp ready"
        ))
    } else {
        Err(format!("profile={profile}: {}", issues.join("; ")))
    };
    advisory("proof_readiness", res)
}

fn github_board_check(repo_root: Option<&Path>, cfg: &EffectiveConfig) -> Check {
    if repo_root.is_none() {
        return not_applicable("github_board");
    }
    advisory("github_board", hosts::github_board_status(cfg))
}

#[cfg(target_os = "macos")]
fn keychain_sandbox_exposure_check(policy: aaf_domain::keychain::KeychainPolicy) -> Check {
    use aaf_domain::keychain::KeychainPolicy;
    let res = match policy {
        KeychainPolicy::Always => Ok(
            "macOS Keychain read and SecurityServer IPC are denied in Seatbelt for all runs"
                .to_string(),
        ),
        KeychainPolicy::Auto => Err(
            "Keychain (~/Library/Keychains) is closed automatically for API-key and non-Claude \
             runs, while remaining open for Claude OAuth (SECURITY.md §10 #3; set [safety] \
             deny_macos_keychain = \"always\" with API key auth to close for all runs)"
                .to_string(),
        ),
        KeychainPolicy::Never => Err(
            "Seatbelt keeps ~/Library/Keychains readable because [safety] deny_macos_keychain = \
             \"never\"; sandboxed agents can read user Keychain items (SECURITY.md §10 #3)"
                .to_string(),
        ),
    };
    advisory("keychain_sandbox_exposure", res)
}

pub fn run(args: &DoctorArgs, ctx: &Ctx) -> CmdResult {
    let repo_state = ctx.repo_state();
    let repo_root = match &repo_state {
        RepoState::Root(root) => Ok(root.clone()),
        _ => Err(()),
    };
    // Outside a repo the global tier can still be checked.
    let config_root = repo_root.as_deref().unwrap_or(&ctx.cwd);
    let config = ctx.load_config(config_root);
    // The limits in effect when the config does not load are the defaults.
    let effective = config.clone().unwrap_or_else(|_| EffectiveConfig {
        global: Default::default(),
        repo: Default::default(),
        sources: Vec::new(),
        warnings: Vec::new(),
    });
    let root_opt = repo_root.as_deref().ok();
    let (git_res, tracking_opt, daemons, git_hooks_res) = std::thread::scope(|s| {
        let t_git = s.spawn(|| git_check(ctx));
        let t_tracking = s.spawn(|| root_opt.map(settings::Tracking::of));
        let t_daemons = s.spawn(aaf_engine::throttle::daemons::list);
        let t_hooks = s.spawn(|| heavy::git_hooks_check(root_opt));
        (
            t_git
                .join()
                .unwrap_or_else(|_| Err("git check thread panicked".to_string())),
            t_tracking.join().unwrap_or(None),
            t_daemons.join().unwrap_or_default(),
            t_hooks
                .join()
                .unwrap_or_else(|_| Err("git hooks check thread panicked".to_string())),
        )
    });
    let repo_checks = match &repo_state {
        RepoState::Root(root) => {
            let tracking = tracking_opt.unwrap_or(settings::Tracking::Unknown);
            let (hook_registered, hook_binary) = hook_checks(ctx, root, tracking);
            [
                required("repo_root", Ok(root.display().to_string())),
                required("hook_registered", hook_registered),
                required("hook_binary", hook_binary),
                advisory("hook_committed", hook_committed_check(root, tracking)),
            ]
        }
        // A repository is here and git fails on it: `aaf gate` and `aaf run`
        // fail the same way, so this is a failure, not "nothing to check".
        RepoState::GitFailed(why) => {
            let unchecked = || Err("not checked: git could not read the repository".to_string());
            [
                required("repo_root", Err(why.clone())),
                advisory("hook_registered", unchecked()),
                advisory("hook_binary", unchecked()),
                advisory("hook_committed", unchecked()),
            ]
        }
        RepoState::NotARepo => [
            "repo_root",
            "hook_registered",
            "hook_binary",
            "hook_committed",
        ]
        .map(not_applicable),
    };
    let [
        repo_root_check,
        hook_registered,
        hook_binary,
        hook_committed,
    ] = repo_checks;
    let profile = root_opt.map(|root| effective_profile(&effective.repo, root));
    let (accounts_check, accounts_fields) = accounts::report(ctx, &effective, root_opt);
    let config_detail = config
        .as_ref()
        .map(|cfg| {
            let mut detail = if cfg.sources.is_empty() {
                "no config file found; defaults in effect".to_string()
            } else {
                format!("loaded {}", cfg.sources.join(", "))
            };
            for warning in &cfg.warnings {
                detail.push_str("; warning: ");
                detail.push_str(warning);
            }
            detail
        })
        .map_err(|err| err.message.clone());
    let mut checks = vec![
        advisory("version", Ok(env!("AAF_BUILD_VERSION").to_string())),
        required("git", git_res),
        repo_root_check,
        required("config", config_detail),
        hook_registered,
        hook_binary,
        hook_committed,
        required("data_dir", data_dir_check(&ctx.data_dir)),
        required("receipt_key", receipt_key_check(&ctx.data_dir)),
        advisory("claude", on_path(ctx, "claude")),
        advisory("sandbox", sandbox_check()),
        advisory("gate_confinement", hosts::gate_confinement_check()),
        advisory("accounts", accounts_check),
        advisory("machine", Ok(machine_detail(&effective.global.machine))),
        advisory(
            "head_lock_agent_files",
            head_lock::agent_files_check(&effective.repo.gate.head_lock_globs),
        ),
        advisory(
            "head_lock_runner_configs",
            head_lock::runner_config_check(&effective.repo.gate.head_lock_globs),
        ),
        advisory("gradle_daemon", heavy::live_daemons(&daemons)),
        advisory(
            "gradle_daemon_config",
            heavy::daemon_config(root_opt, &effective.repo.commands),
        ),
        advisory("git_hooks_heavy", git_hooks_res),
        skills_sync_check(root_opt, profile.as_deref()),
        proof_readiness_check(root_opt, &effective, profile.as_deref()),
        github_board_check(root_opt, &effective),
        root_opt.map_or_else(
            || not_applicable("executor_token_budget"),
            |_| {
                advisory(
                    "executor_token_budget",
                    hosts::executor_token_budget(ctx, &effective.repo),
                )
            },
        ),
        advisory(
            "telegram",
            hosts::telegram_status(&effective.global.telegram),
        ),
        advisory("executors", hosts::executors_summary(&ctx.path_var)),
        advisory(
            "dual_frontier",
            dual::dual_frontier_check(ctx, &effective.repo),
        ),
        root_opt.map_or_else(
            || not_applicable("mcp_hygiene"),
            |r| advisory("mcp_hygiene", hosts::mcp_hygiene(r)),
        ),
    ];
    checks.extend(hosts::repo_hook_checks(ctx, root_opt, &effective.repo));
    checks.extend(stale_defaults::checks(args.fix, ctx, &repo_state));
    checks.push(seed::worktree_cow_volume_check(ctx, root_opt));
    checks.push(seed::seed_cache_dry_run_check(
        ctx,
        root_opt,
        &effective.repo.worktree,
    ));
    #[cfg(target_os = "macos")]
    checks.push(keychain_sandbox_exposure_check(
        effective.global.safety.deny_macos_keychain,
    ));

    let failed = checks.iter().any(|check| check.required && !check.ok);
    let text = checks
        .iter()
        .map(|check| {
            let mark = match (check.applicable, check.ok, check.required) {
                (false, ..) => "n/a ",
                (true, true, _) => "ok  ",
                (true, false, true) => "FAIL",
                (true, false, false) => "warn",
            };
            format!("{mark} {}: {}", check.name, check.detail)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let out = if failed {
        CmdOutput::policy("failed", text)
    } else {
        CmdOutput::ok("ok", text)
    };
    Ok(out
        .field("checks", json!(checks))
        .field("config_warnings", json!(effective.warnings))
        .field("accounts", accounts_fields)
        .field(
            "machine",
            json!({
                "max_heavy_parallel": effective.global.machine.max_heavy_parallel,
                "effective_heavy_parallel": heavy_ceiling(&effective.global.machine),
                "max_load_percent": effective.global.machine.max_load_percent,
                "max_cpu_percent": effective.global.machine.max_cpu_percent,
                "heavy_wait_secs": effective.global.machine.heavy_wait_secs,
                "low_priority": effective.global.machine.low_priority,
            }),
        ))
}
