//! `aaf skills list|sync`: inspect and sync profile-appropriate domain skills.

use std::path::{Path, PathBuf};

use aaf_engine::config::{PROFILES, REPO_CONFIG_REL, detect_stack, effective_profile};
use aaf_engine::skills::{
    SkillSyncOutcome, all_skills, is_valid_profile, skills_for_profile, sync_skills_for_profile,
};
use serde_json::json;

use super::board::store::{refuse_inside_agent_run, repo_root};
use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput, CmdResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum SkillHost {
    #[default]
    Claude,
    Agents,
    All,
}

impl SkillHost {
    pub fn as_str(self) -> &'static str {
        match self {
            SkillHost::Claude => "claude",
            SkillHost::Agents => "agents",
            SkillHost::All => "all",
        }
    }

    pub fn resolve_dirs(self, root: &Path) -> Vec<PathBuf> {
        match self {
            SkillHost::Claude => vec![root.join(".claude").join("skills")],
            SkillHost::Agents => vec![root.join(".agents").join("skills")],
            SkillHost::All => vec![
                root.join(".claude").join("skills"),
                root.join(".agents").join("skills"),
            ],
        }
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
    /// Write profile-matched skills into `.claude/skills`, `.agents/skills`, or `--dir` without clobbering edits.
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
    /// Target host directory layout (`claude` -> `.claude/skills`, `agents` -> `.agents/skills`, `all` -> both).
    #[arg(long, value_enum, default_value_t = SkillHost::Claude)]
    pub host: SkillHost,
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
    let repo = repo_root(ctx).ok();
    let (dests, host_label) = match &args.dir {
        Some(dir) => (vec![dir.clone()], "custom"),
        None => {
            let root = match &repo {
                Some(root) => root.clone(),
                None => repo_root(ctx)?,
            };
            (args.host.resolve_dirs(&root), args.host.as_str())
        }
    };

    if let Some(explicit) = &args.profile {
        validate_profile(explicit)?;
    }
    let resolved_profile = if args.all || !args.only.is_empty() {
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
    let only = (!args.only.is_empty()).then_some(args.only.as_slice());

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
        .field("profile", json!(resolved_profile))
        .field("written", json!(merged.written))
        .field("skipped_existing", json!(merged.skipped_existing))
        .field("unchanged", json!(merged.unchanged))
        .field("skipped_modified", json!(merged.skipped_modified))
        .field("targets", json!(per_target)))
}
