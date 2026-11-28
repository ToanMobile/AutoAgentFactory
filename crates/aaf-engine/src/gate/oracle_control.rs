//! Post-`Proven` verification for the G6 red->green oracle:
//! 1. Re-runs the test command on the reverted tree to detect flaky failures.
//! 2. Reverts the changed tests to `base_sha` on the same scratch worktree and
//!    runs a control pass to verify the failure is attributable to the changed
//!    tests rather than an already-red base suite.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use aaf_domain::config::OracleMode;
use aaf_domain::gate::ChangeKind;
use aaf_domain::oracle_config::OracleCompileRedPolicy;

use super::changes::Change;
use super::oracle::{OracleVerdict, TestFilesEnv, is_oracle_test_path, map_outcome};
use super::oracle_classify::{OracleOutcome, classify};
use super::oracle_worktree::{is_gitlink_at, prepare_safe_write_target, safe_remove};
use super::proc::git_bytes;
use super::runner::HeavyContext;
use super::{GIT_TIMEOUT, GateError};
use crate::throttle::HeavyJob;

const REASON_FLAKY_RERUN: &str = "flaky: changed tests failed on base once, then passed on rerun";
const REASON_RERUN_TIMEOUT: &str = "rerun on base timed out";
const REASON_RERUN_SIGNAL: &str = "rerun on base killed by signal";
const REASON_CONTROL_TIMEOUT: &str = "control run on base timed out";
const REASON_CONTROL_SIGNAL: &str = "control run on base killed by signal";
const REASON_BASE_ALREADY_RED: &str = "warning: oracle: red not attributable to the changed tests \
    (base suite already fails without them); behavioral_red_unproven";

/// Inputs for the post-`Proven` rerun and base control pass.
pub(super) struct ControlContext<'a> {
    pub repo_root: &'a Path,
    pub scratch_path: &'a Path,
    pub base_sha: &'a str,
    pub oracle_mode: OracleMode,
    pub compile_red_policy: OracleCompileRedPolicy,
    pub skip_flake_rerun: bool,
    pub changes: &'a [Change],
    pub inline_rust_tests: &'a BTreeSet<String>,
    pub heavy: &'a HeavyContext<'a>,
    pub job: &'a HeavyJob<'a>,
    pub extra_env: &'a [(&'a str, &'a str)],
    pub env_remove: &'a [&'a str],
}

