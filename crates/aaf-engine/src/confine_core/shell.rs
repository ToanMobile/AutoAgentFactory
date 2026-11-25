//! Confinement for host-executed gate, proof-capture, and hydration commands.
//!
//! Keeps `$AAF_DATA_DIR` (signing key, receipts, approvals, DB, factory locks)
//! closed from repo-controlled shell commands while preserving normal repo/temp
//! access and the throttle guest/token paths needed by nested `aaf slot`.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::{Command, Stdio};
#[cfg(target_os = "linux")]
use std::sync::OnceLock;

use super::path;

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Reason recorded in [`ConfineMode::Unconfined`] when no data directory exists
/// yet (no user warning is emitted for this case).
pub(crate) const REASON_NO_DATA_DIR: &str = "no data dir";

/// Confinement backend used for a shell command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfineMode {
    Seatbelt,
    Bwrap,
    Unconfined { reason: String },
}

/// Fail-closed error when a command cannot be confined safely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfineError(pub String);

impl fmt::Display for ConfineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "gate commands cannot be confined: {}", self.0)
    }
}

impl std::error::Error for ConfineError {}

impl From<ConfineError> for io::Error {
    fn from(err: ConfineError) -> Self {
        io::Error::other(err)
    }
}

pub(super) fn bare_sh(command: &str) -> Vec<OsString> {
    vec!["/bin/sh".into(), "-c".into(), command.into()]
}

fn validate_path_str<'a>(path: &'a Path, label: &str) -> Result<&'a str, ConfineError> {
    let text = path
        .to_str()
        .ok_or_else(|| ConfineError(format!("{label} is not valid UTF-8: {path:?}")))?;
    if !path.is_absolute() {
        return Err(ConfineError(format!(
            "{label} must be an absolute path: {text:?}"
        )));
    }
    if text
        .chars()
        .any(|c| c == '"' || c == '\\' || c.is_control())
    {
        return Err(ConfineError(format!(
            "{label} contains forbidden quote, backslash, or control character: {text:?}"
        )));
    }
    Ok(text)
}

fn canonical_validated(path: &Path, label: &str) -> Result<PathBuf, ConfineError> {
    let raw = validate_path_str(path, label)?;
    let canon = path::canonical(raw).map_err(|err| ConfineError(err.to_string()))?;
    let _ = validate_path_str(&canon, label)?;
    Ok(canon)
}

/// Returns `Some(cwd_canon)` when `cwd` is strictly under `<data>/warm-slots/`
/// or `<data>/worktrees/`; `None` when outside `<data>`; `Err` otherwise.
fn check_cwd_vs_data_dir(
    cwd_canon: &Path,
    data_canon: &Path,
) -> Result<Option<PathBuf>, ConfineError> {
    if !cwd_canon.starts_with(data_canon) {
        return Ok(None);
    }
    let warm_slots = data_canon.join("warm-slots");
    let worktrees = data_canon.join("worktrees");
    let allowed = (cwd_canon.starts_with(&warm_slots) && cwd_canon != warm_slots)
        || (cwd_canon.starts_with(&worktrees) && cwd_canon != worktrees);
    if allowed {
        Ok(Some(cwd_canon.to_path_buf()))
    } else {
        Err(ConfineError(format!(
            "working directory {} is inside data dir {}",
            cwd_canon.display(),
            data_canon.display()
        )))
    }
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Extra `ui-service.token` locations outside `data_canon` denied as defense-in-depth (S-UI-1).
fn extra_ui_token_paths(data_canon: &Path) -> BTreeSet<PathBuf> {
    let mut raw_paths = Vec::new();
    if let Some(cfg) = std::env::var_os("AAF_CONFIG_DIR").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(cfg);
        if p.is_absolute() {
            raw_paths.push(p.join("ui-service.token"));
        }
    }
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        let h = PathBuf::from(home);
        if h.is_absolute() {
            raw_paths.push(h.join(".config/autofactory/ui-service.token"));
            raw_paths.push(h.join(".local/share/autofactory/ui-service.token"));
        }
    }
    let mut out = BTreeSet::new();
    for p in raw_paths {
        let Ok(raw) = validate_path_str(&p, "ui-service.token") else {
            continue;
        };
        for resolved in [path::canonical(raw), path::canonical_entry(raw)]
            .into_iter()
            .flatten()
        {
            if !resolved.starts_with(data_canon)
                && validate_path_str(&resolved, "ui-service.token").is_ok()
            {
                out.insert(resolved);
            }
        }
    }
    out
}

