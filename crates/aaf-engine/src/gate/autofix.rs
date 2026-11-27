//! Pre-gate deterministic formatter (`commands.autofix`) with fail-closed
//! scope quarantine.
//!
//! Loaded strictly from immutable `git show <base_sha>:.autofactory/config.toml`
//! (never from the agent worktree), skipped whenever `head_lock` is not `Clear`
//! (V2-SEC-03) or on `docs_fast_path`, executed confined + throttled, and
//! quarantined so only non-locked paths already modified by the agent may keep
//! formatting changes.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::time::Duration;

use aaf_domain::config::{AGENT_CONFIG_GLOBS, MachineConfig, RepoConfig};
use aaf_domain::engine_contracts::REPO_CONFIG_REL;
use aaf_domain::gate::{ChangeKind, GateMode, LockStatus};
use aaf_domain::ports::Clock;

use super::changes::Change;
use super::hidden::When;
use super::runner::{CommandEnv, DiffEnvFiles, HeavyContext};
use super::{CommandTimeouts, GIT_TIMEOUT, GateError, HiddenPolicy};
use crate::throttle::{Progress, SystemProbe, Throttle};

/// Hard ceiling (seconds) for a single pre-gate `commands.autofix` invocation.
pub const MAX_AUTOFIX_TIMEOUT_SECS: u64 = 300;
/// Commit message for quarantined pre-gate formatting edits.
pub const AUTOFIX_COMMIT_MSG: &str = "aaf: autofix";

const AUTOFIX_ENV_REMOVE: &[&str] = &["AAF_DATA_DIR", "AAF_RUN_ID", "AAF_TASK_ID"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutofixOutcome {
    Skipped {
        reason: String,
    },
    Clean,
    Formatted {
        paths: Vec<String>,
    },
    Failed {
        exit_code: Option<i32>,
        timed_out: bool,
        reason: String,
    },
}

/// Steps 1-4 run before the formatter is spawned: an error there leaves the
/// worktree untouched, so autofix is skipped rather than blocking the task.
macro_rules! before_run {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(err) => {
                return Ok(AutofixOutcome::Skipped {
                    reason: format!("autofix unavailable: {err}"),
                });
            }
        }
    };
}