/// Runs the rerun + base-control verification after the initial reverted run
/// classified as [`OracleOutcome::Proven`].
pub(super) fn verify_proven(ctx: &ControlContext<'_>) -> Result<OracleVerdict, GateError> {
    // 1. Re-run the reverted tree with the exact same job and environment to
    //    detect flaky failures. In `OracleMode::Warn`, skip this redundant pass
    //    when the initial failure already emitted an anchored runner summary
    //    (the pure-base control pass below still proves anti-vacuity).
    if !ctx.skip_flake_rerun {
        let (rerun, _mode) = ctx
            .heavy
            .throttle
            .run_heavy_confined_with_env_and_removals(
                ctx.heavy.data_dir,
                ctx.heavy.clock,
                ctx.job,
                ctx.extra_env,
                ctx.env_remove,
            )
            .map_err(super::runner::map_throttle_err)?;
        let rerun_out = String::from_utf8_lossy(&rerun.output);
        let rerun_outcome = classify(rerun.exit_code, rerun.timed_out, &rerun_out);
        if let Some(inconclusive) = map_rerun_non_proven(rerun_outcome) {
            return Ok(map_outcome(
                ctx.oracle_mode,
                ctx.compile_red_policy,
                inconclusive,
            ));
        }
    }

    // 2. Revert test-side changes (and inline Rust tests) back to `base_sha`.
    let control_files = match revert_test_files_to_base(
        ctx.repo_root,
        ctx.scratch_path,
        ctx.base_sha,
        ctx.changes,
        ctx.inline_rust_tests,
    ) {
        Ok(files) => files,
        Err(err) => {
            return Ok(map_outcome(
                ctx.oracle_mode,
                ctx.compile_red_policy,
                OracleOutcome::Inconclusive(format!("could not prepare control tree: {err}")),
            ));
        }
    };
    let control_env_file = match TestFilesEnv::write(&control_files) {
        Ok(env) => env,
        Err(err) => {
            return Ok(map_outcome(
                ctx.oracle_mode,
                ctx.compile_red_policy,
                OracleOutcome::Inconclusive(format!("could not prepare control tree: {err}")),
            ));
        }
    };
    // Filter `AAF_CHANGED_FILES_FILE` to paths that still exist on the pure-base
    // control tree so newly-added test files removed above are not handed to a
    // runner that executes every listed path directly, while keeping touched
    // modules mapped for diff-aware `test_scoped` scripts.
    let control_changed: BTreeSet<String> = ctx
        .changes
        .iter()
        .filter(|ch| ch.kind != ChangeKind::Deleted && ctx.scratch_path.join(&ch.path).exists())
        .map(|ch| ch.path.clone())
        .collect();
    let control_changed_env = match TestFilesEnv::write(&control_changed) {
        Ok(env) => env,
        Err(err) => {
            return Ok(map_outcome(
                ctx.oracle_mode,
                ctx.compile_red_policy,
                OracleOutcome::Inconclusive(format!("could not prepare control tree: {err}")),
            ));
        }
    };
    let control_path = control_env_file.path_string();
    let control_changed_path = control_changed_env.path_string();
    let control_env: Vec<(&str, &str)> = ctx
        .extra_env
        .iter()
        .map(|&(k, v)| match k {
            "AAF_TEST_FILES_FILE" => (k, control_path.as_str()),
            "AAF_CHANGED_FILES_FILE" => (k, control_changed_path.as_str()),
            _ => (k, v),
        })
        .collect();

    // 3. Run the control pass on the pure base state for all changed paths.
    let (control, _mode) = ctx
        .heavy
        .throttle
        .run_heavy_confined_with_env_and_removals(
            ctx.heavy.data_dir,
            ctx.heavy.clock,
            ctx.job,
            &control_env,
            ctx.env_remove,
        )
        .map_err(super::runner::map_throttle_err)?;

    Ok(map_control_result(
        ctx.oracle_mode,
        ctx.compile_red_policy,
        control.exit_code,
        control.timed_out,
    ))
}

fn map_rerun_non_proven(outcome: OracleOutcome) -> Option<OracleOutcome> {
    match outcome {
        OracleOutcome::Proven => None,
        OracleOutcome::Vacuous => Some(OracleOutcome::Inconclusive(REASON_FLAKY_RERUN.to_string())),
        OracleOutcome::Inconclusive(ref msg) if msg == "timed out" => Some(
            OracleOutcome::Inconclusive(REASON_RERUN_TIMEOUT.to_string()),
        ),
        OracleOutcome::Inconclusive(ref msg) if msg == "killed by signal" => {
            Some(OracleOutcome::Inconclusive(REASON_RERUN_SIGNAL.to_string()))
        }
        OracleOutcome::CompileRed(msg) => Some(OracleOutcome::Inconclusive(format!(
            "rerun on base did not reproduce the failure: compile_red ({msg})"
        ))),
        OracleOutcome::Inconclusive(msg) => Some(OracleOutcome::Inconclusive(format!(
            "rerun on base did not reproduce the failure: {msg}"
        ))),
    }
}