/// Builds the macOS Seatbelt SBPL profile for confining a command away from `data_dir`.
pub(super) fn build_seatbelt_profile(data_dir: &Path, cwd: &Path) -> Result<String, ConfineError> {
    let raw_data = validate_path_str(data_dir, "data_dir")?;
    let data_canon = canonical_validated(data_dir, "data_dir")?;
    let cwd_canon = canonical_validated(cwd, "cwd")?;
    let reopen_cwd = check_cwd_vs_data_dir(&cwd_canon, &data_canon)?;

    let mut literal_entries = BTreeSet::new();
    for entry in path::entries_to(data_dir).map_err(|err| ConfineError(err.to_string()))? {
        literal_entries.insert(entry);
    }
    for link in path::links_to(raw_data).map_err(|err| ConfineError(err.to_string()))? {
        for entry in path::entries_to(&link).map_err(|err| ConfineError(err.to_string()))? {
            literal_entries.insert(entry);
        }
    }

    let mut profile = String::from("(version 1)\n(allow default)\n");
    for entry in &literal_entries {
        let text = validate_path_str(entry, "data_dir ancestor")?;
        profile.push_str(&format!("(deny file-write* (literal \"{text}\"))\n"));
    }

    let data_str = validate_path_str(&data_canon, "data_dir")?;
    let escaped_data = path::regex_escape(data_str);
    profile.push_str(&format!(
        "(deny file-read* file-write* (subpath \"{data_str}\"))\n\
         (allow file-read* (subpath \"{data_str}/throttle\"))\n\
         (allow file-read* file-write* (subpath \"{data_str}/throttle/guest\"))\n\
         (deny file-write* (literal \"{data_str}/throttle/guest\"))\n\
         (allow file-read* (regex #\"^{escaped_data}/locks/heavy\\.[0-9]+\\.token$\"))\n"
    ));
    if let Some(reopen) = reopen_cwd {
        let cwd_str = validate_path_str(&reopen, "cwd")?;
        profile.push_str(&format!(
            "(allow file-read* file-write* (subpath \"{cwd_str}\"))\n"
        ));
    }
    for tok in extra_ui_token_paths(&data_canon) {
        if let Ok(tok_str) = validate_path_str(&tok, "ui-service.token") {
            profile.push_str(&format!(
                "(deny file-read* file-write* (literal \"{tok_str}\"))\n"
            ));
        }
    }
    Ok(profile)
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) fn macos_shell_argv(
    command: &str,
    cwd: &Path,
    data_dir: &Path,
    sandbox_exec: &Path,
) -> Result<(Vec<OsString>, ConfineMode), ConfineError> {
    if !is_executable(sandbox_exec) {
        return Err(ConfineError(format!(
            "{} is missing or not executable",
            sandbox_exec.display()
        )));
    }
    let profile = build_seatbelt_profile(data_dir, cwd)?;
    let argv = vec![
        sandbox_exec.as_os_str().to_os_string(),
        "-p".into(),
        profile.into(),
        "--".into(),
        "/bin/sh".into(),
        "-c".into(),
        command.into(),
    ];
    Ok((argv, ConfineMode::Seatbelt))
}

fn check_no_symlink_prefix(path: &Path) -> Result<(), ConfineError> {
    let mut cur = PathBuf::new();
    for comp in path.components() {
        cur.push(comp);
        if matches!(comp, Component::RootDir | Component::CurDir) {
            continue;
        }
        if fs::symlink_metadata(&cur).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(ConfineError(
                "data dir path goes through a symlink; set AAF_DATA_DIR to its resolved path"
                    .to_string(),
            ));
        }
    }
    Ok(())
}

