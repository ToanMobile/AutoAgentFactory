//! Linux Bubblewrap (`bwrap`) and hardened-container sandbox support.
//!
//! This module is intentionally cross-platform pure Rust (no Linux-only
//! syscalls and no `unsafe`) so its policy builder, container detector, and
//! tests run identically on macOS and Linux.

mod bwrap;
mod detect;
mod ro_overlay;

use std::path::Path;

use aaf_domain::ports::{PortError, PortResult};

use super::pty::CommandSpec;
use super::sandbox::SandboxPaths;

pub use bwrap::build_bwrap_args;
pub use detect::{
    LinuxIsolation, detect_container, detect_host_linux_isolation, linux_sandbox_status,
    probe_bwrap, probe_host_linux_isolation, resolve_usable_linux_isolation,
};

/// Rewrites `command` to execute under Bubblewrap (`bwrap_bin`) with `paths`.
pub fn wrap(
    bwrap_bin: &Path,
    paths: &SandboxPaths,
    command: &CommandSpec,
) -> PortResult<CommandSpec> {
    let bwrap_str = bwrap_bin
        .to_str()
        .ok_or_else(|| PortError::Invalid("bwrap binary path is not valid UTF-8".to_string()))?;
    bwrap::validate_abs_path(bwrap_str, "bwrap_bin").map_err(PortError::Invalid)?;
    let args =
        build_bwrap_args(paths, &command.program, &command.args).map_err(PortError::Invalid)?;
    Ok(CommandSpec {
        program: bwrap_str.to_string(),
        args,
        cwd: command.cwd.clone(),
        env: command.env.clone(),
        env_clear: command.env_clear,
    })
}
