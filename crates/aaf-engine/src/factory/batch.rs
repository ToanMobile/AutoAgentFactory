//! Batched gate (docs/PLAN.md C.11): every task waiting for the gate is
//! merged into one integration worktree and gated once. A red batch is cut
//! in halves until the offending task is alone; the number of gate runs
//! grows with the logarithm of the batch, not with its size.

use std::collections::HashMap;
use std::path::Path;

use aaf_domain::gate::{GateStatus, Receipt};

use super::ports::{GateRunner, GitOps};
use crate::git::TaskWorktree;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub task_id: String,
    pub worktree: TaskWorktree,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The task passed, alone or as part of a clear batch.
    Clear(Receipt),
    /// The task's own gate is blocked (or it fails only in combination).
    Red(Receipt),
    /// The gate could not verify (no test command, tree changed): nothing an
    /// agent retry can fix.
    Unverified(Receipt),
    /// The gate itself could not run.
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judged {
    pub task_id: String,
    pub verdict: Verdict,
    /// The integration tree the verdict comes from (a key of
    /// [`BatchOutcome::groups`]); `None` when the task's own worktree was
    /// gated. Two green halves of a bisected batch are two groups: each was
    /// gated without the other.
    pub batch_group: Option<u32>,
}

/// One integration tree that was gated and whose verdict was spread onto
/// its members.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    /// Base tip the integration tree was built on; `None` when the base
    /// moved while the tree was being built, so nobody can tell.
    pub base: Option<String>,
    /// Branches merged into the tree, in merge order.
    pub branches: Vec<String>,
}

#[derive(Debug, Default)]
pub struct BatchOutcome {
    /// One entry per member, in no particular order.
    pub verdicts: Vec<Judged>,
    pub gate_runs: u32,
    /// Integration worktrees that could not be removed.
    pub warnings: Vec<String>,
    /// Every gated integration tree, by the id its members carry.
    pub groups: HashMap<u32, Group>,
}

/// Runs the gate once on `tree` and maps the receipt/error to a [`Verdict`].
pub(super) fn gate_one(gate: &dyn GateRunner, tree: &Path) -> Verdict {
    match gate.run(tree) {
        Ok(receipt) => match receipt.status {
            GateStatus::Clear => Verdict::Clear(receipt),
            GateStatus::Blocked => Verdict::Red(receipt),
            GateStatus::Unverified => Verdict::Unverified(receipt),
        },
        Err(err) => Verdict::Error(err.to_string()),
    }
}

/// Gates `members` with as few gate runs as possible.
pub fn gate_batch(git: &dyn GitOps, gate: &dyn GateRunner, members: &[Member]) -> BatchOutcome {
    let mut batcher = Batcher {
        git,
        gate,
        out: BatchOutcome::default(),
    };
    batcher.group(members);
    batcher.out
}

struct Batcher<'a> {
    git: &'a dyn GitOps,
    gate: &'a dyn GateRunner,
    out: BatchOutcome,
}

impl Batcher<'_> {
    /// Returns whether every member came out clear.
    fn group(&mut self, members: &[Member]) -> bool {
        match members {
            [] => true,
            // A batch of one is the task's own worktree: no integration
            // tree, and the receipt is the task's own.
            [one] => self.solo(one),
            _ => self.many(members),
        }
    }

    fn run(&mut self, tree: &Path) -> Verdict {
        self.out.gate_runs += 1;
        gate_one(self.gate, tree)
    }

    fn solo(&mut self, member: &Member) -> bool {
        let verdict = self.run(&member.worktree.path);
        let clear = matches!(verdict, Verdict::Clear(_));
        self.out.verdicts.push(Judged {
            task_id: member.task_id.clone(),
            verdict,
            batch_group: None,
        });
        clear
    }

    fn each_alone(&mut self, members: &[Member]) -> bool {
        let mut all = true;
        for member in members {
            all &= self.solo(member);
        }
        all
    }

    fn many(&mut self, members: &[Member]) -> bool {
        let ids: Vec<String> = members.iter().map(|m| m.task_id.clone()).collect();
        let base_before = self.git.base_sha().ok();
        let Ok(integration) = self.git.integrate(&ids) else {
            // No integration tree: the batch is lost, the tasks are not.
            return self.each_alone(members);
        };
        // The tree was cut from the base as it was at some point between
        // these two readings: only when they agree is that base known. An
        // unknown base never lets a merge skip its own gate run.
        let base = base_before.filter(|before| self.git.base_sha().ok().as_ref() == Some(before));
        let (merged, alone): (Vec<Member>, Vec<Member>) = members
            .iter()
            .cloned()
            .partition(|m| integration.merged.contains(&m.task_id));
        let verdict = (merged.len() > 1).then(|| self.run(&integration.tree.path));
        if let Err(err) = self.git.discard(&integration) {
            self.out.warnings.push(format!(
                "integration worktree {} was not removed: {err}",
                integration.tree.path.display()
            ));
        }
        // A branch that conflicted while the batch was built does not sink
        // the batch: it is gated on its own.
        let mut all = self.each_alone(&alone);
        let Some(verdict) = verdict else {
            return self.each_alone(&merged) && all;
        };
        match verdict {
            Verdict::Red(receipt) => {
                all = false;
                self.bisect(&merged, &receipt);
            }
            Verdict::Clear(_) => self.spread(&merged, &verdict, base),
            Verdict::Unverified(_) | Verdict::Error(_) => {
                all = false;
                self.spread(&merged, &verdict, base);
            }
        }
        all
    }

    /// Gives every member the verdict of the integration tree they were
    /// gated in, as a group of its own.
    fn spread(&mut self, members: &[Member], verdict: &Verdict, base: Option<String>) {
        let id = u32::try_from(self.out.groups.len()).unwrap_or(u32::MAX);
        let branches = members.iter().map(|m| m.worktree.branch.clone()).collect();
        self.out.groups.insert(id, Group { base, branches });
        for member in members {
            self.out.verdicts.push(Judged {
                task_id: member.task_id.clone(),
                verdict: verdict.clone(),
                batch_group: Some(id),
            });
        }
    }

    fn bisect(&mut self, merged: &[Member], union: &Receipt) {
        let (left, right) = merged.split_at(merged.len() / 2);
        let left_clear = self.group(left);
        let right_clear = self.group(right);
        if !(left_clear && right_clear) {
            return;
        }
        // Each half passes and the union does not: the halves only fail
        // together. The first half keeps its pass; the second is reported
        // red with the union's receipt so its tasks are reworked against it.
        let mut receipt = union.clone();
        let with: Vec<&str> = left.iter().map(|m| m.task_id.as_str()).collect();
        receipt.reasons.insert(
            0,
            format!("fails only in combination with: {}", with.join(", ")),
        );
        for judged in &mut self.out.verdicts {
            if right.iter().any(|m| m.task_id == judged.task_id) {
                judged.verdict = Verdict::Red(receipt.clone());
            }
        }
    }
}