fn map_control_result(
    mode: OracleMode,
    compile_red_policy: OracleCompileRedPolicy,
    exit_code: Option<i32>,
    timed_out: bool,
) -> OracleVerdict {
    if timed_out {
        return map_outcome(
            mode,
            compile_red_policy,
            OracleOutcome::Inconclusive(REASON_CONTROL_TIMEOUT.to_string()),
        );
    }
    match exit_code {
        Some(0) => map_outcome(mode, compile_red_policy, OracleOutcome::Proven),
        None => map_outcome(
            mode,
            compile_red_policy,
            OracleOutcome::Inconclusive(REASON_CONTROL_SIGNAL.to_string()),
        ),
        Some(_) => OracleVerdict::Applied {
            status: None,
            reason: REASON_BASE_ALREADY_RED.to_string(),
        },
    }
}

fn plan_control_reverts<'a>(
    changes: &'a [Change],
    inline_rust_tests: &BTreeSet<String>,
) -> (BTreeMap<&'a str, bool>, BTreeSet<String>) {
    let mut actions: BTreeMap<&'a str, bool> = BTreeMap::new();
    for ch in changes {
        let is_test_change = is_oracle_test_path(&ch.path)
            || ch.old_path.as_deref().is_some_and(is_oracle_test_path);
        let is_inline_rust = inline_rust_tests.contains(&ch.path);
        if !is_test_change && !is_inline_rust {
            continue;
        }
        match ch.kind {
            ChangeKind::Added => {
                actions.entry(ch.path.as_str()).or_insert(false);
            }
            ChangeKind::Modified | ChangeKind::Deleted => {
                actions.insert(ch.path.as_str(), true);
            }
            ChangeKind::Renamed => {
                actions.entry(ch.path.as_str()).or_insert(false);
                if let Some(old) = &ch.old_path {
                    actions.insert(old.as_str(), true);
                }
            }
        }
    }

    let mut control_test_files = BTreeSet::new();
    for (&rel_path, &existed_at_base) in &actions {
        if existed_at_base
            && (is_oracle_test_path(rel_path) || inline_rust_tests.contains(rel_path))
        {
            control_test_files.insert(rel_path.to_string());
        }
    }
    (actions, control_test_files)
}

fn revert_test_files_to_base(
    repo_root: &Path,
    scratch: &Path,
    base_sha: &str,
    changes: &[Change],
    inline_rust_tests: &BTreeSet<String>,
) -> Result<BTreeSet<String>, GateError> {
    let (actions, control_test_files) = plan_control_reverts(changes, inline_rust_tests);
    for (rel_path, existed_at_base) in actions {
        if existed_at_base {
            let base_bytes = match git_bytes(
                repo_root,
                &["show", &format!("{base_sha}:{rel_path}")],
                GIT_TIMEOUT,
            ) {
                Ok(bytes) => bytes,
                Err(_) if is_gitlink_at(repo_root, base_sha, rel_path)? => continue,
                Err(err) => return Err(err),
            };
            let dst = prepare_safe_write_target(scratch, rel_path)?;
            fs::write(&dst, &base_bytes)?;
        } else {
            safe_remove(scratch, rel_path)?;
        }
    }
    Ok(control_test_files)
}

#[cfg(test)]
mod tests {
    use aaf_domain::gate::GateStatus;

    use super::*;

    const TRANSIENT_REASONS: [&str; 2] = [
        "oracle: inconclusive (timed out)",
        "oracle: inconclusive (killed by signal)",
    ];

    fn verdict_reason(v: OracleVerdict) -> (Option<GateStatus>, String) {
        match v {
            OracleVerdict::Applied { status, reason } => (status, reason),
            OracleVerdict::Skipped(note) => {
                panic!("expected Applied verdict, got Skipped({note:?})")
            }
        }
    }

