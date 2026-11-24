//! What an agent run is given: the prompt and the sandbox policy.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use aaf_domain::account::AccountProfile;
use aaf_domain::ports::SandboxPolicy;
use aaf_domain::task::Task;

use super::Settings;

/// `runs.executor` of a read-only plan-stage run: separates plan runs from
/// implementation runs in account history, audit author lookup, and worktree
/// rebase checks.
pub const PLAN_EXECUTOR: &str = "plan";
/// Context note prefix rendered under `# Approved Plan`.
pub const PLAN_APPROVED_PREFIX: &str = "plan_approved:";
/// Context note prefix rendered under `# Operator Steering`.
pub const STEER_PREFIX: &str = "steer:";

const MAX_LISTED_SKILLS: usize = 20;

/// Repository runtime hints appended to an agent prompt.
#[derive(Debug, Clone)]
pub struct PromptContext<'a> {
    pub repo_root: &'a Path,
    pub profile: &'a str,
    pub proof_required: bool,
}

/// The prompt of one agent run: fixed rules, the immutable spec, then the
/// notes earlier attempts left. Notes are passed through untouched, the
/// `unblock[Tn] digest=` marker lines included, so the agent sees what was
/// already tried.
pub fn build_prompt(task: &Task, branch: &str) -> String {
    let mut prompt = format!(
        "You are working on one task in the git worktree at the current directory, on \
         branch `{branch}`.\n\
         Rules: stay on this branch and inside this directory; write the test first, then the \
         change; commit your work on this branch when it is complete; do not push, do not \
         merge, do not edit the task description. Existing tests and build configuration are \
         locked: change them only when the task requires it.\n\
         You run unattended; nobody answers questions. When the safety hook denies a command, \
         follow its fix (use the Write/Edit tools for file content, simpler or split commands \
         — e.g. run a heredoc that writes a file as its own command, then git commands \
         separately). If only a human can do a step, finish without it and name the step in \
         your final message.\n\n\
         # Task: {}\n\n{}\n\n# Acceptance criteria\n\n",
        task.title, task.spec.description
    );
    for criterion in &task.spec.acceptance {
        prompt.push_str(&format!("- {criterion}\n"));
    }
    let mut approved_plans: Vec<&str> = Vec::new();
    let mut steering: Vec<&str> = Vec::new();
    let mut failure_notes: Vec<&str> = Vec::new();
    for note in &task.context_notes {
        if let Some(rest) = note.strip_prefix(PLAN_APPROVED_PREFIX) {
            let trimmed = rest.trim();
            if !trimmed.is_empty() {
                approved_plans.push(trimmed);
            }
        } else if let Some(rest) = note.strip_prefix(STEER_PREFIX) {
            let trimmed = rest.trim();
            if !trimmed.is_empty() {
                steering.push(trimmed);
            }
        } else {
            failure_notes.push(note.as_str());
        }
    }
    if !approved_plans.is_empty() {
        prompt.push_str("\n# Approved Plan\n\n");
        for plan in approved_plans {
            prompt.push_str(plan);
            prompt.push('\n');
        }
    }
    if !steering.is_empty() {
        prompt.push_str("\n# Operator Steering\n\n");
        for note in steering {
            prompt.push_str(&format!("- {note}\n"));
        }
    }
    if !failure_notes.is_empty() {
        prompt.push_str(
            "\n# Notes from earlier attempts\n\n\
             Earlier runs of this task failed as described below. Do not repeat the same \
             approach. For a merge conflict, merge the base branch into this branch and \
             resolve it.\n\n",
        );
        for note in failure_notes {
            prompt.push_str(&format!("- {note}\n"));
        }
    }
    prompt
}

/// Prompt for the read-only plan stage: instructs the agent to inspect the
/// repository and produce a concise implementation plan without editing files.
pub fn build_plan_prompt(task: &Task, ctx: &PromptContext<'_>) -> String {
    let mut prompt = format!(
        "You are in READ-ONLY planning mode for one task. Do NOT edit, create, or delete \
         any files, and do NOT run commits or mutating commands.\n\
         Inspect the codebase and produce a concise implementation plan covering:\n\
         1. Files to change (paths and responsibilities)\n\
         2. Test strategy (tests to add/update to prove each acceptance criterion)\n\
         3. Risks and edge cases\n\n\
         # Task: {}\n\n{}\n\n# Acceptance criteria\n\n",
        task.title, task.spec.description
    );
    for criterion in &task.spec.acceptance {
        prompt.push_str(&format!("- {criterion}\n"));
    }
    let steering: Vec<&str> = task
        .context_notes
        .iter()
        .filter_map(|note| note.strip_prefix(STEER_PREFIX))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if !steering.is_empty() {
        prompt.push_str("\n# Operator Steering\n\n");
        for note in steering {
            prompt.push_str(&format!("- {note}\n"));
        }
    }
    append_repo_environment(&mut prompt, ctx);
    prompt
}

