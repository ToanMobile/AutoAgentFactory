//! The signature (C4): once both reviewers passed, the lane signs the lock
//! fingerprint it computed itself, on the base tip and merge-base the dry run
//! used, so the real merge finds an approval it can verify. Never before.

use std::fs;
use std::path::Path;

use aaf_domain::pipeline::{ReviewerConfig, canonical_provider};

use super::flow::{Exit, Flow};
use super::lane::Lane;
use super::lock_view::LockView;
use crate::gate::GateError;
use crate::gate::base::Base;
use crate::gate::head_lock::approve_expecting;
use crate::git::TaskWorktree;

/// True when the approval this lane just signed (`reviewer`, independent, for
/// `lock_fp`) is readable where the merge gate looks for it: under the
/// repository's own root. An older approval of the same lock fingerprint, by
/// another reviewer label, does not count: it would hide a failed mirror write.
fn approval_mirrored(data_dir: &Path, repo_root: &Path, lock_fp: &str, reviewer: &str) -> bool {
    let Ok(key) = aaf_fp::load_key(data_dir) else {
        return false;
    };
    let mut roots = vec![repo_root.to_path_buf()];
    roots.extend(fs::canonicalize(repo_root).ok());
    roots.iter().any(|root| {
        matches!(
            aaf_fp::read_approval(data_dir, &key, root, lock_fp),
            Ok(Some(approval))
                if approval.passes_independent(lock_fp)
                    && !approval.same_account
                    && approval.reviewer == reviewer
        )
    })
}

impl Lane<'_> {
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
        if !approval_mirrored(
            &self.deps.settings.data_dir,
            &self.repo_root,
            lock_fp,
            &reviewer,
        ) {
            return Err(Exit::abort(
                "needs_human_head_lock",
                "the approval could not be stored where the merge gate reads it",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaf_domain::gate::Approval;

    const NEW: &str = "adopt-dual:codex+opencode";

    fn approval(reviewer: &str, same_account: bool, verdict: &str) -> Approval {
        Approval {
            lock_fp: "fp1".to_string(),
            reviewer: reviewer.to_string(),
            verdict: verdict.to_string(),
            same_account,
            created_at: 1,
        }
    }

    fn store(data: &Path, repo: &Path, approval: &Approval) {
        let key = aaf_fp::load_or_create_key(data).unwrap();
        aaf_fp::write_approval(data, &key, repo, approval).unwrap();
    }

    #[test]
    fn only_the_approval_just_signed_counts_as_mirrored() {
        let dirs = tempfile::TempDir::new().unwrap();
        let (data, repo) = (dirs.path().join("data"), dirs.path().join("repo"));
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&data).unwrap();

        // Nothing stored, then an older independent approval by someone else:
        // the mirror write of the lane's own signature is missing or failed.
        assert!(!approval_mirrored(&data, &repo, "fp1", NEW));
        store(&data, &repo, &approval("someone-else", false, "PASS"));
        assert!(!approval_mirrored(&data, &repo, "fp1", NEW));

        // The lane's own signature, but not independent or not a pass.
        store(&data, &repo, &approval(NEW, true, "PASS"));
        assert!(!approval_mirrored(&data, &repo, "fp1", NEW));
        store(&data, &repo, &approval(NEW, false, "FAIL"));
        assert!(!approval_mirrored(&data, &repo, "fp1", NEW));

        // Another fingerprint is no cover either.
        store(&data, &repo, &approval(NEW, false, "PASS"));
        assert!(!approval_mirrored(&data, &repo, "fp2", NEW));
        assert!(approval_mirrored(&data, &repo, "fp1", NEW));
        assert!(!approval_mirrored(
            &data,
            &repo,
            "fp1",
            "adopt-dual:codex+claude"
        ));
    }
}
