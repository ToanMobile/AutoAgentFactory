//! `aaf skills list|sync`: inspect and sync profile-appropriate domain skills.

use std::path::{Path, PathBuf};

use aaf_engine::config::{PROFILES, REPO_CONFIG_REL, detect_stack, effective_profile};
use aaf_engine::skills::{
    DEFAULT_GLOBAL_SKILLS, GlobalSkillHost, SkillSyncOutcome, all_skills, global_skill_dirs,
    is_valid_profile, skills_for_profile, sync_skills_for_profile,
};
use serde_json::json;

use super::board::store::{refuse_inside_agent_run, repo_root};
use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput, CmdResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum SkillHost {
    #[default]
    Claude,
    #[value(alias = "codex")]
    Agents,
    #[value(alias = "gemini")]
    Agent,
    All,
}

impl SkillHost {
    pub fn as_str(self) -> &'static str {
        match self {
            SkillHost::Claude => "claude",
            SkillHost::Agents => "agents",
            SkillHost::Agent => "agent",
            SkillHost::All => "all",
        }
    }

    pub fn resolve_dirs(self, root: &Path) -> Vec<PathBuf> {
        match self {
            SkillHost::Claude => vec![root.join(".claude").join("skills")],
            SkillHost::Agents => vec![root.join(".agents").join("skills")],
            SkillHost::Agent => vec![root.join(".agent").join("skills")],
            SkillHost::All => vec![
                root.join(".claude").join("skills"),
                root.join(".agents").join("skills"),
                root.join(".agent").join("skills"),
            ],
        }
    }
}

fn resolve_global_dirs(host: Option<SkillHost>, home: &Path) -> (Vec<PathBuf>, &'static str) {
    match host {
        None => (
            global_skill_dirs(home, GlobalSkillHost::ExistingOrDefault),
            "global",
        ),
        Some(SkillHost::Claude) => (
            global_skill_dirs(home, GlobalSkillHost::Claude),
            "global:claude",
        ),
        Some(SkillHost::Agents) => (
            global_skill_dirs(home, GlobalSkillHost::Codex),
            "global:codex",
        ),
        Some(SkillHost::Agent) => (
            global_skill_dirs(home, GlobalSkillHost::Gemini),
            "global:gemini",
        ),
        Some(SkillHost::All) => (global_skill_dirs(home, GlobalSkillHost::All), "global:all"),
    }
}

#[derive(Debug, clap::Args)]
pub struct SkillsArgs {
    #[command(subcommand)]
    pub command: SkillsCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum SkillsCommand {
    /// List bundled AutoAgentFactory domain skills (all, or filtered by `--profile`).
    List(ListArgs),
    /// Write profile-matched skills into `.claude/skills`, `.agents/skills`, `.agent/skills`, `--dir`, or `--global` without clobbering edits.
    Sync(SyncArgs),
}

#[derive(Debug, clap::Args)]
pub struct ListArgs {
    /// Show only skills for this stack profile (`auto`, `android`, `ios`, `flutter`, `react-native`, `expo`, `web`, `backend`, `rust`, `unity`).
    #[arg(long)]
    pub profile: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct SyncArgs {
    /// Destination directory; overrides `--host` when provided.
    #[arg(long)]
    pub dir: Option<PathBuf>,
    /// Sync into global user-level host directories (`~/.claude/skills`, `~/.gemini/config/skills`, `~/.codex/skills`). Defaults to `aaf-workflow` unless `--only`, `--profile`, or `--all` is given.
    #[arg(long, conflicts_with = "dir")]
    pub global: bool,
    /// Target host directory layout (`claude` -> `.claude/skills`, `agents`/`codex` -> `.agents/skills`, `agent`/`gemini` -> `.agent/skills`, `all` -> all hosts).
    #[arg(long, value_enum)]
    pub host: Option<SkillHost>,
    /// Override target profile instead of auto-detecting from repo config/files.
    #[arg(long, conflicts_with = "all")]
    pub profile: Option<String>,
    /// Sync every bundled skill regardless of repository profile.
    #[arg(long)]
    pub all: bool,
    /// Sync only specific named skills; repeat for more.
    #[arg(long = "only")]
    pub only: Vec<String>,
    /// Overwrite existing SKILL.md files.
    #[arg(long)]
    pub force: bool,
}

pub fn run(args: &SkillsArgs, ctx: &Ctx) -> CmdResult {
    match &args.command {
        SkillsCommand::List(list_args) => list(list_args),
        SkillsCommand::Sync(sync_args) => sync(sync_args, ctx),
    }
}

fn validate_profile(profile: &str) -> Result<(), CmdError> {
    if is_valid_profile(profile) {
        return Ok(());
    }
    Err(CmdError::new(format!(
        "--profile {profile:?} is unknown: expected auto or {}",
        PROFILES.join(", ")
    )))
}

fn list(args: &ListArgs) -> CmdResult {
    let skills: Vec<_> = match &args.profile {
        Some(p) => {
            validate_profile(p)?;
            skills_for_profile(p)
        }
        None => all_skills().iter().collect(),
    };
    let lines: Vec<String> = skills
        .iter()
        .map(|s| format!("{:<24} {}", s.name, s.description))
        .collect();
    let rows: Vec<_> = skills
        .iter()
        .map(|s| {
            let profiles = if s.profiles.is_empty() {
                vec!["all"]
            } else {
                s.profiles.to_vec()
            };
            json!({
                "name": s.name,
                "description": s.description,
                "profiles": profiles,
            })
        })
        .collect();
    Ok(CmdOutput::ok("ok", lines.join("\n"))
        .field("count", json!(skills.len()))
        .field("profile", json!(args.profile))
        .field("skills", json!(rows)))
}

pub(crate) fn merge_outcomes(outcomes: &[SkillSyncOutcome]) -> SkillSyncOutcome {
    let mut merged = SkillSyncOutcome::default();
    let push_unique = |dst: &mut Vec<String>, src: &[String]| {
        for item in src {
            if !dst.contains(item) {
                dst.push(item.clone());
            }
        }
    };
    for out in outcomes {
        push_unique(&mut merged.written, &out.written);
        push_unique(&mut merged.skipped_existing, &out.skipped_existing);
        push_unique(&mut merged.unchanged, &out.unchanged);
        push_unique(&mut merged.skipped_modified, &out.skipped_modified);
    }
    merged
}

fn sync(args: &SyncArgs, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf skills sync") {
        return Ok(refused);
    }
    let repo = if args.global {
        None
    } else {
        repo_root(ctx).ok()
    };
    let (dests, host_label) = if args.global {
        resolve_global_dirs(args.host, &ctx.home)
    } else {
        match &args.dir {
            Some(dir) => (vec![dir.clone()], "custom"),
            None => {
                let root = match &repo {
                    Some(root) => root.clone(),
                    None => repo_root(ctx)?,
                };
                let host = args.host.unwrap_or_default();
                (host.resolve_dirs(&root), host.as_str())
            }
        }
    };

