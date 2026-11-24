//! `aaf init`: detect the stack, write `.autofactory/config.toml`, register
//! `aaf-hook` in `.claude/settings.json`, ignore the local config file.
//!
//! Safe to run again: an existing config is kept unless `--force`, the hook
//! entry is merged and the ignore line is added once.

pub mod settings;
#[cfg(test)]
mod tests;

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use aaf_engine::config::{
    LOCAL_CONFIG_REL, REPO_CONFIG_REL, detect_stack, effective_profile, render_repo_config,
};
use aaf_engine::executors::find_on_path;
use aaf_engine::hook_settings::multi_host::{self, HookHostTarget};
use serde_json::json;

use crate::cmd::skills::{SkillHost, merge_outcomes};
use crate::ctx::{self, Ctx};
use crate::output::{CmdError, CmdOutput, CmdResult};

use settings::{HOOK_BINARY, SETTINGS_REL, Tracking};

#[derive(Debug, clap::Args)]
pub struct InitArgs {
    /// Overwrite an existing `.autofactory/config.toml`.
    #[arg(long)]
    pub force: bool,
    /// Also sync bundled domain skills into the host skills directory without overwriting existing ones.
    #[arg(long)]
    pub with_skills: bool,
    /// Target host directory layout for `--with-skills` (`claude`, `agents`, or `all`).
    #[arg(long, value_enum, default_value_t = SkillHost::Claude)]
    pub host: SkillHost,
    /// Agent hook hosts to register (`claude`, `codex`, `gemini`, or `all`).
    #[arg(long, default_value = "claude")]
    pub hook_host: String,
}

pub fn run(args: &InitArgs, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = super::board::store::refuse_inside_agent_run(ctx, "aaf init") {
        return Ok(refused);
    }
    let mut out = run_init(args, ctx)?;
    if args.with_skills && out.exit == crate::output::Exit::Ok {
        let root = super::board::store::repo_root(ctx).unwrap_or_else(|_| ctx.cwd.clone());
        let profile = if root.join(REPO_CONFIG_REL).exists() {
            let cfg = ctx.load_config(&root)?;
            effective_profile(&cfg.repo, &root)
        } else {
            detect_stack(&root).profile
        };
        let dests = args.host.resolve_dirs(&root);
        let mut raw_outcomes = Vec::with_capacity(dests.len());
        let mut targets = Vec::with_capacity(dests.len());
        for dest in &dests {
            let synced =
                aaf_engine::skills::sync_skills_for_profile(dest, Some(&profile), None, false)
                    .map_err(|e| CmdError::new(e.to_string()))?;
            targets.push(json!({
                "dir": dest.to_string_lossy().into_owned(),
                "written": synced.written,
                "skipped_existing": synced.skipped_existing,
                "unchanged": synced.unchanged,
                "skipped_modified": synced.skipped_modified,
            }));
            raw_outcomes.push(synced);
        }
        let merged = merge_outcomes(&raw_outcomes);
        let dirs: Vec<String> = dests
            .iter()
            .map(|d| d.to_string_lossy().into_owned())
            .collect();
        out.text.push_str(&format!(
            "\nskills: profile={profile} host={} written={} unchanged={} skipped_modified={} ({})",
            args.host.as_str(),
            merged.written.len(),
            merged.unchanged.len(),
            merged.skipped_modified.len(),
            dirs.join(", "),
        ));
        if profile == "auto" {
            out.text.push_str(
                "\nnote: profile is auto (core skills only); run `aaf skills sync --profile <stack>` for stack-specific skills",
            );
        }
        out = out.field(
            "skills",
            json!({
                "profile": profile,
                "host": args.host.as_str(),
                "dirs": dirs,
                "written": merged.written,
                "skipped_existing": merged.skipped_existing,
                "unchanged": merged.unchanged,
                "skipped_modified": merged.skipped_modified,
                "targets": targets,
            }),
        );
    }
    Ok(out)
}

fn run_init(args: &InitArgs, ctx: &Ctx) -> CmdResult {
    init(args, ctx, ctx::current_exe().as_deref())
}

/// Where the registered `aaf-hook` was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookSource {
    /// Only next to the running `aaf` binary: registered by absolute path.
    Sibling,
    /// On `PATH`: registered by name.
    Path,
}

impl HookSource {
    fn as_str(self) -> &'static str {
        match self {
            HookSource::Sibling => "sibling",
            HookSource::Path => "path",
        }
    }
}

