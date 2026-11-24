//! `aaf upgrade` (alias `aaf update`): fail-closed SHA-256 verified self-updater
//! or local `--from-source` Cargo installer (human-only).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
    /// Download/verify or preview source build without overwriting installed binaries.
    #[arg(long)]
    pub dry_run: bool,
    /// Build and install `aaf` and `aaf-hook` locally via `cargo install --locked`
    /// from an AutoAgentFactory checkout (defaults to current directory when no path is given).
    /// Also auto-activated when run inside an AutoAgentFactory checkout without `--version`/`--check`.
    #[arg(long, num_args = 0..=1, default_missing_value = ".")]
    pub from_source: Option<PathBuf>,
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

    if let Some(source_root) = resolve_source_checkout(args, ctx)? {
        return upgrade_from_source(&source_root, args);
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

    if !plan.is_newer && args.version.is_none() && !args.dry_run {
        let text = if plan.is_same {
            format!(
                "already up to date: aaf {} ({})",
                plan.current_version, plan.target
            )
        } else {
            format!(
                "already up to date: local aaf {} is newer than {} ({})",
                plan.current_version, plan.target_version, plan.target
            )
        };
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

/// Returns `true` when `root` looks like a genuine AutoAgentFactory workspace
/// (contains `Cargo.toml`, `crates/aaf-cli/Cargo.toml`, and `crates/aaf-hook/Cargo.toml`).
pub(crate) fn is_aaf_source_checkout(root: &Path) -> bool {
    let ws = root.join("Cargo.toml");
    let cli = root.join("crates/aaf-cli/Cargo.toml");
    let hook = root.join("crates/aaf-hook/Cargo.toml");
    if !(ws.is_file() && cli.is_file() && hook.is_file()) {
        return false;
    }
    let cli_ok = fs::read_to_string(&cli)
        .ok()
        .is_some_and(|t| t.contains("name = \"aaf-cli\""));
    let hook_ok = fs::read_to_string(&hook)
        .ok()
        .is_some_and(|t| t.contains("name = \"aaf-hook\""));
    cli_ok && hook_ok
}

fn find_aaf_checkout_root(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if is_aaf_source_checkout(dir) {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

fn resolve_source_checkout(args: &UpgradeArgs, ctx: &Ctx) -> Result<Option<PathBuf>, CmdError> {
    if let Some(raw) = &args.from_source {
        let candidate = if raw.is_absolute() {
            raw.clone()
        } else {
            ctx.cwd.join(raw)
        };
        let resolved = find_aaf_checkout_root(&candidate).ok_or_else(|| {
            CmdError::new(format!(
                "upgrade: {} is not an AutoAgentFactory source checkout (missing crates/aaf-cli and crates/aaf-hook)",
                candidate.display()
            ))
        })?;
        return Ok(Some(resolved));
    }
    if args.version.is_none() && !args.check {
        return Ok(find_aaf_checkout_root(&ctx.cwd));
    }
    Ok(None)
}

fn cargo_install_root(dir: Option<&Path>) -> Option<PathBuf> {
    let dir = dir?;
    if dir.file_name().and_then(|n| n.to_str()) == Some("bin") {
        dir.parent().map(Path::to_path_buf)
    } else {
        Some(dir.to_path_buf())
    }
}

fn upgrade_from_source(root: &Path, args: &UpgradeArgs) -> CmdResult {
    let crates = ["crates/aaf-cli", "crates/aaf-hook"];
    let install_root = cargo_install_root(args.dir.as_deref());
    if args.dry_run {
        let mut lines = vec![format!("dry-run source upgrade from {}", root.display())];
        for rel in crates {
            let mut cmd_str = format!("cargo install --path {} --locked", root.join(rel).display());
            if let Some(r) = &install_root {
                cmd_str.push_str(&format!(" --root {}", r.display()));
            }
            lines.push(format!("  would run: {cmd_str}"));
        }
        return Ok(CmdOutput::ok("dry_run_source", lines.join("\n"))
            .field("mode", json!("from_source"))
            .field("source_root", json!(root))
            .field("crates", json!(crates)));
    }

    for rel in crates {
        let crate_path = root.join(rel);
        notice(
            "upgrade",
            &format!("building and installing {}...", crate_path.display()),
        );
        let mut cmd = Command::new("cargo");
        cmd.arg("install")
            .arg("--path")
            .arg(&crate_path)
            .arg("--locked");
        if let Some(r) = &install_root {
            cmd.arg("--root").arg(r);
        }
        let status = cmd
            .status()
            .map_err(|err| CmdError::new(format!("upgrade: failed to run cargo install: {err}")))?;
        if !status.success() {
            return Err(CmdError::new(format!(
                "upgrade: cargo install failed for {} (exit {:?})",
                crate_path.display(),
                status.code()
            )));
        }
    }

    let text = format!(
        "upgraded aaf and aaf-hook from local source ({})\n  all repositories using PATH now run the updated binaries",
        root.display()
    );
    Ok(CmdOutput::ok("upgraded_from_source", text)
        .field("mode", json!("from_source"))
        .field("source_root", json!(root))
        .field("crates", json!(crates)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaf_engine::updater::ReleaseSource;

    struct PanicSource;
    impl ReleaseSource for PanicSource {
        fn latest_tag(&self, _repo: &str) -> Result<String, UpdateError> {
            panic!("should not query remote releases when building from source");
        }
        fn fetch_text(&self, _url: &str) -> Result<String, UpdateError> {
            panic!("should not query remote releases when building from source");
        }
        fn fetch_bytes(&self, _url: &str, _max_bytes: usize) -> Result<Vec<u8>, UpdateError> {
            panic!("should not query remote releases when building from source");
        }
    }

    fn make_fake_aaf_checkout(dir: &Path) {
        fs::create_dir_all(dir.join("crates/aaf-cli")).unwrap();
        fs::create_dir_all(dir.join("crates/aaf-hook")).unwrap();
        fs::write(dir.join("Cargo.toml"), "[workspace]\n").unwrap();
        fs::write(
            dir.join("crates/aaf-cli/Cargo.toml"),
            "[package]\nname = \"aaf-cli\"\n",
        )
        .unwrap();
        fs::write(
            dir.join("crates/aaf-hook/Cargo.toml"),
            "[package]\nname = \"aaf-hook\"\n",
        )
        .unwrap();
    }

    fn base_args() -> UpgradeArgs {
        UpgradeArgs {
            check: false,
            version: None,
            dry_run: true,
            from_source: None,
            dir: None,
            repo: DEFAULT_REPO.to_string(),
        }
    }

    fn make_ctx(cwd: PathBuf, base: &Path, run_id: Option<&str>) -> Ctx {
        Ctx {
            cwd,
            home: base.join("home"),
            data_dir: base.join("data"),
            config_dir: base.join("config"),
            worktrees_dir: base.join("worktrees"),
            path_var: std::ffi::OsString::new(),
            run_id: run_id.map(str::to_string),
            stdin_is_tty: true,
            aaf_env: Vec::new(),
        }
    }

    #[test]
    fn auto_detects_aaf_checkout_from_nested_subdir_in_dry_run() {
        let tmp = tempfile::tempdir().unwrap();
        make_fake_aaf_checkout(tmp.path());
        let nested = tmp.path().join("crates/aaf-cli");
        let ctx = make_ctx(nested, tmp.path(), None);
        let out = run_with_source(&base_args(), &ctx, &PanicSource, "0.1.0").unwrap();
        assert_eq!(out.status, "dry_run_source");
        assert!(out.text.contains("cargo install --path"));
    }

    #[test]
    fn refuses_inside_agent_run_even_with_from_source() {
        let tmp = tempfile::tempdir().unwrap();
        make_fake_aaf_checkout(tmp.path());
        let mut args = base_args();
        args.from_source = Some(tmp.path().to_path_buf());
        let ctx = make_ctx(tmp.path().to_path_buf(), tmp.path(), Some("run-123"));
        let out = run_with_source(&args, &ctx, &PanicSource, "0.1.0").unwrap();
        assert_eq!(out.status, "refused");
        assert_eq!(out.exit, crate::output::Exit::Policy);
    }

    #[test]
    fn rejects_non_aaf_dir_for_explicit_from_source() {
        let tmp = tempfile::tempdir().unwrap();
        let mut args = base_args();
        args.from_source = Some(tmp.path().to_path_buf());
        let ctx = make_ctx(tmp.path().to_path_buf(), tmp.path(), None);
        let err = run_with_source(&args, &ctx, &PanicSource, "0.1.0").unwrap_err();
        assert!(
            err.message
                .contains("not an AutoAgentFactory source checkout")
        );
    }

    struct OlderReleaseSource(&'static str);
    impl ReleaseSource for OlderReleaseSource {
        fn latest_tag(&self, _repo: &str) -> Result<String, UpdateError> {
            Ok(self.0.to_string())
        }
        fn fetch_text(&self, _url: &str) -> Result<String, UpdateError> {
            panic!("must not download checksums when local version is newer");
        }
        fn fetch_bytes(&self, _url: &str, _max_bytes: usize) -> Result<Vec<u8>, UpdateError> {
            panic!("must not download archive when local version is newer");
        }
    }

    #[test]
    fn newer_local_build_is_not_silently_downgraded() {
        let tmp = tempfile::tempdir().unwrap();
        let mut args = base_args();
        args.dry_run = false;
        let ctx = make_ctx(tmp.path().to_path_buf(), tmp.path(), None);
        let out = run_with_source(&args, &ctx, &OlderReleaseSource("v0.4.0"), "0.5.0").unwrap();
        assert_eq!(out.status, "up_to_date");
        assert!(out.text.contains("newer"), "{}", out.text);
    }
}
