//! `aaf upgrade`: fail-closed SHA-256 verified self-updater (human-only).

use std::path::{Path, PathBuf};

use aaf_engine::updater::{
    DEFAULT_REPO, HttpReleaseSource, ReleaseSource, UpdateError, UpdatePlan, check_for_update,
    current_target_triple, download_and_verify, install_verified_binaries,
};
use serde_json::json;

use crate::ctx::{Ctx, current_exe};
use crate::output::{CmdError, CmdOutput, CmdResult, notice};

#[derive(Debug, clap::Args)]
pub struct UpgradeArgs {
    /// Check whether a newer release is available without downloading or modifying binaries.
    #[arg(long)]
    pub check: bool,
    /// Install a specific release tag (e.g. `v0.2.0`) instead of the latest release.
    #[arg(long)]
    pub version: Option<String>,
    /// Download and verify SHA-256 without overwriting installed binaries.
    #[arg(long)]
    pub dry_run: bool,
    /// Destination directory for `aaf` and `aaf-hook` (defaults to the directory of this `aaf` binary).
    #[arg(long)]
    pub dir: Option<PathBuf>,
    /// GitHub repository slug (`owner/repo`).
    #[arg(long, default_value = DEFAULT_REPO)]
    pub repo: String,
}

pub fn run(args: &UpgradeArgs, ctx: &Ctx) -> CmdResult {
    let source = HttpReleaseSource::default();
    run_with_source(args, ctx, &source, env!("CARGO_PKG_VERSION"))
}

/// Testable entry point accepting an injected [`ReleaseSource`] and current version string.
pub fn run_with_source(
    args: &UpgradeArgs,
    ctx: &Ctx,
    source: &dyn ReleaseSource,
    current_version: &str,
) -> CmdResult {
    if let Some(run_id) = &ctx.run_id {
        let reason = format!(
            "AAF_RUN_ID is set ({run_id}): self-upgrade is human-only and refused inside an agent run"
        );
        return Ok(CmdOutput::policy("refused", format!("refused: {reason}"))
            .field("reason", json!(reason)));
    }

    let target = current_target_triple().map_err(map_update_err)?;
    let plan = check_for_update(
        source,
        args.repo.trim(),
        current_version,
        args.version.as_deref(),
        target,
    )
    .map_err(map_update_err)?;

    if args.check {
        return Ok(format_check_output(&plan));
    }

    if plan.is_same && args.version.is_none() && !args.dry_run {
        let text = format!(
            "already up to date: aaf {} ({})",
            plan.current_version, plan.target
        );
        return Ok(attach_plan_fields(CmdOutput::ok("up_to_date", text), &plan));
    }

    let install_dir = resolve_install_dir(args.dir.as_deref())?;
    notice(
        "upgrade",
        &format!("downloading {} and verifying SHA-256...", plan.archive_name),
    );

    let verified = match download_and_verify(source, &plan) {
        Ok(v) => v,
        Err(
            err @ (UpdateError::ChecksumMismatch { .. }
            | UpdateError::MissingChecksum(_)
            | UpdateError::MalformedChecksum(_)
            | UpdateError::UnsafeArchiveEntry(_)),
        ) => {
            let msg = err.to_string();
            return Ok(attach_plan_fields(
                CmdOutput::policy("verification_failed", format!("refused: {msg}"))
                    .field("error", json!(msg)),
                &plan,
            ));
        }
        Err(err) => return Err(map_update_err(err)),
    };

    if args.dry_run {
        let text = format!(
            "dry-run verified: {} -> {} ({})\n  sha256: {}\n  bytes: {}\n  install_dir: {}",
            plan.current_version,
            plan.target_version,
            plan.target,
            verified.sha256,
            verified.bytes.len(),
            install_dir.display()
        );
        return Ok(attach_plan_fields(
            CmdOutput::ok("dry_run_verified", text)
                .field("sha256", json!(verified.sha256))
                .field("archive_bytes", json!(verified.bytes.len()))
                .field("install_dir", json!(install_dir)),
            &plan,
        ));
    }

    let receipt = match install_verified_binaries(&verified, &install_dir) {
        Ok(r) => r,
        Err(err @ UpdateError::UnsafeArchiveEntry(_)) => {
            let msg = err.to_string();
            return Ok(attach_plan_fields(
                CmdOutput::policy("verification_failed", format!("refused: {msg}"))
                    .field("error", json!(msg)),
                &plan,
            ));
        }
        Err(err) => return Err(map_update_err(err)),
    };

    let installed_list = receipt
        .installed_binaries
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let text = format!(
        "upgraded aaf {} -> {} ({})\n  sha256: {}\n  install_dir: {}\n  binaries: {}",
        plan.current_version,
        plan.target_version,
        receipt.target,
        receipt.sha256,
        receipt.install_dir.display(),
        installed_list
    );
    Ok(attach_plan_fields(
        CmdOutput::ok("upgraded", text)
            .field("sha256", json!(receipt.sha256))
            .field("install_dir", json!(receipt.install_dir))
            .field("installed_binaries", json!(receipt.installed_binaries)),
        &plan,
    ))
}

fn resolve_install_dir(explicit: Option<&Path>) -> Result<PathBuf, CmdError> {
    if let Some(dir) = explicit {
        return Ok(dir.to_path_buf());
    }
    let exe = current_exe()
        .ok_or_else(|| CmdError::new("cannot determine current executable path; pass --dir"))?;
    let parent = exe.parent().ok_or_else(|| {
        CmdError::new("cannot determine parent directory of current executable; pass --dir")
    })?;
    Ok(parent.to_path_buf())
}

fn format_check_output(plan: &UpdatePlan) -> CmdOutput {
    let (status, summary) = if plan.is_newer {
        (
            "update_available",
            format!(
                "update available: {} -> {} ({})\n  archive: {}",
                plan.current_version, plan.target_version, plan.target, plan.archive_url
            ),
        )
    } else if plan.is_same {
        (
            "up_to_date",
            format!("up to date: aaf {} ({})", plan.current_version, plan.target),
        )
    } else {
        (
            "version_selected",
            format!(
                "selected version {} is older than current {} ({})",
                plan.target_version, plan.current_version, plan.target
            ),
        )
    };
    attach_plan_fields(CmdOutput::ok(status, summary), plan)
}

fn attach_plan_fields(out: CmdOutput, plan: &UpdatePlan) -> CmdOutput {
    out.field("repo", json!(plan.repo))
        .field("target", json!(plan.target))
        .field("current_version", json!(plan.current_version.to_string()))
        .field("target_version", json!(plan.target_version.to_string()))
        .field("tag", json!(plan.tag))
        .field("is_newer", json!(plan.is_newer))
        .field("is_same", json!(plan.is_same))
        .field("archive_name", json!(plan.archive_name))
        .field("archive_url", json!(plan.archive_url))
        .field("checksums_url", json!(plan.checksums_url))
}

fn map_update_err(err: UpdateError) -> CmdError {
    CmdError::new(format!("upgrade: {err}"))
}