    #[test]
    fn control_reason_strings_never_hit_transient_retry() {
        let policy = OracleCompileRedPolicy::Blocked;
        let rerun_cases = [
            map_rerun_non_proven(OracleOutcome::Vacuous).unwrap(),
            map_rerun_non_proven(OracleOutcome::Inconclusive("timed out".into())).unwrap(),
            map_rerun_non_proven(OracleOutcome::Inconclusive("killed by signal".into())).unwrap(),
            map_rerun_non_proven(OracleOutcome::CompileRed("rust compiler error".into())).unwrap(),
            map_rerun_non_proven(OracleOutcome::Inconclusive("0 tests ran".into())).unwrap(),
        ];

        for mode in [OracleMode::Block, OracleMode::Warn] {
            for outcome in &rerun_cases {
                let (_, reason) = verdict_reason(map_outcome(mode, policy, outcome.clone()));
                for transient in TRANSIENT_REASONS {
                    assert_ne!(
                        reason, transient,
                        "rerun reason in {mode:?} must not match transient string {transient:?}"
                    );
                }
            }
            for (exit_code, timed_out) in [(None, true), (None, false), (Some(1), false)] {
                let (_, reason) =
                    verdict_reason(map_control_result(mode, policy, exit_code, timed_out));
                for transient in TRANSIENT_REASONS {
                    assert_ne!(
                        reason, transient,
                        "control reason in {mode:?} must not match transient string {transient:?}"
                    );
                }
            }
        }

        let (blk_status, blk_reason) = verdict_reason(map_control_result(
            OracleMode::Block,
            policy,
            Some(1),
            false,
        ));
        assert_eq!(blk_status, None);
        assert!(blk_reason.contains("behavioral_red_unproven"));
        assert!(!blk_reason.contains("red_green=proven"));

        let (ok_status, ok_reason) = verdict_reason(map_control_result(
            OracleMode::Block,
            policy,
            Some(0),
            false,
        ));
        assert_eq!(ok_status, None);
        assert_eq!(ok_reason, "oracle: red_green=proven");
    }

    #[test]
    fn plan_control_reverts_handles_renames_and_inline_rust_files() {
        let changes = vec![
            Change {
                kind: ChangeKind::Added,
                path: "tests/test_new.sh".into(),
                old_path: None,
            },
            Change {
                kind: ChangeKind::Modified,
                path: "tests/test_existing.sh".into(),
                old_path: None,
            },
            Change {
                kind: ChangeKind::Deleted,
                path: "tests/test_deleted.sh".into(),
                old_path: None,
            },
            Change {
                kind: ChangeKind::Renamed,
                path: "docs/moved.md".into(),
                old_path: Some("tests/test_moved.sh".into()),
            },
            Change {
                kind: ChangeKind::Renamed,
                path: "tests/test_from_prod.sh".into(),
                old_path: Some("src/old_prod.sh".into()),
            },
            Change {
                kind: ChangeKind::Modified,
                path: "src/lib.rs".into(),
                old_path: None,
            },
            Change {
                kind: ChangeKind::Added,
                path: "src/new_inline.rs".into(),
                old_path: None,
            },
        ];
        let inline: BTreeSet<String> = ["src/lib.rs", "src/new_inline.rs"]
            .into_iter()
            .map(str::to_string)
            .collect();

        let (actions, control_files) = plan_control_reverts(&changes, &inline);
        assert_eq!(actions.get("tests/test_new.sh"), Some(&false));
        assert_eq!(actions.get("tests/test_existing.sh"), Some(&true));
        assert_eq!(actions.get("tests/test_deleted.sh"), Some(&true));
        assert_eq!(actions.get("docs/moved.md"), Some(&false));
        assert_eq!(actions.get("tests/test_moved.sh"), Some(&true));
        assert_eq!(actions.get("tests/test_from_prod.sh"), Some(&false));
        assert_eq!(actions.get("src/old_prod.sh"), Some(&true));
        assert_eq!(actions.get("src/lib.rs"), Some(&true));
        assert_eq!(actions.get("src/new_inline.rs"), Some(&false));

        let expected: BTreeSet<String> = [
            "tests/test_existing.sh",
            "tests/test_deleted.sh",
            "tests/test_moved.sh",
            "src/lib.rs",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        assert_eq!(control_files, expected);
    }
}
