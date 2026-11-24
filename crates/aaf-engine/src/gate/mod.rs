//! Verification gate (docs/PLAN.md C.5): static checks on added lines, head
//! lock, store compliance, then the repo's own commands; the outcome is a
//! signed receipt bound to the work tree's fingerprint.

pub mod base;
pub mod changes;
pub mod docs_fast;
pub mod head_lock;
pub mod hidden;
pub mod oracle;
pub mod oracle_classify;
pub(crate) mod oracle_worktree;
pub(crate) mod proc;
pub(crate) mod reuse;
pub mod runner;
pub mod static_checks;
#[cfg(test)]
mod tests;

use std::path::Path;
use std::time::Duration;

use aaf_domain::config::{MachineConfig, RepoConfig};
use aaf_domain::gate::{
    CommandResult, GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION,
    Receipt, StoreComplianceReport, StoreStatus,
};
use aaf_domain::ports::Clock;

use crate::db::private;
use crate::throttle::{Progress, SystemProbe, Throttle};
pub use base::GateRunOptions;
pub use hidden::HiddenPolicy;
use hidden::When;
pub use reuse::{config_hash, repo_root_and_stats_id, stats_repo_id};

/// Budget for one git call of the gate. Larger than `aaf_fp::GIT_TIMEOUT`
/// because fingerprinting or diffing a big tree is not a ten-second job.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(60);

pub const REASON_TREE_CHANGED: &str = "tree changed during gate";

#[derive(Debug, thiserror::Error)]
pub enum GateError {
    #[error(transparent)]
    Fp(#[from] aaf_fp::FpError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("git {0}")]
    Git(String),
    #[error("config: {0}")]
    Config(String),
    #[error("no locked file is changed; there is nothing to approve")]
    NothingLocked,
    #[error("the locked diff changed since it was shown; nothing approved")]
    LockFpChanged,
}

fn mode_name(mode: GateMode) -> &'static str {
    match mode {
        GateMode::Scoped => "scoped",
        GateMode::Full => "full",
        GateMode::Release => "release",
    }
}

/// What the stages found, reduced to what decides the status.
pub(crate) struct Evidence<'a> {
    /// The blocking reasons of [`hidden::inspect`]: files on disk that are
    /// not in the fingerprint and that the policy does not let pass.
    pub hidden: &'a [String],
    pub critical_findings: usize,
    pub head_lock: &'a HeadLockReport,
    pub store: &'a StoreComplianceReport,
    pub proof_block: Option<&'a str>,
    /// `None` when the commands were skipped because an earlier stage blocked.
    pub commands: Option<&'a [CommandResult]>,
    pub test_command_missing: bool,
    pub tree_changed: bool,
    pub mode: GateMode,
}

/// Blocking reasons found before any command runs.
fn early_blocks(evidence: &Evidence<'_>) -> Vec<String> {
    // First: with files nobody signed in the tree, nothing below describes
    // what a command would run on.
    let mut reasons: Vec<String> = evidence.hidden.to_vec();
    if evidence.critical_findings > 0 {
        reasons.push(format!(
            "static checks: {} critical finding(s)",
            evidence.critical_findings
        ));
    }
    if evidence.head_lock.status == LockStatus::Blocked {
        reasons.push(format!(
            "head lock: {} locked file(s) changed without a signed approval",
            evidence.head_lock.locked_changes.len()
        ));
    }
    if evidence.store.status == StoreStatus::Blocked {
        reasons.push(format!(
            "store compliance: {} critical finding(s)",
            evidence.store.critical
        ));
    }
    if let Some(reason) = evidence.proof_block {
        reasons.push(format!("proof required: {reason}"));
    }
    reasons
}

/// `Blocked` wins over `Unverified`; `Clear` needs no reason at all.
pub(crate) fn decide(evidence: &Evidence<'_>) -> (GateStatus, Vec<String>) {
    let mut reasons = early_blocks(evidence);
    match evidence.commands {
        None => reasons.push("commands skipped: an earlier stage already blocks".to_string()),
        Some(results) => {
            for result in results.iter().filter(|result| !result.passed()) {
                reasons.push(if result.timed_out {
                    format!("command {} timed out", result.name)
                } else if let Some(code) = result.exit_code {
                    format!("command {} failed (exit {code})", result.name)
                } else {
                    format!("command {} was killed by a signal", result.name)
                });
            }
        }
    }
    let blocked = !reasons.is_empty();
    let mut unverified = false;
    if evidence.test_command_missing {
        unverified = true;
        reasons.push(format!(
            "no test command is configured for mode {}",
            mode_name(evidence.mode)
        ));
    }
    if evidence.tree_changed {
        unverified = true;
        reasons.push(REASON_TREE_CHANGED.to_string());
    }
    let status = if blocked {
        GateStatus::Blocked
    } else if unverified {
        GateStatus::Unverified
    } else {
        GateStatus::Clear
    };
    (status, reasons)
}

