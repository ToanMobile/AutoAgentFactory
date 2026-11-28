//! The signature (C4): once both reviewers passed, the lane signs the lock
//! fingerprint it computed itself, on the base tip and merge-base the dry run
//! used, so the real merge finds an approval it can verify. Never before.

use std::fs;

use aaf_domain::pipeline::{ReviewerConfig, canonical_provider};

use super::flow::{Exit, Flow};
use super::lane::Lane;
use super::lock_view::LockView;
use crate::gate::GateError;
use crate::gate::base::Base;
use crate::gate::head_lock::approve_expecting;
use crate::git::TaskWorktree;

impl Lane<'_> {
    /// True when the signed approval for `lock_fp` is readable where the merge
    /// gate looks for it: under the repository's own root.
    fn approval_mirrored(&self, lock_fp: &str) -> bool {
        let data_dir = &self.deps.settings.data_dir;
        let Ok(key) = aaf_fp::load_key(data_dir) else {
            return false;
        };
        let mut roots = vec![self.repo_root.clone()];
        roots.extend(fs::canonicalize(&self.repo_root).ok());
        roots.iter().any(|root| {
            matches!(
                aaf_fp::read_approval(data_dir, &key, root, lock_fp),
                Ok(Some(approval)) if approval.passes_independent(lock_fp)
            )
        })
    }

    pub(super) fn sign(
        &self,
        view: &LockView,
        reviewers: &[ReviewerConfig],
        lock_fp: &str,
        wt: &TaskWorktree,
    ) -> Flow<()> {
        let providers: Vec<String> = reviewers
            .iter()
            .map(|r| canonical_provider(&r.executor))
            .collect();
        let reviewer = format!("adopt-dual:{}", providers.join("+"));
        let base = Base {
            sha: Some(view.tip.clone()),
            diff_target: view.merge_base.clone(),
            head_sha: Some(self.pinned.clone()),
        };
        match approve_expecting(
            &self.deps.settings.data_dir,
            &wt.path,
            &view.cfg,
            &base,
            (&reviewer, false),
            &*self.deps.clock,
            lock_fp,
        ) {
            Ok(_) => {}
            Err(GateError::LockFpChanged | GateError::NothingLocked) => {
                return Err(Exit::abort(
                    "lock_changed",
                    "the locked files changed between the review and the signature",
                ));
            }
            Err(other) => return Err(Exit::retry("sign_error", other.to_string())),
        }
        if !self.approval_mirrored(lock_fp) {
            return Err(Exit::abort(
                "needs_human_head_lock",
                "the approval could not be stored where the merge gate reads it",
            ));
        }
        Ok(())
    }
}