#[allow(clippy::too_many_arguments)]
pub fn run_autofix(
    data_dir: &Path,
    tree: &Path,
    cfg: &RepoConfig,
    policy: HiddenPolicy,
    clock: &dyn Clock,
    machine: &MachineConfig,
    progress: &dyn Fn(&Progress<'_>),
) -> Result<AutofixOutcome, GateError> {
    let repo_root = before_run!(fs::canonicalize(tree));
    let base = before_run!(super::base::resolve(
        &repo_root,
        &cfg.gate.base_ref,
        GIT_TIMEOUT
    ));
    let Some(base_sha) = base.sha.as_deref() else {
        return Ok(AutofixOutcome::Skipped {
            reason: "no base_sha resolved".to_string(),
        });
    };
    // Step 1: Load `commands.autofix` strictly from immutable `base_sha`.
    let Some((autofix_cmd, light_commands)) =
        before_run!(load_base_autofix_cmd(&repo_root, base_sha))
    else {
        return Ok(AutofixOutcome::Skipped {
            reason: "commands.autofix is empty at base_sha".to_string(),
        });
    };

    // Step 2: Capture `pre_head`, inspect hidden files, check changes and docs fast path.
    let pre_head = before_run!(git_ok(&repo_root, &["rev-parse", "HEAD"]))
        .trim()
        .to_string();
    let hidden_before = before_run!(super::hidden::inspect(
        &repo_root,
        policy,
        When::BeforeCommands,
        false
    ));
    if !hidden_before.blocks.is_empty() {
        return Ok(AutofixOutcome::Skipped {
            reason: "hidden files flagged before autofix".to_string(),
        });
    }
    let set = before_run!(super::changes::collect(
        &repo_root,
        &base.diff_target,
        GIT_TIMEOUT
    ));
    if set.changes.is_empty() {
        return Ok(AutofixOutcome::Skipped {
            reason: "no changes against base".to_string(),
        });
    }
    if cfg.gate.docs_fast_path
        && before_run!(super::docs_fast::is_docs_only_in_repo(
            &repo_root,
            &set.changes,
            &cfg.gate.head_lock_globs,
            &cfg.gate.docs_fast_path_exclude_globs,
            GIT_TIMEOUT,
        ))
    {
        return Ok(AutofixOutcome::Skipped {
            reason: "docs_fast_path".to_string(),
        });
    }

    // Step 3: V2-SEC-03 — refuse to run when any head-locked file was touched.
    let hl_before = super::head_lock::evaluate(data_dir, &repo_root, cfg, &base.diff_target);
    if hl_before.status != LockStatus::Clear {
        return Ok(AutofixOutcome::Skipped {
            reason: "head_lock is not Clear (V2-SEC-03)".to_string(),
        });
    }

    // Step 4: Scope lock to unlocked, non-deleted, non-agent-config paths modified by agent.
    let is_locked = super::head_lock_path_matcher(&cfg.gate.head_lock_globs);
    let agent_globs: Vec<String> = AGENT_CONFIG_GLOBS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    let is_agent_cfg = super::head_lock_path_matcher(&agent_globs);
    let allowed_changes: Vec<Change> = set
        .changes
        .iter()
        .filter(|ch| {
            ch.kind != ChangeKind::Deleted
                && !is_locked(&ch.path)
                && !is_agent_cfg(&ch.path)
                && !is_protected_config_dir(&ch.path)
        })
        .cloned()
        .collect();
    let allowed_paths: BTreeSet<String> = allowed_changes.iter().map(|c| c.path.clone()).collect();
    if allowed_paths.is_empty() {
        return Ok(AutofixOutcome::Skipped {
            reason: "no unlocked editable paths in diff".to_string(),
        });
    }
    let diff_env = before_run!(DiffEnvFiles::write(
        GateMode::Scoped,
        Some(base_sha),
        &allowed_changes
    ));
    let env_owned = diff_env.env_pairs();
    let env_refs: Vec<(&str, &str)> = env_owned.iter().map(|(k, v)| (*k, v.as_str())).collect();

    // Step 5: Execute confined + throttled (heavy slot acquired & released inside this call).
    let timeout_secs = cfg
        .gate
        .command_timeout_secs
        .clamp(1, MAX_AUTOFIX_TIMEOUT_SECS);
    let timeouts = CommandTimeouts {
        wall: Duration::from_secs(timeout_secs),
        idle: (cfg.gate.idle_timeout_secs > 0)
            .then(|| Duration::from_secs(cfg.gate.idle_timeout_secs)),
    };
    let run_res = if super::runner::is_heavy_with_light("autofix", &autofix_cmd, &light_commands) {
        let stats_id = super::stats_repo_id(&repo_root);
        let probe = SystemProbe::detect();
        let throttle = Throttle::system(machine, &probe, progress);
        let heavy = HeavyContext {
            data_dir,
            stats_repo_id: &stats_id,
            clock,
            throttle: &throttle,
        };
        let mut notes = Vec::new();
        let env = CommandEnv {
            extra: &env_refs,
            remove: AUTOFIX_ENV_REMOVE,
        };
        super::runner::run_heavy(
            &repo_root,
            "autofix",
            &autofix_cmd,
            timeouts,
            &heavy,
            env,
            &mut notes,
        )
    } else {
        super::runner::run_command_confined(
            &repo_root,
            "autofix",
            &autofix_cmd,
            timeouts,
            &env_refs,
            AUTOFIX_ENV_REMOVE,
            Some(data_dir),
        )
    };
    let (cmd_res, _) = match run_res {
        Ok(r) => r,
        Err(err) => {
            let _ = reset_clean(&repo_root, &pre_head);
            return Err(err);
        }
    };

    // Step 6: Fail-closed post-run quarantine.
    let hidden_after = match super::hidden::inspect(&repo_root, policy, When::AfterCommands, false)
    {
        Ok(h) => h,
        Err(err) => {
            let _ = reset_clean(&repo_root, &pre_head);
            return Err(err);
        }
    };
    if !cmd_res.passed() || !hidden_after.blocks.is_empty() {
        reset_clean(&repo_root, &pre_head)?;
        let reason = if !hidden_after.blocks.is_empty() {
            "hidden files created during autofix".to_string()
        } else if cmd_res.timed_out {
            "autofix timed out".to_string()
        } else {
            format!("autofix exited with {:?}", cmd_res.exit_code)
        };
        return Ok(AutofixOutcome::Failed {
            exit_code: cmd_res.exit_code,
            timed_out: cmd_res.timed_out,
            reason,
        });
    }

    plain_pseudo_refs(&repo_root)?;
    // Undo any commits or index staging performed by the formatter while keeping worktree edits.
    if let Err(err) =
        git_ok(&repo_root, &["reset", "--mixed", "--quiet", &pre_head]).and_then(|_| {
            quarantine_worktree(
                &repo_root,
                &pre_head,
                &allowed_paths,
                &is_locked,
                &is_agent_cfg,
            )
        })
    {
        let _ = reset_clean(&repo_root, &pre_head);
        return Err(err);
    }

    let hl_after = super::head_lock::evaluate(data_dir, &repo_root, cfg, &base.diff_target);
    if hl_after.status != LockStatus::Clear {
        reset_clean(&repo_root, &pre_head)?;
        return Ok(AutofixOutcome::Failed {
            exit_code: Some(0),
            timed_out: false,
            reason: "head_lock drifted after autofix quarantine".to_string(),
        });
    }

    let remaining = match dirty_paths_z(&repo_root) {
        Ok(r) => r,
        Err(err) => {
            let _ = reset_clean(&repo_root, &pre_head);
            return Err(err);
        }
    };
    let mut formatted: Vec<String> = Vec::new();
    for entry in remaining {
        if entry.untracked || !allowed_paths.contains(&entry.path) {
            reset_clean(&repo_root, &pre_head)?;
            return Ok(AutofixOutcome::Failed {
                exit_code: Some(0),
                timed_out: false,
                reason: format!("disallowed path survived quarantine: {}", entry.path),
            });
        }
        formatted.push(entry.path);
    }
    if formatted.is_empty() {
        Ok(AutofixOutcome::Clean)
    } else {
        Ok(AutofixOutcome::Formatted { paths: formatted })
    }
}

fn load_base_autofix_cmd(
    repo_root: &Path,
    base_sha: &str,
) -> Result<Option<(String, Vec<String>)>, GateError> {
    let sha = base_sha.trim();
    if sha.is_empty()
        || sha.starts_with('-')
        || sha.contains(':')
        || sha.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(GateError::Config(format!("invalid base_sha {base_sha:?}")));
    }
    let listed = aaf_fp::git(
        repo_root,
        &["ls-tree", sha, "--", REPO_CONFIG_REL],
        &[],
        GIT_TIMEOUT,
    )?;
    let line = listed.trim();
    if line.is_empty() {
        return Ok(None);
    }
    let mode = line.split_whitespace().next().unwrap_or("");
    if !matches!(mode, "100644" | "100755") {
        return Err(GateError::Config(format!(
            "{REPO_CONFIG_REL} at {sha} must be a regular file blob (got {mode:?})"
        )));
    }
    let spec = format!("{sha}:{REPO_CONFIG_REL}");
    let shown = aaf_fp::git(repo_root, &["show", &spec], &[], GIT_TIMEOUT)?;
    let base_cfg: RepoConfig = aaf_domain::builtin_defaults::parse_repo(&shown)
        .map_err(|err| GateError::Config(format!("invalid TOML in {spec}: {}", err.message())))?;
    let cmd = base_cfg.commands.autofix.trim();
    if cmd.is_empty() {
        Ok(None)
    } else {
        Ok(Some((cmd.to_string(), base_cfg.commands.light_commands)))
    }
}

