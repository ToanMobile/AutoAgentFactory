//! Linux sandbox capability detection and Bubblewrap (`bwrap`) smoke-probing.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::process::run_with_timeout;

/// Bounded timeout for the `bwrap` smoke probe (`bwrap ... -- /bin/true`).
const BWRAP_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Detected or configured isolation mechanism on Linux hosts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinuxIsolation {
    /// Per-run namespace and mount confinement via `bwrap` (Bubblewrap).
    Bubblewrap { bwrap_bin: PathBuf },
    /// Running inside an OCI/container runtime (Docker, Podman, Kubernetes, containerd, LXC).
    HardenedContainer { indicator: &'static str },
    /// Neither Bubblewrap nor a recognized container runtime was detected.
    None,
}

impl LinuxIsolation {
    /// Returns `true` when at least one Linux isolation layer is active.
    pub fn is_isolated(&self) -> bool {
        !matches!(self, Self::None)
    }

    /// Returns a concise human-readable summary suitable for `aaf doctor`.
    pub fn summary(&self) -> String {
        match self {
            Self::Bubblewrap { bwrap_bin } => format!("bubblewrap ({})", bwrap_bin.display()),
            Self::HardenedContainer { indicator } => format!("hardened container ({indicator})"),
            Self::None => {
                "none (install bubblewrap `bwrap` or run inside a hardened container)".to_string()
            }
        }
    }
}

/// Detects whether the current process appears to be running inside a Linux container.
pub fn detect_container(
    has_dockerenv: bool,
    cgroup_text: Option<&str>,
    env_container: Option<&str>,
) -> Option<&'static str> {
    if has_dockerenv {
        return Some("docker");
    }
    if let Some(trimmed) = env_container.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(match trimmed.to_ascii_lowercase().as_str() {
            "podman" => "podman",
            "docker" => "docker",
            "oci" => "oci",
            "lxc" | "lxd" => "lxc",
            "kubepods" | "kubernetes" => "kubernetes",
            "containerd" => "containerd",
            _ => "container",
        });
    }
    if let Some(lower) = cgroup_text.map(str::to_ascii_lowercase) {
        const MARKERS: [(&str, &str); 6] = [
            ("kubepods", "kubernetes"),
            ("containerd", "containerd"),
            ("docker", "docker"),
            ("podman", "podman"),
            ("libpod", "podman"),
            ("lxc", "lxc"),
        ];
        if let Some((_, label)) = MARKERS.iter().find(|(needle, _)| lower.contains(needle)) {
            return Some(label);
        }
    }
    None
}

/// Resolves the effective [`LinuxIsolation`] from deterministic probe inputs.
pub fn linux_sandbox_status(
    bwrap_bin: Option<PathBuf>,
    has_dockerenv: bool,
    cgroup_text: Option<&str>,
    env_container: Option<&str>,
) -> LinuxIsolation {
    if let Some(bwrap_bin) = bwrap_bin {
        return LinuxIsolation::Bubblewrap { bwrap_bin };
    }
    detect_container(has_dockerenv, cgroup_text, env_container)
        .map_or(LinuxIsolation::None, |indicator| {
            LinuxIsolation::HardenedContainer { indicator }
        })
}

/// Resolves usable Linux isolation after running a `bwrap` smoke probe.
///
/// When `bwrap` exists but fails (for example under Ubuntu 24.04 AppArmor
/// userns restrictions), falls back to an active container runtime if one is
/// detected; otherwise fails closed with a diagnostic error.
pub fn resolve_usable_linux_isolation<F>(
    bwrap_bin: Option<PathBuf>,
    probe: F,
    container: Option<&'static str>,
) -> Result<LinuxIsolation, String>
where
    F: FnOnce(&Path) -> Result<(), String>,
{
    if let Some(bin) = bwrap_bin {
        match probe(&bin) {
            Ok(()) => return Ok(LinuxIsolation::Bubblewrap { bwrap_bin: bin }),
            Err(why) => {
                if let Some(indicator) = container {
                    return Ok(LinuxIsolation::HardenedContainer { indicator });
                }
                return Err(format!(
                    "bubblewrap ({}) failed smoke check: {why} (check AppArmor/unprivileged \
                     userns or pass --unsandboxed)",
                    bin.display()
                ));
            }
        }
    }
    Ok(container.map_or(LinuxIsolation::None, |indicator| {
        LinuxIsolation::HardenedContainer { indicator }
    }))
}