/// [`build_prompt`] plus a concise `## Repo environment` section describing
/// the active profile, synced skills, proof policy and heavy-command slot hint.
pub fn build_prompt_with_context(task: &Task, branch: &str, ctx: &PromptContext<'_>) -> String {
    let mut prompt = build_prompt(task, branch);
    append_repo_environment(&mut prompt, ctx);
    prompt
}

fn append_repo_environment(prompt: &mut String, ctx: &PromptContext<'_>) {
    prompt.push_str("\n## Repo environment\n\n");
    let profile = ctx.profile.trim();
    if !profile.is_empty() && profile != "auto" {
        prompt.push_str(&format!("- Active profile: `{profile}`\n"));
    }
    let skills = discover_skills(ctx.repo_root);
    if !skills.is_empty() {
        let shown: Vec<&str> = skills
            .iter()
            .take(MAX_LISTED_SKILLS)
            .map(String::as_str)
            .collect();
        let extra = skills.len().saturating_sub(shown.len());
        if extra > 0 {
            prompt.push_str(&format!(
                "- Synced skills: {} (+{extra} more)\n",
                shown.join(", ")
            ));
        } else {
            prompt.push_str(&format!("- Synced skills: {}\n", shown.join(", ")));
        }
    }
    if ctx.proof_required {
        prompt.push_str(
            "- Verification proof required: capture a device screenshot via `mobile-mcp` \
             (`mobile_save_screenshot`); the host/operator records the signed proof outside \
             the sandbox before the gate clears.\n",
        );
    }
    prompt.push_str(
        "- Heavy builds/tests: wrap heavy build or test commands with `aaf slot -- <cmd>` \
         so they respect machine load limits.\n",
    );
}

fn discover_skills(repo_root: &Path) -> Vec<String> {
    let mut names = BTreeSet::new();
    for rel in [".claude/skills", ".agents/skills"] {
        let Ok(entries) = fs::read_dir(repo_root.join(rel)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir()
                && path.join("SKILL.md").is_file()
                && let Some(name) = entry.file_name().to_str()
                && !name.starts_with('.')
            {
                names.insert(name.to_string());
            }
        }
    }
    names.into_iter().collect()
}

/// Sandbox for a run on `own` account.
///
/// Sandboxed: the agent may not read the aaf data dir (receipt key,
/// approvals) nor another account's config dir. `<home>/.claude` is never
/// denied: profile directories symlink their `projects` and `settings` into
/// it, so denying it breaks every other Claude account.
/// Unsandboxed (Linux, or `--unsandboxed`): disabled, and the executor then
/// drops its permission-bypass flags.
pub fn sandbox_policy(
    sandboxed: bool,
    data_dir: &Path,
    home: &Path,
    own: Option<&AccountProfile>,
    all: &[AccountProfile],
) -> SandboxPolicy {
    if !sandboxed {
        return SandboxPolicy::default();
    }
    // Resolved, because the sandbox matches resolved paths: an account dir
    // that is itself a link to `~/.claude` must not deny it either.
    let resolve = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let shared = resolve(&home.join(".claude"));
    let own_dir = own.map(|own| resolve(Path::new(&own.config_dir)));
    let mut read_denied = vec![data_dir.to_string_lossy().into_owned()];
    for account in all {
        let dir = resolve(Path::new(&account.config_dir));
        // A directory that is gone cannot be read, so it needs no rule.
        if Some(&dir) != own_dir.as_ref() && dir != shared && dir.is_dir() {
            read_denied.push(account.config_dir.clone());
        }
    }
    SandboxPolicy {
        enabled: true,
        writable_paths: Vec::new(),
        read_denied_paths: read_denied,
        read_only: false,
    }
}

/// Read-only sandbox policy for a plan-stage run: denies writes to the
/// worktree, `.git`, and build caches, and causes `ClaudeCodeRunner` to pass
/// `--permission-mode plan`.
pub fn plan_sandbox_policy(
    settings: &Settings,
    own: Option<&AccountProfile>,
    all: &[AccountProfile],
) -> SandboxPolicy {
    SandboxPolicy {
        read_only: true,
        ..sandbox_policy(
            settings.sandboxed,
            &settings.data_dir,
            &settings.home,
            own,
            all,
        )
    }
}
