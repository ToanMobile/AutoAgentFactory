//! Verification gate (docs/PLAN.md C.5): static checks on added lines, head
//! lock, store compliance, then the repo's own commands; the outcome is a
//! signed receipt bound to the work tree's fingerprint.

pub mod autofix;
pub mod base;
pub mod changes;
pub mod confine;
pub mod docs_fast;
pub mod head_lock;
mod helpers;
pub mod hidden;
pub mod oracle;
pub mod oracle_classify;
mod oracle_control;
pub(crate) mod oracle_worktree;
pub(crate) mod proc;
pub mod proof_need;
pub(crate) mod reuse;
pub mod runner;
pub mod rust_scope;
pub mod static_checks;
#[cfg(test)]
mod tests;

pub use crate::confine_core::GIT_TIMEOUT;
use crate::throttle::{Progress, SystemProbe, Throttle};
use aaf_domain::config::{MachineConfig, RepoConfig};
use aaf_domain::gate::{CommandResult, GateMode, GateStatus, RECEIPT_SCHEMA_VERSION, Receipt};
use aaf_domain::ports::Clock;
use aaf_fp::fs_private as private;
pub use base::GateRunOptions;
pub use helpers::CommandTimeouts;
pub(crate) use helpers::{Evidence, decide};
pub use hidden::HiddenPolicy;
use hidden::When;
pub use reuse::{config_hash, repo_root_and_stats_id, stats_repo_id};
use std::path::Path;

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
    #[error(transparent)]
    Confine(#[from] confine::ConfineError),
    #[error("no locked file is changed; there is nothing to approve")]
    NothingLocked,
    #[error("the locked diff changed since it was shown; nothing approved")]
    LockFpChanged,
}

/// A gate verdict and where it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct GateRun {
    pub receipt: Receipt,
    /// True when the receipt is a stored one that already proved what was
    /// asked: no check and no command ran in this call.
    pub reused: bool,
    /// Set when gate commands ran unconfined (e.g. Linux without `bwrap`).
    pub unconfined: Option<String>,
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

/// Runs the gate with `throttle` and stores the signed receipt under `data_dir`.
/// Heavy commands ([`runner::is_heavy`]) go through `throttle`.
pub fn run_gate_reporting(
    data_dir: &Path,
    repo_root: &Path,
    cfg: &RepoConfig,
    mode: GateMode,
    clock: &dyn Clock,
    throttle: &Throttle<'_>,
) -> Result<GateRun, GateError> {
    run_gate_policy(
        data_dir,
        repo_root,
        cfg,
        mode,
        clock,
        throttle,
        HiddenPolicy::default(),
    )
}

/// [`run_gate_reporting`] with an explicit [`HiddenPolicy`].
pub fn run_gate_policy(
    data_dir: &Path,
    repo_root: &Path,
    cfg: &RepoConfig,
    mode: GateMode,
    clock: &dyn Clock,
    throttle: &Throttle<'_>,
    policy: HiddenPolicy,
) -> Result<GateRun, GateError> {
    let opts = GateRunOptions {
        policy,
        ..GateRunOptions::default()
    };
    run_gate_with_options(data_dir, repo_root, cfg, mode, clock, throttle, opts)
}