fn is_protected_config_dir(path: &str) -> bool {
    path.split('/').any(|seg| {
        let lower = seg.to_ascii_lowercase();
        lower == ".claude" || lower == ".autofactory"
    })
}

fn git_ok(repo_root: &Path, args: &[&str]) -> Result<String, GateError> {
    aaf_fp::git(repo_root, args, &[], GIT_TIMEOUT).map_err(GateError::from)
}

/// SEC-125: a planted `ORIG_HEAD` symref would let the reset move another ref.
fn plain_pseudo_refs(repo_root: &Path) -> Result<(), GateError> {
    crate::git::pseudo_ref_guard::ensure_plain_pseudo_refs(repo_root)
        .map_err(|e| GateError::Git(e.to_string()))
}

fn reset_clean(repo_root: &Path, pre_head: &str) -> Result<(), GateError> {
    plain_pseudo_refs(repo_root)?;
    let _ = git_ok(repo_root, &["reset", "--hard", "--quiet", pre_head])?;
    let _ = git_ok(repo_root, &["clean", "-ffdq"])?;
    Ok(())
}

struct StatusEntry {
    untracked: bool,
    path: String,
    orig_path: Option<String>,
}

fn dirty_paths_z(repo_root: &Path) -> Result<Vec<StatusEntry>, GateError> {
    let raw = super::proc::git_bytes(
        repo_root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        GIT_TIMEOUT,
    )?;
    let text = String::from_utf8_lossy(&raw);
    let mut parts = text.split('\0');
    let mut entries = Vec::new();
    while let Some(rec) = parts.next() {
        if rec.len() < 4 {
            continue;
        }
        let code = &rec[..2];
        let path = rec[3..].to_string();
        let orig_path = if code.contains('R') || code.contains('C') {
            parts.next().map(str::to_string)
        } else {
            None
        };
        entries.push(StatusEntry {
            untracked: code == "??",
            path,
            orig_path,
        });
    }
    Ok(entries)
}

