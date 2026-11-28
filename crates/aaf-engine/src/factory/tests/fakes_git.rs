//! In-memory [`GitOps`] fake used by the factory loop tests.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use aaf_domain::gate::Receipt;
use aaf_domain::ports::{PortError, PortResult};

use crate::factory::ports::{GitOps, Integration};
use crate::git::pr_feedback_gh::{PrFeedbackSnapshot, PrRef};
use crate::git::pr_merged_gh::VerifiedPrMerge;
use crate::git::{GitError, MergeOutcome, PrOutcome, TaskWorktree};
use crate::pr_ops::PrMergeOutcome;

pub(super) fn task_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[derive(Default)]
pub struct GitState {
    /// Tree path -> task ids whose branch is in it.
    pub trees: HashMap<PathBuf, Vec<String>>,
    /// Tasks whose branch conflicts when a batch is built.
    pub conflicts: HashSet<String>,
    pub no_changes: HashSet<String>,
    pub fail_integrate: bool,
    pub rebase_conflicts: HashMap<String, Vec<String>>,
    /// Scripted merge outcomes per task; without one the merge follows the
    /// regate callback.
    pub merge_script: HashMap<String, VecDeque<MergeOutcome>>,
    pub pr: Option<PrOutcome>,
    /// `open_pr` fails with this error.
    pub pr_error: Option<String>,
    pub base: u32,
    pub merged: Vec<String>,
    pub integrations: u32,
    pub discarded: u32,
    pub fetches: u32,
    pub rebases: Vec<String>,
    /// `commit:<task>` and `rebase:<task>`, in call order.
    pub log: Vec<String>,
    /// Tasks whose worktree and branch were removed.
    pub cleaned: Vec<String>,
    /// Somebody else pushes to the base right before these tasks merge.
    pub external_before: HashSet<String>,
    /// Worktree path -> tasks already in the base it was last rebased onto.
    pub rebased_on: HashMap<PathBuf, Vec<String>>,
    /// Somebody else pushes to the base while an integration tree is built.
    pub integrate_moves_base: bool,
    /// The note of every pull request, by task.
    pub pr_notes: Vec<(String, Option<String>)>,
    /// Optional per-worktree diff override.
    pub diffs: HashMap<PathBuf, String>,
    /// `base_sha` of every `diff_from` call.
    pub diff_from_calls: Vec<String>,
    /// Optional PR feedback snapshot returned by `pr_feedback` (keyed by PR number).
    pub pr_feedback: HashMap<u64, PortResult<Option<PrFeedbackSnapshot>>>,
    /// Number of times `GitOps::pr_feedback` was called.
    pub pr_feedback_calls: u32,
    /// Scripted `merge_pr` outcomes, oldest first; empty means unsupported.
    pub merge_pr_script: VecDeque<PrMergeOutcome>,
    /// `(pr_url, head_sha)` of every `merge_pr` call.
    pub merge_pr_calls: Vec<(String, String)>,
    /// Optional verified merged PR outcomes returned by `verified_merged_pr` (keyed by PR number).
    pub verified_merged_pr: HashMap<u64, Result<Option<VerifiedPrMerge>, String>>,
}

#[derive(Default)]
pub struct FakeGit(pub Mutex<GitState>);

impl FakeGit {
    pub fn ids(&self, tree: &Path) -> Vec<String> {
        let state = self.0.lock().unwrap();
        state.trees.get(tree).cloned().unwrap_or_default()
    }
}

pub fn worktree(task_id: &str) -> TaskWorktree {
    TaskWorktree {
        path: PathBuf::from(format!("/fake/wt/{task_id}")),
        branch: format!("aaf/{task_id}"),
    }
}