/// A gate verdict and where it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct GateRun {
    pub receipt: Receipt,
    /// True when the receipt is a stored one that already proved what was
    /// asked: no check and no command ran in this call.
    pub reused: bool,
}

/// [`run_gate_reporting`] for callers that only want the receipt.
pub fn run_gate_throttled(
    data_dir: &Path,
    repo_root: &Path,
    cfg: &RepoConfig,
    mode: GateMode,
    clock: &dyn Clock,
    throttle: &Throttle<'_>,
) -> Result<Receipt, GateError> {
    run_gate_reporting(data_dir, repo_root, cfg, mode, clock, throttle).map(|run| run.receipt)
}

/// Runs the gate with the given throttle and stores the signed receipt under
/// `data_dir`. Static checks, head lock, the store scan and a light lint run
/// at once; build and test commands, and a lint that starts a build tool,
/// go through `throttle` ([`runner::is_heavy`]).
pub fn run_gate_reporting(
    data_dir: &Path,
    repo_root: &Path,
    cfg: &RepoConfig,
    mode: GateMode,
    clock: &dyn Clock,
    throttle: &Throttle<'_>,
) -> Result<GateRun, GateError> {
    let policy = HiddenPolicy::default();
    run_gate_policy(data_dir, repo_root, cfg, mode, clock, throttle, policy)
}

/// [`run_gate_reporting`] with a say on files that are on disk and ignored
/// only by a rule the fingerprint does not cover ([`HiddenPolicy`]).
pub fn run_gate_policy(
    data_dir: &Path,
    repo_root: &Path,
    cfg: &RepoConfig,
    mode: GateMode,
    clock: &dyn Clock,
    throttle: &Throttle<'_>,
    policy: HiddenPolicy,
) -> Result<GateRun, GateError> {
    run_gate_with_options(
        data_dir,
        repo_root,
        cfg,
        mode,
        clock,
        throttle,
        GateRunOptions {
            policy,
            base_ref_override: None,
            require_independent_approval: false,
        },
    )
}