fn remove_untracked(full: &Path) -> Result<(), GateError> {
    let Ok(meta) = fs::symlink_metadata(full) else {
        return Ok(());
    };
    if meta.file_type().is_dir() && !meta.file_type().is_symlink() {
        fs::remove_dir_all(full)?;
    } else {
        fs::remove_file(full)?;
    }
    Ok(())
}

fn restore_path(repo_root: &Path, pre_head: &str, rel: &str) -> Result<(), GateError> {
    let full = repo_root.join(rel);
    if fs::symlink_metadata(&full).is_ok_and(|m| m.file_type().is_symlink()) {
        let _ = fs::remove_file(&full);
    }
    if git_ok(repo_root, &["checkout", pre_head, "--", rel]).is_err() {
        remove_untracked(&full)?;
    }
    Ok(())
}

fn quarantine_worktree(
    repo_root: &Path,
    pre_head: &str,
    allowed_paths: &BTreeSet<String>,
    is_locked: &dyn Fn(&str) -> bool,
    is_agent_cfg: &dyn Fn(&str) -> bool,
) -> Result<(), GateError> {
    for entry in dirty_paths_z(repo_root)? {
        if entry.untracked {
            remove_untracked(&repo_root.join(&entry.path))?;
            continue;
        }
        let full = repo_root.join(&entry.path);
        let meta = fs::symlink_metadata(&full).ok();
        let became_symlink_or_deleted = meta
            .as_ref()
            .is_none_or(|m| m.file_type().is_symlink() || !m.file_type().is_file());
        let allowed = !became_symlink_or_deleted
            && allowed_paths.contains(&entry.path)
            && !is_locked(&entry.path)
            && !is_agent_cfg(&entry.path)
            && !is_protected_config_dir(&entry.path);
        if !allowed {
            restore_path(repo_root, pre_head, &entry.path)?;
        }
        if let Some(orig) = entry.orig_path {
            restore_path(repo_root, pre_head, &orig)?;
        }
    }
    let _ = git_ok(repo_root, &["clean", "-ffdq"])?;
    Ok(())
}
