//! The review phase (I5): exactly two reviewer turns of two different
//! providers on the PR's own diff, from scratch. No stored partial verdict,
//! no `start_dual_review` / `finalize_dual_review`, no adjudication.

use aaf_domain::config::RepoConfig;
use aaf_domain::pipeline::{ReviewerConfig, canonical_provider};
use aaf_fp::PartialAuditRecord;

use super::flow::{Exit, Flow};
use super::lane::Lane;
use super::lock_view::LockView;
use super::verify::Dry;
use crate::factory::Grant;
use crate::factory::audit_backend::adopt_reviewers;
use crate::factory::dual_audit::{
    PartialTurnResult, ReviewInputs, ReviewPolicy, run_reviewer_turn,
};
use crate::factory::ports::Purpose;
use crate::factory::stage_router::StageTarget;
use crate::factory::worker::{GateJob, Next};
use crate::git::TaskWorktree;

const MAX_ISSUES_SHOWN: usize = 5;
const MAX_ISSUE_CHARS: usize = 300;

/// Why a turn that ended without a verdict is to be retried.
fn describe_next(next: &Next) -> String {
    match next {
        Next::Halt(detail) | Next::RetryAfter { detail, .. } => detail.clone(),
        Next::Wait { .. } => "the reviewer's account is cooling down".to_string(),
        Next::Switch => "the reviewer's account was switched".to_string(),
        _ => "the reviewer turn was deferred".to_string(),
    }
}

/// PASS, and no blocking finding.
fn passed(rec: &PartialAuditRecord) -> bool {
    rec.verdict == "PASS" && rec.p0 == 0 && rec.p1 == 0
}

fn finding(rec: &PartialAuditRecord) -> String {
    let mut text = format!(
        "{} ({}): {} (P0 {}, P1 {})",
        rec.reviewer_id, rec.canonical_provider, rec.verdict, rec.p0, rec.p1
    );
    for issue in rec.issues.iter().take(MAX_ISSUES_SHOWN) {
        let detail: String = issue.description.chars().take(MAX_ISSUE_CHARS).collect();
        text.push_str(&format!(
            "\n- {:?} {}: {detail}",
            issue.severity, issue.location
        ));
    }
    text
}

impl Lane<'_> {
    /// The two reviewers: from the base config (so the PR cannot choose its own
    /// reviewers), each with a target wired at startup. Anything that does not
    /// line up cancels rather than falls back to one reviewer.
    fn pick_reviewers(&self, cfg: &RepoConfig) -> Flow<Vec<(ReviewerConfig, &StageTarget)>> {
        let installed: Vec<&str> = self
            .router
            .adopt_reviewers
            .iter()
            .map(|(_, target)| target.executor.as_str())
            .collect();
        let reviewers = adopt_reviewers(cfg, &installed);
        if reviewers.len() != 2 {
            return Err(Exit::abort(
                "needs_human_reviewers",
                format!(
                    "adopting needs two reviewers of different providers; {} could be set up",
                    installed.len().min(reviewers.len())
                ),
            ));
        }
        let mut picked = Vec::new();
        for reviewer in reviewers {
            let Some(target) = self.router.adopt_reviewer(&reviewer.id) else {
                return Err(Exit::abort(
                    "needs_human_reviewers",
                    format!(
                        "reviewer {:?} of the base config has no runner wired in this run",
                        reviewer.id
                    ),
                ));
            };
            picked.push((reviewer, target));
        }
        if canonical_provider(&picked[0].1.executor) == canonical_provider(&picked[1].1.executor) {
            return Err(Exit::abort(
                "needs_human_reviewers",
                "both reviewers resolve to one provider",
            ));
        }
        Ok(picked)
    }

    /// Runs the two turns on the adopt worktree. Both PASS with no P0/P1 =>
    /// `Ok(reviewers)`. A FAIL, a P0/P1 or an inconclusive verdict cancels
    /// (`review_failed`); a turn that gave no verdict (limit, infra, no free
    /// account) is retried.
    pub(super) fn review(
        &self,
        view: &LockView,
        dry: &Dry,
        wt: &TaskWorktree,
    ) -> Flow<Vec<ReviewerConfig>> {
        let picked = self.pick_reviewers(&view.cfg)?;
        let job = GateJob {
            task_id: self.task_id.clone(),
            worktree: wt.clone(),
            audited: false,
            cached_receipt: None,
        };
        let inputs = ReviewInputs {
            policy: ReviewPolicy::from_cross_audit(&view.cfg.cross_audit),
            diff_base: Some(view.merge_base.clone()),
            // The dry run ran the commands, the proof and the oracle; the head
            // lock alone may still block its receipt.
            proof_ok: Some(true),
        };
        let (mut failed, mut deferred) = (Vec::new(), None);
        for (reviewer, target) in &picked {
            let Some(grant) = target.accounts.acquire_for(&self.task_id, Purpose::Audit)? else {
                deferred = Some(format!("no free account for reviewer {}", reviewer.id));
                continue;
            };
            match self.reviewer_turn(target, reviewer, &job, dry, &grant, &inputs) {
                Ok(PartialTurnResult::Completed(rec)) if passed(&rec) => {}
                Ok(PartialTurnResult::Completed(rec)) => failed.push(finding(&rec)),
                Ok(PartialTurnResult::Deferred(next)) => deferred = Some(describe_next(&next)),
                Err(err) => deferred = Some(err.to_string()),
            }
        }
        if !failed.is_empty() {
            return Err(Exit::abort("review_failed", failed.join("\n")));
        }
        if let Some(why) = deferred {
            return Err(Exit::retry("reviewer_deferred", why));
        }
        Ok(picked.into_iter().map(|(reviewer, _)| reviewer).collect())
    }

    fn reviewer_turn(
        &self,
        target: &StageTarget,
        reviewer: &ReviewerConfig,
        job: &GateJob,
        dry: &Dry,
        grant: &Grant,
        inputs: &ReviewInputs,
    ) -> aaf_domain::ports::PortResult<PartialTurnResult> {
        run_reviewer_turn(
            self.deps,
            target,
            reviewer,
            job,
            &dry.receipt,
            "",
            grant,
            inputs,
        )
    }
}
