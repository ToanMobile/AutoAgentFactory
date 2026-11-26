//! Base-infra drift rebase check and cross-branch ancestry guard for worker turns.

use aaf_domain::config::RepoConfig;
use aaf_domain::engine_contracts::REPO_CONFIG_REL;
use aaf_domain::ports::PortResult;
use aaf_domain::task::Task;

use super::super::ports::Grant;
use super::super::stage_router::StageTarget;
use super::super::{Deps, unblock};
use super::Next;
use crate::gate::head_lock_path_matcher;
use crate::git::TaskWorktree;
use crate::unblocker::FailureSignal;

/// True when any path modified on the base branch since the worktree's
/// merge-base affects gate infrastructure (config, hook settings, scripts
/// referenced by `[commands]`, or locked globs).
pub(super) fn base_touched_gate_infra(changed: &[String], repo: &RepoConfig) -> bool {
    if changed.is_empty() {
        return false;
    }
    let commands = [
        repo.commands.build.as_str(),
        repo.commands.test_scoped.as_str(),
        repo.commands.test_full.as_str(),
        repo.commands.lint.as_str(),
        repo.commands.lint_full.as_str(),
    ];
    let locked = head_lock_path_matcher(&repo.gate.head_lock_globs);
    changed.iter().any(|path| {
        path == REPO_CONFIG_REL
            || path == ".claude/settings.json"
            || locked(path)
            || commands
                .iter()
                .any(|cmd| !cmd.is_empty() && cmd.contains(path.as_str()))
    })
}

/// Re-bases `worktree` onto current base tip when base modified gate
/// infrastructure since `merge-base(base, HEAD)`. Returns `Some(Next)` if the
/// rebase hit merge conflicts and was handed to the unblocker ladder.
pub(super) fn rebase_if_gate_infra_drifted(
    deps: &Deps,
    task_id: &str,
    worktree: &TaskWorktree,
    grant: &Grant,
    routed: Option<&StageTarget>,
) -> PortResult<Option<Next>> {
    let changed = deps.git.base_changed_paths_since_merge_base(worktree)?;
    if !base_touched_gate_infra(&changed, &deps.settings.repo) {
        return Ok(None);
    }
    let conflicts = deps.git.rebase(worktree)?;
    if conflicts.is_empty() {
        return Ok(None);
    }
    let mut signal = FailureSignal::empty();
    signal.merge_conflict_files = conflicts;
    unblock::handle_with(deps, task_id, Some(worktree), signal, grant, routed).map(Some)
}

/// Blocks the task before gating when its branch HEAD pulled in unmerged
/// commits from another `aaf/*` task branch (e.g. a subtask running
/// `git merge aaf/<parent>` or `git merge aaf/<sibling>`).
pub(super) fn block_if_cross_branch_contaminated(
    deps: &Deps,
    task: &Task,
    worktree: &TaskWorktree,
) -> PortResult<bool> {
    let foreign = deps.git.foreign_aaf_branch_ancestors(worktree)?;
    if foreign.is_empty() {
        return Ok(false);
    }
    let why = format!(
        "cross-branch contamination: {} contains unmerged commits from {}; \
         each task branch must stay isolated on the clean base branch",
        worktree.branch,
        foreign.join(", ")
    );
    unblock::block_untried(deps, &task.id, &why)?;
    Ok(true)
}
