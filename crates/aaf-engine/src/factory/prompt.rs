//! What an agent run is given: the prompt and the sandbox policy.

use std::path::Path;

use aaf_domain::account::AccountProfile;
use aaf_domain::ports::SandboxPolicy;
use aaf_domain::task::Task;

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
         locked: change them only when the task requires it.\n\n\
         # Task: {}\n\n{}\n\n# Acceptance criteria\n\n",
        task.title, task.spec.description
    );
    for criterion in &task.spec.acceptance {
        prompt.push_str(&format!("- {criterion}\n"));
    }
    if !task.context_notes.is_empty() {
        prompt.push_str(
            "\n# Notes from earlier attempts\n\n\
             Earlier runs of this task failed as described below. Do not repeat the same \
             approach. For a merge conflict, merge the base branch into this branch and \
             resolve it.\n\n",
        );
        for note in &task.context_notes {
            prompt.push_str(&format!("- {note}\n"));
        }
    }
    prompt
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
    }
}
