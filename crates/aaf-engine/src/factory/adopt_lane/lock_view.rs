//! The lock of the PR as the lane sees it (I4): evaluated on the adopt
//! worktree with the trusted base config, ignoring every approval already on
//! disk, plus the files that only a person may approve.

use aaf_domain::config::RepoConfig;
use aaf_domain::gate::HeadLockReport;

use super::flow::{Exit, Flow};
use super::lane::Lane;
use crate::gate::changes::{self, ChangeSet};
use crate::gate::head_lock::{
    always_human_link_reason, always_human_reason, evaluate_changes_effective,
};
use crate::gate::{GIT_TIMEOUT, GateError, static_checks};
use crate::git::TaskWorktree;
use crate::git::cmd::git;
use crate::pr_ops::load_trusted_base_config;

/// The lock judged at one base tip.
pub(super) struct LockView {
    pub tip: String,
    /// `merge-base(tip, pinned)`: where the PR's own diff starts.
    pub merge_base: String,
    /// The base config at `tip`, tightened by tier 1.
    pub cfg: RepoConfig,
    pub report: HeadLockReport,
}

fn needs_human(why: impl Into<String>) -> Exit {
    Exit::abort("needs_human_head_lock", why)
}

impl Lane<'_> {
    /// Evaluates the lock at `tip`. Fails closed: an evaluation error, a file a
    /// person must approve, a gitlink or a symlink all cancel the adoption.
    pub(super) fn lock_view(&self, tip: &str, wt: &TaskWorktree) -> Flow<LockView> {
        let cfg = load_trusted_base_config(
            &self.repo_root,
            tip,
            self.env.request.global_config.as_ref(),
        )?;
        let merge_base = git(&self.repo_root, &["merge-base", tip, &self.pinned])?;
        let evaluated = (|| -> Result<(HeadLockReport, ChangeSet), GateError> {
            let set = changes::collect(&wt.path, &merge_base, GIT_TIMEOUT)?;
            let added = changes::added_lines(
                &wt.path,
                &merge_base,
                &set.untracked,
                &static_checks::skip_path,
                GIT_TIMEOUT,
            )?;
            // The extra Hard globs come from the tip, the diff from the merge
            // base: the same split the signature uses.
            let (_, extra_hard) = crate::proof::load_gate_base_capture_cmd(&wt.path, Some(tip))
                .map_err(|e| GateError::Config(e.to_string()))?;
            evaluate_changes_effective(
                &self.deps.settings.data_dir,
                &wt.path,
                None,
                &cfg,
                &extra_hard,
                (&merge_base, &set.changes),
                &added,
                cfg.cross_audit.require_different_provider,
            )
        })();
        let (report, effective) = evaluated
            .map_err(|e| needs_human(format!("the lock of the PR could not be evaluated: {e}")))?;
        if let Some(why) = always_human_reason(&effective, &cfg) {
            return Err(needs_human(why));
        }
        match always_human_link_reason(&wt.path, &merge_base, &effective) {
            Ok(None) => {}
            Ok(Some(why)) => return Err(needs_human(why)),
            Err(e) => {
                return Err(needs_human(format!(
                    "gitlinks and symlinks of the PR could not be checked: {e}"
                )));
            }
        }
        Ok(LockView {
            tip: tip.to_string(),
            merge_base,
            cfg,
            report,
        })
    }
}