fn is_token_filename(name: &str) -> bool {
    name.strip_prefix("heavy.")
        .and_then(|rest| rest.strip_suffix(".token"))
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

pub(super) fn build_bwrap_argv(
    bwrap_bin: &Path,
    command: &str,
    cwd: &Path,
    data_dir: &Path,
) -> Result<Vec<OsString>, ConfineError> {
    let _ = validate_path_str(data_dir, "data_dir")?;
    let _ = validate_path_str(cwd, "cwd")?;
    check_no_symlink_prefix(data_dir)?;

    let data_canon = canonical_validated(data_dir, "data_dir")?;
    let cwd_canon = canonical_validated(cwd, "cwd")?;
    let reopen_cwd = check_cwd_vs_data_dir(&cwd_canon, &data_canon)?;

    let mut argv: Vec<OsString> = vec![
        bwrap_bin.as_os_str().to_os_string(),
        "--die-with-parent".into(),
        "--dev-bind".into(),
        "/".into(),
        "/".into(),
    ];

    // Pin every ancestor of `data_canon` below `/` in shallow->deep order so
    // `mv` on any parent directory fails with EBUSY.
    let mut ancestors: Vec<&Path> = data_canon
        .ancestors()
        .skip(1)
        .filter(|p| p.parent().is_some())
        .collect();
    ancestors.reverse();
    for ancestor in ancestors {
        let s = validate_path_str(ancestor, "data_dir ancestor")?;
        argv.extend(["--bind".into(), s.into(), s.into()]);
    }

    let data_str = validate_path_str(&data_canon, "data_dir")?;
    argv.extend(["--tmpfs".into(), data_str.into()]);

    let throttle = data_canon.join("throttle");
    if throttle.exists() {
        let s = validate_path_str(&throttle, "throttle dir")?;
        argv.extend(["--ro-bind".into(), s.into(), s.into()]);
    }
    let guest = throttle.join("guest");
    if guest.exists() {
        let s = validate_path_str(&guest, "throttle guest dir")?;
        argv.extend(["--bind".into(), s.into(), s.into()]);
    }
    let locks = data_canon.join("locks");
    if locks.is_dir()
        && let Ok(entries) = fs::read_dir(&locks)
    {
        let mut tokens: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| {
                e.file_type().is_ok_and(|t| t.is_file())
                    && e.file_name().to_str().is_some_and(is_token_filename)
            })
            .map(|e| e.path())
            .collect();
        tokens.sort();
        for tok in tokens {
            let s = validate_path_str(&tok, "token file")?;
            argv.extend(["--ro-bind".into(), s.into(), s.into()]);
        }
    }
    if let Some(reopen) = reopen_cwd
        && reopen.exists()
    {
        let s = validate_path_str(&reopen, "cwd")?;
        argv.extend(["--bind".into(), s.into(), s.into()]);
    }
    for tok in extra_ui_token_paths(&data_canon) {
        if fs::symlink_metadata(&tok).is_ok_and(|m| m.file_type().is_file())
            && let Ok(s) = validate_path_str(&tok, "ui-service.token")
        {
            argv.extend(["--ro-bind".into(), "/dev/null".into(), s.into()]);
        }
    }

    let cwd_str = validate_path_str(&cwd_canon, "cwd")?;
    argv.extend([
        "--chdir".into(),
        cwd_str.into(),
        "--".into(),
        "/bin/sh".into(),
        "-c".into(),
        command.into(),
    ]);
    Ok(argv)
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn linux_shell_argv_with_probe(
    command: &str,
    cwd: &Path,
    data_dir: &Path,
    probe: Result<&Path, &str>,
) -> Result<(Vec<OsString>, ConfineMode), ConfineError> {
    let bwrap_bin = match probe {
        Ok(bin) => bin,
        Err(reason) => {
            return Ok((
                bare_sh(command),
                ConfineMode::Unconfined {
                    reason: reason.to_string(),
                },
            ));
        }
    };
    let argv = build_bwrap_argv(bwrap_bin, command, cwd, data_dir)?;
    Ok((argv, ConfineMode::Bwrap))
}

#[cfg(target_os = "linux")]
fn find_bwrap_binary() -> Option<PathBuf> {
    std::env::var_os("PATH")
        .and_then(|p| path::find_on_path(&p, "bwrap"))
        .or_else(|| {
            ["/usr/bin/bwrap", "/bin/bwrap"]
                .iter()
                .map(PathBuf::from)
                .find(|p| is_executable(p))
        })
}

#[cfg(target_os = "linux")]
fn cached_bwrap_probe() -> &'static Result<PathBuf, String> {
    static PROBE: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    PROBE.get_or_init(|| {
        let Some(bin) = find_bwrap_binary() else {
            return Err("bwrap binary not found on PATH".to_string());
        };
        let status = Command::new(&bin)
            .args(["--dev-bind", "/", "/", "--tmpfs", "/tmp", "--", "/bin/true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        match status {
            Ok(s) if s.success() => Ok(bin),
            Ok(s) => Err(format!(
                "{} probe exited with {:?}",
                bin.display(),
                s.code()
            )),
            Err(err) => Err(format!("{} probe failed: {err}", bin.display())),
        }
    })
}

/// Builds the confined shell argv for executing `command` in `cwd`.
pub(crate) fn shell_argv(
    command: &str,
    cwd: &Path,
    data_dir: Option<&Path>,
) -> Result<(Vec<OsString>, ConfineMode), ConfineError> {
    let Some(data_dir) = data_dir.filter(|d| d.exists()) else {
        return Ok((
            bare_sh(command),
            ConfineMode::Unconfined {
                reason: REASON_NO_DATA_DIR.to_string(),
            },
        ));
    };
    #[cfg(target_os = "macos")]
    {
        macos_shell_argv(command, cwd, data_dir, Path::new(SANDBOX_EXEC))
    }
    #[cfg(target_os = "linux")]
    {
        let probe = cached_bwrap_probe()
            .as_ref()
            .map(PathBuf::as_path)
            .map_err(String::as_str);
        linux_shell_argv_with_probe(command, cwd, data_dir, probe)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (cwd, data_dir);
        Ok((
            bare_sh(command),
            ConfineMode::Unconfined {
                reason: "unsupported OS for gate confinement".to_string(),
            },
        ))
    }
}

/// Probes host gate-command confinement capability for `aaf doctor`.
/// Never touches or creates `data_dir`.
pub fn probe() -> Result<String, String> {
    #[cfg(target_os = "macos")]
    {
        let path = Path::new(SANDBOX_EXEC);
        if is_executable(path) {
            Ok(format!("{SANDBOX_EXEC} (Seatbelt)"))
        } else {
            Err(format!(
                "{SANDBOX_EXEC} is missing; gate commands fail closed"
            ))
        }
    }
    #[cfg(target_os = "linux")]
    {
        match cached_bwrap_probe() {
            Ok(bin) => Ok(format!("bubblewrap ({})", bin.display())),
            Err(reason) => Err(format!(
                "bubblewrap unavailable ({reason}); gate commands run unconfined"
            )),
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err("no gate confinement backend on this OS; gate commands run unconfined".to_string())
    }
}