impl GitOps for FakeGit {
    fn task_worktree(&self, task_id: &str) -> PortResult<TaskWorktree> {
        let tree = worktree(task_id);
        let mut state = self.0.lock().unwrap();
        state
            .trees
            .insert(tree.path.clone(), vec![task_id.to_string()]);
        Ok(tree)
    }
    fn commit_all(&self, worktree: &TaskWorktree, _message: &str) -> PortResult<()> {
        let entry = format!("commit:{}", task_of(&worktree.path));
        self.0.lock().unwrap().log.push(entry);
        Ok(())
    }
    fn has_changes(&self, worktree: &TaskWorktree) -> PortResult<bool> {
        let state = self.0.lock().unwrap();
        Ok(!state.no_changes.contains(&task_of(&worktree.path)))
    }
    fn diff(&self, worktree: &TaskWorktree) -> PortResult<String> {
        if let Some(d) = self.0.lock().unwrap().diffs.get(&worktree.path) {
            return Ok(d.clone());
        }
        let id = task_of(&worktree.path);
        Ok(format!(
            "diff --git a/src/lib.rs b/src/lib.rs\n\
             --- a/src/lib.rs\n\
             +++ b/src/lib.rs\n\
             @@ -1,1 +1,2 @@\n\
              fn base() {{}}\n\
             +change of {id}\n"
        ))
    }
    fn diff_from(&self, worktree: &TaskWorktree, base_sha: &str) -> PortResult<String> {
        self.0
            .lock()
            .unwrap()
            .diff_from_calls
            .push(base_sha.to_string());
        self.diff(worktree)
    }
    fn integrate(&self, task_ids: &[String]) -> PortResult<Integration> {
        let mut state = self.0.lock().unwrap();
        if state.fail_integrate {
            return Err(PortError::Io("disk full".to_string()));
        }
        state.integrations += 1;
        if state.integrate_moves_base {
            state.base += 100;
        }
        let name = format!("batch-{}", state.integrations);
        let (conflicted, merged): (Vec<String>, Vec<String>) = task_ids
            .iter()
            .cloned()
            .partition(|id| state.conflicts.contains(id));
        let tree = worktree(&name);
        state.trees.insert(tree.path.clone(), merged.clone());
        Ok(Integration {
            tree,
            merged,
            conflicted,
        })
    }
    fn discard(&self, integration: &Integration) -> PortResult<()> {
        let mut state = self.0.lock().unwrap();
        state.discarded += 1;
        state.trees.remove(&integration.tree.path);
        Ok(())
    }
    fn rebase(&self, worktree: &TaskWorktree) -> PortResult<Vec<String>> {
        let id = task_of(&worktree.path);
        let mut state = self.0.lock().unwrap();
        state.rebases.push(id.clone());
        state.log.push(format!("rebase:{id}"));
        Ok(state.rebase_conflicts.get(&id).cloned().unwrap_or_default())
    }
    fn reset_to(&self, _worktree: &TaskWorktree, _sha: &str) -> PortResult<()> {
        Ok(())
    }
    fn base_sha(&self) -> PortResult<String> {
        Ok(format!("base{}", self.0.lock().unwrap().base))
    }
    fn merge(
        &self,
        worktree: &TaskWorktree,
        regate: &dyn Fn(&Path) -> Result<bool, String>,
    ) -> PortResult<MergeOutcome> {
        let id = task_of(&worktree.path);
        let scripted = {
            let mut state = self.0.lock().unwrap();
            let next = state
                .merge_script
                .get_mut(&id)
                .and_then(VecDeque::pop_front);
            if next == Some(MergeOutcome::NotFastForward) {
                // Somebody else pushed to the base in the meantime.
                state.base += 100;
            }
            next
        };
        if let Some(outcome) = scripted {
            return Ok(outcome);
        }
        {
            let mut state = self.0.lock().unwrap();
            if state.external_before.remove(&id) {
                state.base += 100;
            }
            // The merge queue rebases first: what the gate judges next is
            // this branch on top of everything that already landed.
            let landed = state.merged.clone();
            state.rebased_on.insert(worktree.path.clone(), landed);
        }
        // Not under the lock: the callback asks for the base.
        if !regate(&worktree.path).map_err(PortError::Io)? {
            return Ok(MergeOutcome::GateFailed);
        }
        let mut state = self.0.lock().unwrap();
        state.base += 1;
        state.merged.push(id);
        Ok(MergeOutcome::Merged {
            new_base_sha: format!("base{}", state.base),
        })
    }
    fn open_pr(
        &self,
        worktree: &TaskWorktree,
        _title: &str,
        _receipt: &Receipt,
        note: Option<&str>,
    ) -> PortResult<PrOutcome> {
        let mut state = self.0.lock().unwrap();
        let entry = (task_of(&worktree.path), note.map(str::to_string));
        state.pr_notes.push(entry);
        if let Some(error) = &state.pr_error {
            return Err(PortError::Io(error.clone()));
        }
        Ok(state.pr.clone().unwrap_or(PrOutcome::Skipped {
            reason: "the repository has no remote".to_string(),
        }))
    }
    fn fetch_base(&self) -> PortResult<()> {
        self.0.lock().unwrap().fetches += 1;
        Ok(())
    }
    fn is_merged(&self, task_id: &str) -> PortResult<bool> {
        Ok(self.0.lock().unwrap().merged.iter().any(|m| m == task_id))
    }
    fn cleanup(&self, task_id: &str) -> PortResult<()> {
        self.0.lock().unwrap().cleaned.push(task_id.to_string());
        Ok(())
    }
    fn merge_pr(
        &self,
        _wt: &TaskWorktree,
        pr_url: &str,
        head_sha: &str,
    ) -> PortResult<PrMergeOutcome> {
        let mut state = self.0.lock().unwrap();
        state
            .merge_pr_calls
            .push((pr_url.to_string(), head_sha.to_string()));
        state
            .merge_pr_script
            .pop_front()
            .ok_or_else(|| PortError::Invalid("merge_pr not supported".to_string()))
    }
    fn pr_feedback(
        &self,
        _repo_root: &Path,
        pr: &PrRef,
        _expected_head_branch: &str,
    ) -> PortResult<Option<PrFeedbackSnapshot>> {
        let mut state = self.0.lock().unwrap();
        state.pr_feedback_calls += 1;
        match state.pr_feedback.get(&pr.number) {
            Some(Ok(snap)) => Ok(snap.clone()),
            Some(Err(err)) => Err(PortError::Io(err.to_string())),
            None => Ok(None),
        }
    }
    fn verified_merged_pr(&self, pr: &PrRef) -> Result<Option<VerifiedPrMerge>, GitError> {
        let state = self.0.lock().unwrap();
        match state.verified_merged_pr.get(&pr.number) {
            Some(Ok(res)) => Ok(res.clone()),
            Some(Err(msg)) => Err(GitError::Invalid(msg.clone())),
            None => Ok(None),
        }
    }
}