/// The first `aaf-hook` on `path_var`, else the one next to `current_exe`.
///
/// `.claude/settings.json` is committed: it is read on other machines and in
/// every task worktree. A hook that is on PATH is therefore registered by
/// its name, which holds wherever `aaf-hook` is installed; an absolute path
/// is this machine's and is written only when there is nothing else.
pub(crate) fn resolve_hook(
    current_exe: Option<&Path>,
    path_var: &OsStr,
) -> Option<(PathBuf, HookSource)> {
    let sibling = || {
        // An `aaf` reached through a symlink lives where the link points.
        let exe = current_exe?;
        let exe = fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
        let candidate = exe.parent()?.join(HOOK_BINARY);
        candidate.is_file().then_some(candidate)
    };
    find_on_path(path_var, HOOK_BINARY)
        .map(|path| (path, HookSource::Path))
        .or_else(|| sibling().map(|path| (path, HookSource::Sibling)))
}

/// The command written into the settings file for a hook found at `binary`.
pub(crate) fn hook_command(binary: &Path, source: HookSource) -> Result<String, CmdError> {
    match source {
        HookSource::Path => Ok(HOOK_BINARY.to_string()),
        HookSource::Sibling => binary.to_str().map(settings::shell_quote).ok_or_else(|| {
            CmdError::new(format!(
                "{} is not valid UTF-8 and cannot be written to settings.json",
                binary.display()
            ))
        }),
    }
}

/// `Ok(None)` when the file does not exist.
fn read_optional(path: &Path) -> Result<Option<String>, CmdError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(CmdError::new(format!(
            "cannot read {}: {err}",
            path.display()
        ))),
    }
}

/// Write-then-rename, so an interrupted run never leaves half a file.
fn write_file(path: &Path, text: &str) -> Result<(), CmdError> {
    let fail = |err: io::Error| CmdError::new(format!("cannot write {}: {err}", path.display()));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(fail)?;
    }
    let mut temp = path.as_os_str().to_os_string();
    temp.push(format!(".aaf-tmp-{}", std::process::id()));
    fs::write(&temp, text).map_err(fail)?;
    fs::rename(&temp, path).map_err(fail)
}

/// `.gitignore` with the local config line appended, or `None` when a line
/// already covers it.
fn gitignore_with_local(existing: Option<&str>) -> Option<String> {
    let existing = existing.unwrap_or("");
    let covered = existing
        .lines()
        .map(str::trim)
        .any(|line| line.strip_prefix('/').unwrap_or(line) == LOCAL_CONFIG_REL);
    if covered {
        return None;
    }
    let mut text = existing.to_string();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(LOCAL_CONFIG_REL);
    text.push('\n');
    Some(text)
}

