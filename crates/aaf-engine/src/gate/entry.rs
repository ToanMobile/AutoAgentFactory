//! Entry points of the gate that fix their own throttle, hidden-file policy or
//! data directory, over [`run_gate_reporting`] and [`run_gate_policy`].

use std::path::Path;

use aaf_domain::config::{MachineConfig, RepoConfig};
use aaf_domain::gate::{GateMode, Receipt};
use aaf_domain::ports::Clock;

use super::{GateError, GateRun, HiddenPolicy, run_gate_policy, run_gate_reporting};
use crate::throttle::{Progress, SystemProbe, Throttle};

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