/// [`run_gate_reporting`] with explicit [`GateRunOptions`].
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
    // Inspect hidden files first so policy removals happen before fingerprinting.
    let release = mode == GateMode::Release;
    let base_ref = opts.base_ref_override.unwrap_or(&cfg.gate.base_ref);
    let (found, layout, fp_before, base) = {
        let (hidden_res, base_res, layout_and_fp) = std::thread::scope(|scope| {
            let hidden_handle = scope
                .spawn(|| hidden::inspect(repo_root, opts.policy, When::BeforeCommands, release));
            let base_handle = scope.spawn(|| base::resolve(repo_root, base_ref, GIT_TIMEOUT));
            let layout_and_fp = aaf_fp::probe_git_layout(repo_root, GIT_TIMEOUT).map(|layout| {
                let fp = (opts.policy == HiddenPolicy::Warn)
                    .then(|| aaf_fp::tree_fp_with_layout(repo_root, &layout, GIT_TIMEOUT));
                (layout, fp)
            });
            let hidden_res = hidden_handle
                .join()
                .unwrap_or_else(|_| Err(GateError::Git("hidden inspect worker panicked".into())));
            let base_res = base_handle
                .join()
                .unwrap_or_else(|_| Err(GateError::Git("base resolve worker panicked".into())));
            (hidden_res, base_res, layout_and_fp)
        });
        let found = hidden_res?;
        let (layout, fp_opt) = layout_and_fp?;
        let fp_before = match fp_opt {
            Some(res) => res?,
            None => aaf_fp::tree_fp_with_layout(repo_root, &layout, GIT_TIMEOUT)?,
        };
        (found, layout, fp_before, base_res?)
    };
    let (mut hidden, mut hidden_notes) = (found.blocks, found.notes);
    let common_root = layout.common_repo_root();
    let common_repo_id = aaf_fp::repo_id(common_root);
    let cfg_hash = config_hash(cfg);
    let alt_config_hash = reuse::alt_config_hash(cfg, opts.require_independent_approval);
    let base_flows = if cfg.proof.required {
        crate::proof::load_gate_base_flows(repo_root, base.sha.as_deref())
            .map_err(|e| GateError::Config(e.to_string()))?
    } else {
        Vec::new()
    };
    let pre_proof_err = if cfg.proof.required {
        crate::proof::verify_gate_pre_proof(
            repo_root,
            data_dir,
            &cfg.proof,
            &fp_before,
            base.sha.as_deref(),
            &base_flows,
        )
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
    // earlier `Clear` receipt for this tree proves nothing about it.
    if hidden.is_empty()
        && pre_proof_err.is_none()
        && let Some(receipt) = reuse::reusable_receipt(data_dir, &key, &asked)
    {
        return Ok(GateRun {
            receipt,
            reused: true,
            unconfined: None,
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
    let (base_capture_cmd, base_capture_lock_globs) =
        crate::proof::load_gate_base_capture_cmd(repo_root, base.sha.as_deref())
            .map_err(|e| GateError::Config(e.to_string()))?;
    let head_lock = head_lock::evaluate_changes_with_common(
        data_dir,
        repo_root,
        Some(common_root),
        cfg,
        &base_capture_lock_globs,
        &set.changes,
        &added,
        require_independent,
    )?;
    let mut store = store_res?;
    let changed_paths: Vec<String> = set
        .changes
        .iter()
        .flat_map(|ch| std::iter::once(ch.path.clone()).chain(ch.old_path.iter().cloned()))
        .collect();
    if mode == GateMode::Scoped {
        store = crate::store_compliance::scope_to_diff(store, &changed_paths);
    }
    let proof_needed = crate::proof::proof_triggered(&cfg.proof, &changed_paths);
    if !proof_needed
        && pre_proof_err.is_some()
        && hidden.is_empty()
        && head_lock.status != aaf_domain::gate::LockStatus::Blocked
        && let Some(receipt) = reuse::reusable_receipt(data_dir, &key, &asked)
    {
        return Ok(GateRun {
            receipt,
            reused: true,
            unconfined: None,
        });
    }
    let has_base_capture = proof_needed && pre_proof_err.is_some() && base_capture_cmd.is_some();
    let has_base_proof = has_base_capture || (proof_needed && !base_flows.is_empty());
    let pre_cmd_proof_block = (proof_needed && !has_base_proof)
        .then_some(pre_proof_err.as_deref())
        .flatten();

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
    let mut unconfined = None;
    let test_command_missing = runner::test_command(&cfg.commands, mode).1.is_empty();
    // With no command result yet, `Blocked` means an earlier stage blocks.
    let unblocked =
        verdict(&hidden, pre_cmd_proof_block, Some(&[]), false).0 != GateStatus::Blocked;
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
    // Snapshot ignored-file cleanliness BEFORE commands run to block self-deleting bypass files.
    let clean_ignored_before =
        opts.allow_common_receipt_write || reuse::worktree_has_no_ignored_files(repo_root);
    let commands = if take_docs_fast_path {
        notes.push(format!(
            "{}skipped lint and test_scoped for {} safe doc file(s)",
            docs_fast::DOCS_FAST_PATH_NOTE,
            set.changes.len()
        ));
        Some(Vec::new())
    } else if unblocked {
        let heavy = runner::HeavyContext {
            data_dir,
            stats_repo_id: &common_repo_id,
            clock,
            throttle,
        };
        let diff_env = runner::DiffEnvFiles::write(mode, base.sha.as_deref(), &set.changes)?;
        let env_owned = diff_env.env_pairs();
        let mut env_refs: Vec<(&str, &str)> =
            env_owned.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let env_remove: &[&str] = if opts.strip_compiler_wrappers {
            env_refs.extend_from_slice(runner::COMPILER_WRAPPER_DISABLE_ENV);
            runner::COMPILER_WRAPPER_ENV_VARS
        } else {
            &[]
        };
        let _gate_secrets =
            crate::git::hydration::stage_for_gate_from_base(repo_root, base.sha.as_deref())
                .map_err(|e| GateError::Git(e.to_string()))?;
        if data_dir.is_dir()
            && let Err(err) =
                crate::throttle::slots::prepare_for_guests(data_dir, throttle.ceiling(), &|pgid| {
                    throttle.control.group_state(pgid)
                })
            && err.kind() != std::io::ErrorKind::PermissionDenied
        {
            return Err(GateError::Io(err));
        }
        let timeouts = helpers::CommandTimeouts::from_secs(
            cfg.gate.command_timeout_secs,
            cfg.gate.idle_timeout_secs,
        );
        let commands_cfg =
            rust_scope::expanded_or(&cfg.commands, mode, repo_root, &set.changes, &mut notes);
        let run = runner::run_all_throttled_with_timeouts(
            repo_root,
            &commands_cfg,
            mode,
            timeouts,
            &heavy,
            &env_refs,
            env_remove,
        )?;
        notes.extend(run.notes);
        unconfined = run.unconfined;
        Some(run.results)
    } else {
        None
    };
    if (opts.policy == HiddenPolicy::Remove || release)
        && commands.as_ref().is_some_and(|c| !c.is_empty())
    {
        // Re-check hidden files after commands so caches/artifacts cannot persist or poison releases.
        let after = hidden::inspect(repo_root, opts.policy, When::AfterCommands, release)?;
        hidden.extend(after.blocks);
        hidden_notes.extend(after.notes);
    }
    let mut tree_fp = aaf_fp::tree_fp_with_layout(repo_root, &layout, GIT_TIMEOUT)?;
    let pre_capture_status = verdict(&hidden, None, commands.as_deref(), tree_fp != fp_before).0;
    let proof_ctx = crate::proof::GateProofCheck {
        repo_root,
        data_dir,
        layout: &layout,
        cfg: &cfg.proof,
        base_sha: base.sha.as_deref(),
        base_flows: &base_flows,
        proof_secret_allowlist: opts.proof_secret_allowlist,
        fp_before: &fp_before,
        policy: opts.policy,
        release,
        clock,
    };
    let proof_block = crate::proof::finalize_gate_proof(
        &proof_ctx,
        proof_needed,
        pre_proof_err.as_deref(),
        has_base_proof,
        pre_capture_status,
        &mut tree_fp,
        &mut hidden,
        &mut hidden_notes,
    )?;
    let (status, mut reasons) = verdict(
        &hidden,
        proof_block.as_deref(),
        commands.as_deref(),
        tree_fp != fp_before,
    );
    // Record throttle and hidden-file notes without altering the evidence verdict.
    reasons.append(&mut notes);
    reasons.append(&mut hidden_notes);
    let (status, reasons) = if status == GateStatus::Clear && !cfg.gate.red_green_oracle.is_off() {
        if take_docs_fast_path {
            reasons.push("note: oracle skipped (docs fast path)".to_string());
            (status, reasons)
        } else if let Some(base_sha) = base.sha.as_deref() {
            let heavy = runner::HeavyContext {
                data_dir,
                stats_repo_id: &common_repo_id,
                clock,
                throttle,
            };
            let v = oracle::evaluate_with_options(
                repo_root,
                base_sha,
                &rust_scope::expanded_cfg(cfg, mode, repo_root, &set.changes),
                mode,
                &set.changes,
                &added,
                &heavy,
                opts.strip_compiler_wrappers,
            )?;
            oracle::apply_verdict(status, reasons, v)
        } else {
            reasons.push("note: oracle skipped (no base commit)".to_string());
            (status, reasons)
        }
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
    let write_common = opts.allow_common_receipt_write
        || (clean_ignored_before && reuse::worktree_has_no_ignored_files(repo_root));
    reuse::persist_receipt(
        data_dir,
        &key,
        &common_repo_id,
        cfg.proof.required,
        write_common,
        &receipt,
    )?;
    Ok(GateRun {
        receipt,
        reused: false,
        unconfined,
    })
}

/// [`run_gate_with`] that also reports throttle progress to the caller.
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

/// [`run_gate_observed`] with [`HiddenPolicy::Remove`] for agent worktrees (never developer checkouts).
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
    run_gate_policy(
        data_dir,
        repo_root,
        cfg,
        mode,
        clock,
        &throttle,
        HiddenPolicy::Remove,
    )
    .map(|run| run.receipt)
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

/// Runs the gate with default machine limits and stores the signed receipt under `data_dir`.
pub fn run_gate_in(
    data_dir: &Path,
    repo_root: &Path,
    cfg: &RepoConfig,
    mode: GateMode,
    clock: &dyn Clock,
) -> Result<Receipt, GateError> {
    run_gate_with(
        data_dir,
        repo_root,
        cfg,
        mode,
        clock,
        &MachineConfig::default(),
    )
}

/// Fixed entry point (docs/PLAN.md C.5); the data directory comes from the environment.
pub fn run_gate(
    repo_root: &Path,
    cfg: &RepoConfig,
    mode: GateMode,
    clock: &dyn Clock,
) -> Result<Receipt, GateError> {
    run_gate_in(&aaf_fp::data_dir()?, repo_root, cfg, mode, clock)
}

/// Compiles a lightweight predicate checking whether a repo-relative path matches
/// any configured `gate.head_lock_globs` (case-insensitive for non-test globs).
pub fn head_lock_path_matcher(globs: &[String]) -> impl Fn(&str) -> bool {
    let mut b = globset::GlobSetBuilder::new();
    for g in globs {
        let ci = !aaf_domain::config::TEST_FILE_GLOBS.contains(&g.as_str());
        if let Ok(pat) = globset::GlobBuilder::new(g)
            .literal_separator(true)
            .case_insensitive(ci)
            .build()
        {
            b.add(pat);
        }
    }
    let set = b.build().ok();
    move |path: &str| set.as_ref().is_some_and(|s| s.is_match(path))
}