pub(crate) fn init(args: &InitArgs, ctx: &Ctx, current_exe: Option<&Path>) -> CmdResult {
    let repo_root = ctx.repo_root()?;

    // Everything that can refuse is decided before the first write.
    let hook_targets = multi_host::parse_host_arg(&args.hook_host).map_err(CmdError::new)?;
    let (hook_path, hook_source) = resolve_hook(current_exe, &ctx.path_var).ok_or_else(|| {
        CmdError::new(format!(
            "{HOOK_BINARY} was not found next to the aaf binary or on PATH; \
             install it beside aaf and run aaf init again"
        ))
    })?;
    let raw_hook_cmd = match hook_source {
        HookSource::Path => HOOK_BINARY.to_string(),
        HookSource::Sibling => hook_path
            .to_str()
            .ok_or_else(|| {
                CmdError::new(format!(
                    "{} is not valid UTF-8 and cannot be written to settings",
                    hook_path.display()
                ))
            })?
            .to_string(),
    };
    let settings_path = repo_root.join(SETTINGS_REL);
    let command = hook_command(&hook_path, hook_source)?;
    let (new_settings, hook_change) =
        settings::merge_hook(read_optional(&settings_path)?.as_deref(), &command).map_err(
            |reason| {
                CmdError::new(format!(
                    "{}: {reason}; fix or remove the file and run aaf init again \
                     (it was left untouched)",
                    settings_path.display()
                ))
            },
        )?;
    let gitignore_path = repo_root.join(".gitignore");
    let new_gitignore = gitignore_with_local(read_optional(&gitignore_path)?.as_deref());

    let mut host_merges = Vec::new();
    for target in hook_targets {
        let merge = match target {
            HookHostTarget::Claude => continue,
            HookHostTarget::Codex => multi_host::merge_codex_hook,
            HookHostTarget::Gemini => multi_host::merge_gemini_hook,
        };
        let path = repo_root.join(target.settings_rel());
        let (text, change) =
            merge(read_optional(&path)?.as_deref(), &raw_hook_cmd).map_err(CmdError::new)?;
        host_merges.push((target, path, text, change));
    }

    let detected = detect_stack(&repo_root);
    let config_path = repo_root.join(REPO_CONFIG_REL);
    let config_action = match (config_path.exists(), args.force) {
        (true, false) => "kept",
        (true, true) => "overwritten",
        (false, _) => "written",
    };

    if config_action != "kept" {
        let name = repo_root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        write_file(&config_path, &render_repo_config(&detected, &name))?;
    }
    if let Some(text) = &new_settings {
        write_file(&settings_path, text)?;
    }
    if let Some(text) = &new_gitignore {
        write_file(&gitignore_path, text)?;
    }

    let mut extra_hooks = Vec::new();
    for (target, path, text, change) in &host_merges {
        if let Some(text) = text {
            write_file(path, text)?;
        }
        extra_hooks.push((target.as_str(), target.settings_rel(), change.as_str()));
    }

    // After the writes: what git has of the file as it is now.
    let tracking = Tracking::of(&repo_root);
    let commands = &detected.commands;
    // Only a statement about the file this run wrote: a kept config may
    // carry commands of its own.
    let unverified = config_action != "kept"
        && (commands.test_scoped.is_empty() || commands.test_full.is_empty());
    let gitignore_action = if new_gitignore.is_some() {
        "added"
    } else {
        "unchanged"
    };

    let mut text = format!(
        "profile: {} ({})\nconfig: {} {}\nhook: {} in {} -> {} (found {})\ngitignore: {} {}",
        detected.profile,
        if detected.evidence.is_empty() {
            "no known marker file".to_string()
        } else {
            detected.evidence.join(", ")
        },
        config_action,
        config_path.display(),
        hook_change.as_str(),
        settings_path.display(),
        hook_path.display(),
        match hook_source {
            HookSource::Sibling => {
                "next to aaf, not on PATH: registered by its absolute path, \
                 which is this machine's"
            }
            HookSource::Path => "on PATH: registered by name",
        },
        gitignore_action,
        LOCAL_CONFIG_REL,
    );
    for (host_name, rel_path, action) in &extra_hooks {
        text.push_str(&format!("\nhook ({host_name}): {action} in {rel_path}"));
    }
    if config_action == "kept" {
        text.push_str("\nnote: the existing config was kept; --force regenerates it");
    }
    if let Some(problem) = tracking.problem() {
        // A task worktree is cut from a commit, not from this working tree.
        text.push_str(&format!(
            "\nnote: {SETTINGS_REL} {problem}. The agents of `aaf run` work in worktrees cut \
             from a commit and get the hook only from a committed file: \
             git add {SETTINGS_REL} {REPO_CONFIG_REL} .gitignore && git commit"
        ));
        if tracking == Tracking::Ignored {
            text.push_str(" (remove the ignore rule first, or use `git add -f`)");
        }
    }
    if unverified {
        text.push_str(
            "\nnote: no test command is certain for this stack; the gate reports Unverified \
             until [commands] test_scoped and test_full are set in the config",
        );
    }

    Ok(CmdOutput::ok("ok", text)
        .field("repo_root", json!(repo_root))
        .field("profile", json!(detected.profile))
        .field("evidence", json!(detected.evidence))
        .field("commands", json!(commands))
        .field(
            "config",
            json!({"path": config_path, "action": config_action}),
        )
        .field(
            "hook",
            json!({
                "settings_path": settings_path,
                "action": hook_change.as_str(),
                "matcher": settings::matcher(),
                "command": command,
                "binary": hook_path,
                "source": hook_source.as_str(),
                "registered_as": match hook_source {
                    HookSource::Path => "name",
                    HookSource::Sibling => "absolute_path",
                },
                "tracking": tracking.as_str(),
                "commit_required": tracking != Tracking::Committed,
            }),
        )
        .field(
            "gitignore",
            json!({"path": gitignore_path, "action": gitignore_action}),
        )
        .field("gate_unverified_until_commands_set", json!(unverified)))
}