/// [`run_gate_reporting`] with explicit [`GateRunOptions`], keeping
/// [`config_hash`] on the unmutated `cfg` while enforcing an optional base
/// override or independent head-lock approval requirement.
pub fn run_gate_with_options(
    data_dir: &Path,
    repo_root: &Path,
    cfg: &RepoConfig,
    mode: GateMode,
    clock: &dyn Clock,
    throttle: &Throttle<'_>,
    opts: GateRunOptions<'_>,
) -> Result<GateRun, GateError> {
    let repo_root = std::fs::canonicalize(repo_root)?;
    let repo_root = repo_root.as_path();
    // Receipts, approvals and the signing key are nobody else's to read.
    private::create_dir(data_dir)?;
    let key = aaf_fp::load_or_create_key(data_dir)?;
    // The fingerprint is `git add -A`: a file git was told to overlook
    // (index flag, ignore rule from outside the tree) is on disk for every
    // command and missing from what the receipt signs. First of all, so
    // that what the policy removes is gone before anything is read.
    let release = mode == GateMode::Release;
    let base_ref = opts.base_ref_override.unwrap_or(&cfg.gate.base_ref);
    let (found, layout, fp_before, base) = match opts.policy {
        HiddenPolicy::Warn => {
            let (hidden_res, base_res, layout_and_fp) = std::thread::scope(|scope| {
                let hidden_handle = scope.spawn(|| {
                    hidden::inspect(repo_root, opts.policy, When::BeforeCommands, release)
                });
                let base_handle = scope.spawn(|| base::resolve(repo_root, base_ref, GIT_TIMEOUT));
                let layout_and_fp =
                    aaf_fp::probe_git_layout(repo_root, GIT_TIMEOUT).map(|layout| {
                        let fp_res = aaf_fp::tree_fp_with_layout(repo_root, &layout, GIT_TIMEOUT);
                        (layout, fp_res)
                    });
                let hidden_res = hidden_handle.join().unwrap_or_else(|_| {
                    Err(GateError::Git("hidden inspect worker panicked".to_string()))
                });
                let base_res = base_handle.join().unwrap_or_else(|_| {
                    Err(GateError::Git("base resolve worker panicked".to_string()))
                });
                (hidden_res, base_res, layout_and_fp)
            });
            let found = hidden_res?;
            let (layout, fp_before_res) = layout_and_fp?;
            let fp_before = fp_before_res?;
            let base = base_res?;
            (found, layout, fp_before, base)
        }
        HiddenPolicy::Remove => {
            let (hidden_res, base_res, layout_res) = std::thread::scope(|scope| {
                let hidden_handle = scope.spawn(|| {
                    hidden::inspect(repo_root, opts.policy, When::BeforeCommands, release)
                });
                let base_handle = scope.spawn(|| base::resolve(repo_root, base_ref, GIT_TIMEOUT));
                let layout_res = aaf_fp::probe_git_layout(repo_root, GIT_TIMEOUT);
                let hidden_res = hidden_handle.join().unwrap_or_else(|_| {
                    Err(GateError::Git("hidden inspect worker panicked".to_string()))
                });
                let base_res = base_handle.join().unwrap_or_else(|_| {
                    Err(GateError::Git("base resolve worker panicked".to_string()))
                });
                (hidden_res, base_res, layout_res)
            });
            let found = hidden_res?;
            let layout = layout_res?;
            let fp_before = aaf_fp::tree_fp_with_layout(repo_root, &layout, GIT_TIMEOUT)?;
            let base = base_res?;
            (found, layout, fp_before, base)
        }
    };
    let (mut hidden, mut hidden_notes) = (found.blocks, found.notes);
    let common_root = layout.common_repo_root();
    let common_repo_id = aaf_fp::repo_id(common_root);
    let cfg_hash = config_hash(cfg);
    let alt_config_hash = reuse::alt_config_hash(cfg, opts.require_independent_approval);
    let mut proof_block = if cfg.proof.required {
        crate::proof::verify_proof(repo_root, data_dir, &cfg.proof, Some(&fp_before))
            .err()
            .map(|e| e.to_string())
    } else {
        None
    };
    let asked = reuse::Asked {
        repo_root,
        common_repo_id: &common_repo_id,
        allow_common_cache: !cfg.proof.required,
        strict_hidden: opts.policy == HiddenPolicy::Remove || release,
        hidden_on_disk: opts.policy == HiddenPolicy::Warn && !hidden_notes.is_empty(),
        require_independent_approval: opts.require_independent_approval,
        tree_fp: &fp_before,
        base_sha: base.sha.as_deref(),
        config_hash: &cfg_hash,
        alt_config_hash: alt_config_hash.as_deref(),
        mode,
    };
    // A file behind an index flag does not move the fingerprint, so an
    // earlier `Clear` receipt for this tree proves nothing about it. Files
    // the policy lets pass are the operator's own environment.
    if hidden.is_empty()
        && proof_block.is_none()
        && let Some(receipt) = reuse::reusable_receipt(data_dir, &key, &asked)
    {
        return Ok(GateRun {
            receipt,
            reused: true,
        });
    }
    let head_sha = base.head_sha;
    let require_independent =
        cfg.cross_audit.require_different_provider || opts.require_independent_approval;

    let (store_res, tracked_added_res, changes_res) = std::thread::scope(|scope| {
        let store_handle = scope.spawn(|| crate::store_compliance::scan(repo_root, cfg, release));
        let diff_handle = scope.spawn(|| {
            changes::diff_added_tracked(
                repo_root,
                &base.diff_target,
                &static_checks::skip_path,
                GIT_TIMEOUT,
            )
        });
        let changes_res = changes::collect(repo_root, &base.diff_target, GIT_TIMEOUT);
        let tracked_added_res = diff_handle
            .join()
            .unwrap_or_else(|_| Err(GateError::Git("diff worker panicked".to_string())));
        let store_res = store_handle
            .join()
            .map_err(|_| GateError::Git("store compliance worker panicked".to_string()));
        (store_res, tracked_added_res, changes_res)
    });
    let set = changes_res?;
    let mut added = tracked_added_res?;
    changes::append_untracked_lines(
        repo_root,
        &mut added,
        &set.untracked,
        &static_checks::skip_path,
    )?;
    let statics = static_checks::scan(&added)?;
    let head_lock = head_lock::evaluate_changes_with_common(
        data_dir,
        repo_root,
        Some(common_root),
        cfg,
        &set.changes,
        &added,
        require_independent,
    )?;
    let store = store_res?;

    let verdict = |hidden: &[String],
                   proof_block: Option<&str>,
                   commands: Option<&[CommandResult]>,
                   tree_changed| {
        decide(&Evidence {
            hidden,
            critical_findings: statics.critical(),
            head_lock: &head_lock,
            store: &store,
            proof_block,
            commands,
            test_command_missing: runner::test_command(&cfg.commands, mode).1.is_empty(),
            tree_changed,
            mode,
        })
    };
    let mut notes = Vec::new();
    let test_command_missing = runner::test_command(&cfg.commands, mode).1.is_empty();
    // With no command result yet, `Blocked` means an earlier stage blocks.
    let unblocked =
        verdict(&hidden, proof_block.as_deref(), Some(&[]), false).0 != GateStatus::Blocked;
    let take_docs_fast_path = unblocked
        && mode == GateMode::Scoped
        && cfg.gate.docs_fast_path
        && !test_command_missing
        && !docs_fast::commands_name_docs(
            &[&cfg.commands.lint, &cfg.commands.test_scoped],
            &set.changes,
        )
        && docs_fast::is_docs_only_in_repo(
            repo_root,
            &set.changes,
            &cfg.gate.head_lock_globs,
            &cfg.gate.docs_fast_path_exclude_globs,
            GIT_TIMEOUT,
        )?;
    let commands = if take_docs_fast_path {
        notes.push(format!(
            "{}skipped lint and test_scoped for {} safe doc file(s)",
            docs_fast::DOCS_FAST_PATH_NOTE,
            set.changes.len()
        ));
        Some(Vec::new())
    } else if unblocked {
        let timeout = Duration::from_secs(cfg.gate.command_timeout_secs);
        let heavy = runner::HeavyContext {
            data_dir,
            stats_repo_id: &common_repo_id,
            clock,
            throttle,
        };
        let diff_env = runner::DiffEnvFiles::write(mode, base.sha.as_deref(), &set.changes)?;
        let env_owned = diff_env.env_pairs();
        let env_refs: Vec<(&str, &str)> = env_owned.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let run = runner::run_all_throttled_with_env(
            repo_root,
            &cfg.commands,
            mode,
            timeout,
            &heavy,
            &env_refs,
        )?;
        notes = run.notes;
        Some(run.results)
    } else {
        None
    };
    if (opts.policy == HiddenPolicy::Remove || release)
        && commands.as_ref().is_some_and(|c| !c.is_empty())
    {
        // A command can leave a file that changes the next run (and a test
        // runner leaves its cache every time): gone before the next gate.
        // A release receipt is not signed beside one either.
        let after = hidden::inspect(repo_root, opts.policy, When::AfterCommands, release)?;
        hidden.extend(after.blocks);
        hidden_notes.extend(after.notes);
    }
    let tree_fp = aaf_fp::tree_fp_with_layout(repo_root, &layout, GIT_TIMEOUT)?;
    if cfg.proof.required && proof_block.is_none() {
        proof_block = crate::proof::verify_proof(repo_root, data_dir, &cfg.proof, Some(&fp_before))
            .err()
            .map(|e| e.to_string());
    }
    let (status, mut reasons) = verdict(
        &hidden,
        proof_block.as_deref(),
        commands.as_deref(),
        tree_fp != fp_before,
    );
    // Waiting for the machine is not a failure: it is recorded, and the
    // status stays what the evidence decided. The same goes for what the
    // hidden-file policy let pass or removed.
    reasons.append(&mut notes);
    reasons.append(&mut hidden_notes);
    let (status, reasons) = if status == GateStatus::Clear
        && !cfg.gate.red_green_oracle.is_off()
        && !take_docs_fast_path
        && let Some(base_sha) = base.sha.as_deref()
    {
        let heavy = runner::HeavyContext {
            data_dir,
            stats_repo_id: &common_repo_id,
            clock,
            throttle,
        };
        let v = oracle::evaluate(repo_root, base_sha, cfg, mode, &set.changes, &added, &heavy)?;
        oracle::apply_verdict(status, reasons, v)
    } else {
        (status, reasons)
    };

    let receipt = Receipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        repo_root: repo_root.to_string_lossy().into_owned(),
        tree_fp,
        head_sha,
        base_sha: base.sha,
        mode,
        status,
        reasons,
        config_hash: cfg_hash,
        commands: commands.unwrap_or_default(),
        static_findings: statics.findings,
        suppressions: statics.suppressions,
        head_lock,
        store_compliance: store,
        created_at: clock.now(),
    };
    reuse::persist_receipt(
        data_dir,
        &key,
        &common_repo_id,
        cfg.proof.required,
        &receipt,
    )?;
    Ok(GateRun {
        receipt,
        reused: false,
    })
}