    if let Some(explicit) = &args.profile {
        validate_profile(explicit)?;
    }

    let default_global_only: Vec<String> =
        if args.global && !args.all && args.profile.is_none() && args.only.is_empty() {
            DEFAULT_GLOBAL_SKILLS
                .iter()
                .map(|s| (*s).to_string())
                .collect()
        } else {
            Vec::new()
        };
    let only: Option<&[String]> = if !args.only.is_empty() {
        Some(args.only.as_slice())
    } else if !default_global_only.is_empty() {
        Some(default_global_only.as_slice())
    } else {
        None
    };

    let resolved_profile = if args.all || only.is_some() {
        None
    } else if let Some(explicit) = &args.profile {
        Some(explicit.clone())
    } else if let Some(root) = &repo {
        let profile = if root.join(REPO_CONFIG_REL).exists() {
            let cfg = ctx.load_config(root)?;
            effective_profile(&cfg.repo, root)
        } else {
            detect_stack(root).profile
        };
        Some(profile)
    } else {
        Some("auto".to_string())
    };

    let mut per_target = Vec::with_capacity(dests.len());
    let mut raw_outcomes = Vec::with_capacity(dests.len());
    let mut lines = Vec::with_capacity(dests.len());
    for dest in &dests {
        let outcome = sync_skills_for_profile(dest, resolved_profile.as_deref(), only, args.force)
            .map_err(|e| CmdError::new(e.to_string()))?;
        lines.push(format!(
            "skills synced to {} (profile={}): written={} unchanged={} skipped_modified={}",
            dest.display(),
            resolved_profile.as_deref().unwrap_or("all"),
            outcome.written.len(),
            outcome.unchanged.len(),
            outcome.skipped_modified.len()
        ));
        per_target.push(json!({
            "dir": dest.to_string_lossy().into_owned(),
            "written": outcome.written,
            "skipped_existing": outcome.skipped_existing,
            "unchanged": outcome.unchanged,
            "skipped_modified": outcome.skipped_modified,
        }));
        raw_outcomes.push(outcome);
    }

    let merged = merge_outcomes(&raw_outcomes);
    let dirs_json: Vec<String> = dests
        .iter()
        .map(|d| d.to_string_lossy().into_owned())
        .collect();
    let primary_dir = dirs_json.first().cloned().unwrap_or_default();

    Ok(CmdOutput::ok("ok", lines.join("\n"))
        .field("dir", json!(primary_dir))
        .field("dirs", json!(dirs_json))
        .field("host", json!(host_label))
        .field("global", json!(args.global))
        .field("profile", json!(resolved_profile))
        .field("written", json!(merged.written))
        .field("skipped_existing", json!(merged.skipped_existing))
        .field("unchanged", json!(merged.unchanged))
        .field("skipped_modified", json!(merged.skipped_modified))
        .field("targets", json!(per_target)))
}
