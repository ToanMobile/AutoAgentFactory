//! What the adopt lane needs besides [`super::super::Deps`]: the `gh` client
//! and the merge settings. Carried by the `StageRouter` (so `Deps`, `Settings`
//! and `Hooks` keep their shape) and set only where `aaf run` is wired: a router
//! without it leaves `Adopting` tasks alone.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aaf_domain::config::GlobalConfig;
use aaf_domain::gate::GateMode;

use crate::git::{GhClient, MergeStrategy};
use crate::pr_ops::PrMergeRequest;

/// How long the lane waits for the repo merge lock before it retries later.
const LANE_LOCK_TIMEOUT: Duration = Duration::from_secs(60);

pub struct AdoptEnv {
    /// The only way the lane talks to GitHub (view, checks, merge, comment).
    pub gh: Arc<dyn GhClient>,
    /// The request every dry run and merge of the lane starts from. The lane
    /// sets the PR, the pinned head and the adopt flags itself.
    pub request: PrMergeRequest,
}

impl AdoptEnv {
    pub fn new(gh: Arc<dyn GhClient>, request: PrMergeRequest) -> Self {
        Self { gh, request }
    }

    /// The request template of an unattended merge: squash, scoped gate, a
    /// registered check is required (as for the factory's own PR merges), and
    /// no waiting for CI inside the call (the lane's backoff is the wait).
    pub fn request_template(
        repo_root: PathBuf,
        data_dir: PathBuf,
        remote: &str,
        global_config: Option<GlobalConfig>,
        require_checks: bool,
    ) -> PrMergeRequest {
        let machine = global_config
            .as_ref()
            .map(|cfg| cfg.machine.clone())
            .unwrap_or_default();
        PrMergeRequest {
            repo_root,
            data_dir,
            pr_number: 0,
            expect_head: String::new(),
            remote: remote.to_string(),
            mode: GateMode::Scoped,
            strategy: MergeStrategy::Squash,
            subject: None,
            body_file: None,
            ci_wait_secs: 0,
            require_checks,
            lock_timeout: LANE_LOCK_TIMEOUT,
            dry_run: false,
            machine,
            global_config,
            no_receipt_reuse: true,
            run_commands_despite_head_lock: false,
            strict_merged_head: true,
            keep_non_aaf_local: true,
        }
    }
}