/// [`run_gate_with`] that also reports what the throttle is doing (waiting
/// for a slot, for memory, for the load; pausing) so the caller can show it.
pub fn run_gate_observed(
    data_dir: &Path,
    repo_root: &Path,
    cfg: &RepoConfig,
    mode: GateMode,
    clock: &dyn Clock,
    machine: &MachineConfig,
    progress: &dyn Fn(&Progress<'_>),
) -> Result<Receipt, GateError> {
    run_gate_observed_reporting(data_dir, repo_root, cfg, mode, clock, machine, progress)
        .map(|run| run.receipt)
}

/// [`run_gate_observed`] that also says whether the receipt was reused.
pub fn run_gate_observed_reporting(
    data_dir: &Path,
    repo_root: &Path,
    cfg: &RepoConfig,
    mode: GateMode,
    clock: &dyn Clock,
    machine: &MachineConfig,
    progress: &dyn Fn(&Progress<'_>),
) -> Result<GateRun, GateError> {
    let probe = SystemProbe::detect();
    let throttle = Throttle::system(machine, &probe, progress);
    run_gate_reporting(data_dir, repo_root, cfg, mode, clock, &throttle)
}

/// [`run_gate_observed`] for a work tree an agent wrote: files ignored only
/// by a rule the fingerprint does not cover are deleted before the commands
/// run and again after ([`HiddenPolicy::Remove`]). Never for a developer's
/// own checkout.
pub fn run_gate_strict(
    data_dir: &Path,
    repo_root: &Path,
    cfg: &RepoConfig,
    mode: GateMode,
    clock: &dyn Clock,
    machine: &MachineConfig,
    progress: &dyn Fn(&Progress<'_>),
) -> Result<Receipt, GateError> {
    let probe = SystemProbe::detect();
    let throttle = Throttle::system(machine, &probe, progress);
    let policy = HiddenPolicy::Remove;
    run_gate_policy(data_dir, repo_root, cfg, mode, clock, &throttle, policy).map(|run| run.receipt)
}

/// Runs the gate under this machine's load limits (docs/PLAN.md C.11).
pub fn run_gate_with(
    data_dir: &Path,
    repo_root: &Path,
    cfg: &RepoConfig,
    mode: GateMode,
    clock: &dyn Clock,
    machine: &MachineConfig,
) -> Result<Receipt, GateError> {
    run_gate_observed(data_dir, repo_root, cfg, mode, clock, machine, &|_| {})
}

/// Runs the gate with the default machine limits and stores the signed
/// receipt under `data_dir`.
pub fn run_gate_in(
    data_dir: &Path,
    repo_root: &Path,
    cfg: &RepoConfig,
    mode: GateMode,
    clock: &dyn Clock,
) -> Result<Receipt, GateError> {
    let machine = MachineConfig::default();
    run_gate_with(data_dir, repo_root, cfg, mode, clock, &machine)
}

/// Fixed entry point (docs/PLAN.md C.5); the data directory comes from the
/// environment.
pub fn run_gate(
    repo_root: &Path,
    cfg: &RepoConfig,
    mode: GateMode,
    clock: &dyn Clock,
) -> Result<Receipt, GateError> {
    run_gate_in(&aaf_fp::data_dir()?, repo_root, cfg, mode, clock)
}