/// Runs a minimal `bwrap` invocation to confirm namespaces and mounts work.
pub fn probe_bwrap(bwrap_bin: &Path) -> Result<(), String> {
    let mut cmd = Command::new(bwrap_bin);
    cmd.args([
        "--die-with-parent",
        "--unshare-pid",
        "--unshare-ipc",
        "--new-session",
        "--ro-bind",
        "/",
        "/",
        "--dev",
        "/dev",
        "--proc",
        "/proc",
        "--",
        "/bin/true",
    ]);
    let out = run_with_timeout(cmd, BWRAP_PROBE_TIMEOUT, true)
        .map_err(|err| format!("spawn failed: {err}"))?;
    match out.status {
        None => Err(format!(
            "timed out after {}s",
            BWRAP_PROBE_TIMEOUT.as_secs()
        )),
        Some(status) if status.success() => Ok(()),
        Some(status) => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let detail = if stderr.is_empty() {
                format!("exit {status}")
            } else {
                stderr
            };
            Err(detail)
        }
    }
}

fn read_host_container() -> Option<&'static str> {
    let cgroup_text = std::fs::read_to_string("/proc/1/cgroup")
        .or_else(|_| std::fs::read_to_string("/proc/self/cgroup"))
        .ok();
    let env_container = std::env::var("container")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            Path::new("/run/.containerenv")
                .exists()
                .then(|| "podman".into())
        });
    detect_container(
        Path::new("/.dockerenv").exists(),
        cgroup_text.as_deref(),
        env_container.as_deref(),
    )
}

/// Probes the current host environment for Linux sandbox capabilities without
/// spawning `bwrap`.
pub fn detect_host_linux_isolation() -> LinuxIsolation {
    if let Some(bwrap_bin) = find_bwrap_binary() {
        return LinuxIsolation::Bubblewrap { bwrap_bin };
    }
    read_host_container().map_or(LinuxIsolation::None, |indicator| {
        LinuxIsolation::HardenedContainer { indicator }
    })
}

/// Probes the current host environment and smoke-tests `bwrap` when present.
pub fn probe_host_linux_isolation() -> Result<LinuxIsolation, String> {
    resolve_usable_linux_isolation(find_bwrap_binary(), probe_bwrap, read_host_container())
}

fn find_bwrap_binary() -> Option<PathBuf> {
    std::env::var_os("PATH")
        .and_then(|p| super::super::find_on_path(&p, "bwrap"))
        .or_else(|| {
            ["/usr/bin/bwrap", "/bin/bwrap"]
                .iter()
                .map(PathBuf::from)
                .find(|p| p.is_file())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usable_isolation_fails_closed_when_bwrap_probe_errors_outside_container() {
        let res = resolve_usable_linux_isolation(
            Some(PathBuf::from("/usr/bin/bwrap")),
            |_| Err("setting up uid map: Permission denied".to_string()),
            None,
        );
        let Err(msg) = res else {
            panic!("expected fail-closed error when bwrap probe fails");
        };
        assert!(msg.contains("AppArmor"), "{msg}");
        assert!(msg.contains("Permission denied"), "{msg}");
    }

    #[test]
    fn usable_isolation_falls_back_to_container_when_nested_bwrap_fails() {
        let res = resolve_usable_linux_isolation(
            Some(PathBuf::from("/usr/bin/bwrap")),
            |_| Err("Operation not permitted".to_string()),
            Some("docker"),
        );
        assert_eq!(
            res,
            Ok(LinuxIsolation::HardenedContainer {
                indicator: "docker"
            })
        );
    }

    #[test]
    fn usable_isolation_returns_bwrap_when_probe_passes() {
        let res = resolve_usable_linux_isolation(
            Some(PathBuf::from("/usr/bin/bwrap")),
            |_| Ok(()),
            Some("docker"),
        );
        assert_eq!(
            res,
            Ok(LinuxIsolation::Bubblewrap {
                bwrap_bin: PathBuf::from("/usr/bin/bwrap")
            })
        );
    }
}
