//! Always-on Cockpit service at a fixed loopback URL.
//!
//! `aaf ui --service` serves the Cockpit on loopback (`127.0.0.1:4317`) with a
//! persistent token stored in `<data_dir>/ui-service.token` (mode 0600) — inside
//! `AAF_DATA_DIR`, which Seatbelt, `aaf-hook` (`data_guard`), and gate/proof
//! confinement all deny reading so confined agents cannot steal the token even
//! when `--allow-actions` is enabled. `POST /api/approve` remains unconditionally
//! locked in `--service` mode.
//!
//! `aaf ui --install-service` (macOS) writes and loads the `launchd` agent
//! `com.autoagentfactory.ui` so the service survives reboots and restarts cleanly
//! after `/aaf-update`.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use aaf_engine::web_ui::{DEFAULT_PORT, generate_token};
use serde_json::json;

use super::studio::studio_url;
use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput, CmdResult};

pub(crate) const SERVICE_LABEL: &str = "com.autoagentfactory.ui";
pub(crate) const TOKEN_FILE: &str = "ui-service.token";
const TOKEN_LEN: usize = 32;
const MAX_TOKEN_READ_BYTES: u64 = 128;
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

static TMP_SEQ: AtomicU64 = AtomicU64::new(1);

/// Path of the persistent service token (inside `AAF_DATA_DIR`, read-denied to agents).
pub(crate) fn token_path(data_dir: &Path) -> PathBuf {
    data_dir.join(TOKEN_FILE)
}

pub(crate) fn valid_token(t: &str) -> bool {
    t.len() == TOKEN_LEN && t.bytes().all(|b| b.is_ascii_hexdigit())
}

fn read_existing_regular_token(path: &Path) -> io::Result<Option<String>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "refusing non-regular service token path: {}",
                path.display()
            ),
        ));
    }
    let file = fs::File::open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let fmeta = file.metadata()?;
        if !fmeta.file_type().is_file() || fmeta.dev() != meta.dev() || fmeta.ino() != meta.ino() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("service token path changed during open: {}", path.display()),
            ));
        }
        if (fmeta.permissions().mode() & 0o777) != 0o600 {
            let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
        }
    }
    let mut s = String::new();
    file.take(MAX_TOKEN_READ_BYTES).read_to_string(&mut s)?;
    let trimmed = s.trim();
    if valid_token(trimmed) {
        Ok(Some(trimmed.to_string()))
    } else {
        Ok(None)
    }
}

fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::symlink_metadata(dir)
            && meta.file_type().is_dir()
            && (meta.permissions().mode() & 0o077) != 0
        {
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        }
    }
    Ok(())
}

fn write_tmp_token(dir: &Path, token: &str) -> io::Result<PathBuf> {
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{TOKEN_FILE}.tmp-{}-{seq}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&tmp)?;
    let written = file
        .write_all(token.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(tmp)
}

fn publish_token_atomic(dir: &Path, path: &Path, token: &str, existed: bool) -> io::Result<String> {
    let tmp = write_tmp_token(dir, token)?;
    if !existed {
        // Race-safe creation: first process wins via hard_link; concurrent callers adopt its token.
        match fs::hard_link(&tmp, path) {
            Ok(()) => {
                let _ = fs::remove_file(&tmp);
                return Ok(token.to_string());
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(&tmp);
                if let Some(winner) = read_existing_regular_token(path)? {
                    return Ok(winner);
                }
                return replace_malformed_token(dir, path, token);
            }
            Err(_) => {
                // Filesystem without hard_link support: fall back to atomic rename.
            }
        }
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(token.to_string())
}

/// Reads the service token, creating it (mode 0600, symlink-safe, race-safe) when missing or malformed.
pub(crate) fn load_or_create_token(config_dir: &Path) -> io::Result<String> {
    let path = token_path(config_dir);
    let existed = match read_existing_regular_token(&path)? {
        Some(t) => return Ok(t),
        None => fs::symlink_metadata(&path).is_ok(),
    };
    ensure_private_dir(config_dir)?;
    let token: String = generate_token().chars().take(TOKEN_LEN).collect();
    if !valid_token(&token) {
        return Err(io::Error::other("generated service token is malformed"));
    }
    publish_token_atomic(config_dir, &path, &token, existed)
}

fn replace_malformed_token(config_dir: &Path, path: &Path, token: &str) -> io::Result<String> {
    let tmp = write_tmp_token(config_dir, token)?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(token.to_string())
}

/// Truncate the launchd service log if it exceeds `MAX_LOG_BYTES` to bound disk usage.
pub(crate) fn trim_log_if_large(home: &Path) {
    let path = log_path(home);
    if let Ok(meta) = fs::symlink_metadata(&path)
        && meta.file_type().is_file()
        && meta.len() > MAX_LOG_BYTES
    {
        let _ = OpenOptions::new().write(true).truncate(true).open(&path);
    }
}

/// Fixed loopback bookmark URL for the read-only service.
pub(crate) fn bookmark_url(port: u16, token: &str) -> String {
    format!("http://127.0.0.1:{port}/#token={token}")
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn plist_path(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{SERVICE_LABEL}.plist"))
}

pub(crate) fn log_path(home: &Path) -> PathBuf {
    home.join("Library/Logs/aaf-ui.log")
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Deduplicates colon-separated `PATH` entries while preserving order, preventing plist churn.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn normalize_path_var(raw: &str) -> String {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for part in raw.split(':') {
        let trimmed = part.trim();
        if !trimmed.is_empty() && seen.insert(trimmed) {
            out.push(trimmed);
        }
    }
    out.join(":")
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn service_working_dir(home: &Path, extra_env: &[(&str, String)]) -> PathBuf {
    extra_env
        .iter()
        .find(|(k, v)| *k == "AAF_DATA_DIR" && !v.trim().is_empty())
        .map(|(_, v)| PathBuf::from(v))
        .unwrap_or_else(|| home.join(".local/share/autofactory"))
}

/// Launchd agent definition that runs `<exe> ui --service` at login and keeps it alive.
/// The bearer token is NEVER embedded in the plist (argv/env are exposed via `ps`/`launchctl`).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn plist_xml(
    exe: &Path,
    home: &Path,
    path_var: &str,
    extra_env: &[(&str, String)],
    allow_actions: bool,
) -> String {
    let exe = xml_escape(&exe.display().to_string());
    let work_dir = xml_escape(&service_working_dir(home, extra_env).display().to_string());
    let log = xml_escape(&log_path(home).display().to_string());
    let path_var = xml_escape(&normalize_path_var(path_var));
    let mut env_xml = format!("\t\t<key>PATH</key>\n\t\t<string>{path_var}</string>");
    for (k, v) in extra_env {
        let ek = xml_escape(k);
        let ev = xml_escape(v);
        env_xml.push_str(&format!("\n\t\t<key>{ek}</key>\n\t\t<string>{ev}</string>"));
    }
    let actions_arg = if allow_actions {
        "\n\t\t<string>--allow-actions</string>"
    } else {
        ""
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{SERVICE_LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
		<string>ui</string>
		<string>--service</string>{actions_arg}
	</array>
	<key>EnvironmentVariables</key>
	<dict>
{env_xml}
	</dict>
	<key>WorkingDirectory</key>
	<string>{work_dir}</string>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>ThrottleInterval</key>
	<integer>10</integer>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#
    )
}

pub(super) use super::service_env::non_default_service_env;
#[cfg(test)]
pub(super) use super::service_env::non_default_service_env_with_toolchain;

/// Actionable error message when binding `port` fails with `AddrInUse`.
pub(crate) fn port_in_use_message(port: u16, service: bool, config_dir: &Path) -> String {
    if service {
        format!(
            "port {port} is already in use by another program; the read-only Cockpit service \
             cannot start. Free the port (`lsof -nP -iTCP:{port} -sTCP:LISTEN`); launchd retries automatically"
        )
    } else if port == DEFAULT_PORT && token_path(config_dir).exists() {
        format!(
            "port {port} is already in use, likely by the always-on read-only Cockpit service \
             (`aaf ui --install-service`); print its link with `aaf ui --service-url`. \
             For actions (rework/unblock/approve), start a separate instance: `aaf ui --port 4319`"
        )
    } else {
        format!(
            "port {port} is already in use by another program \
             (`lsof -nP -iTCP:{port} -sTCP:LISTEN`); pick another with `aaf ui --port <n>`"
        )
    }
}

#[cfg(target_os = "macos")]
fn launchctl(args: &[&str]) -> io::Result<std::process::Output> {
    std::process::Command::new("/bin/launchctl")
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
}

#[cfg(target_os = "macos")]
fn gui_domain() -> Result<String, String> {
    let out = std::process::Command::new("/usr/bin/id")
        .arg("-u")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("failed to run /usr/bin/id -u: {e}"))?;
    let uid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || uid.is_empty() || !uid.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("invalid uid from /usr/bin/id -u: '{uid}'"));
    }
    Ok(format!("gui/{uid}"))
}

#[cfg(target_os = "macos")]
fn service_loaded(target: &str) -> bool {
    launchctl(&["print", target]).is_ok_and(|o| o.status.success())
}

#[cfg(target_os = "macos")]
fn write_file_atomic(path: &Path, content: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("invalid path: {}", path.display()))?;
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".{SERVICE_LABEL}.tmp-{}-{seq}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    if let Err(e) = f
        .write_all(content.as_bytes())
        .and_then(|()| f.sync_all())
        .and_then(|()| fs::rename(&tmp, path))
    {
        let _ = fs::remove_file(&tmp);
        return Err(format!("{}: {e}", path.display()));
    }
    Ok(())
}

/// Writes the launchd agent plist atomically and loads or restarts it cleanly.
#[cfg(target_os = "macos")]
pub(crate) fn install(
    home: &Path,
    path_var: &str,
    extra_env: &[(&str, String)],
    allow_actions: bool,
) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let plist = plist_path(home);
    if let Some(dir) = plist.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    if let Some(dir) = log_path(home).parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = fs::create_dir_all(service_working_dir(home, extra_env));
    trim_log_if_large(home);

    let xml = plist_xml(&exe, home, path_var, extra_env, allow_actions);
    let unchanged = fs::read_to_string(&plist).ok().as_deref() == Some(xml.as_str());
    if !unchanged {
        write_file_atomic(&plist, &xml)?;
    }

    let domain = gui_domain()?;
    let target = format!("{domain}/{SERVICE_LABEL}");
    let _ = launchctl(&["enable", &target]);
    if unchanged && service_loaded(&target) {
        let out = launchctl(&["kickstart", "-k", &target])
            .map_err(|e| format!("launchctl kickstart: {e}"))?;
        if out.status.success() {
            return Ok(plist);
        }
    }

    if service_loaded(&target) {
        let _ = launchctl(&["bootout", &target]);
        for _ in 0..20 {
            if !service_loaded(&target) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    let plist_s = plist.display().to_string();
    let mut last_err = String::new();
    for attempt in 0..3 {
        let out =
            launchctl(&["bootstrap", &domain, &plist_s]).map_err(|e| format!("launchctl: {e}"))?;
        if out.status.success() {
            return Ok(plist);
        }
        last_err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if attempt < 2 {
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }
    Err(format!(
        "launchctl bootstrap {domain} {plist_s} failed: {last_err}"
    ))
}

/// Unloads and removes the launchd agent.
#[cfg(target_os = "macos")]
pub(crate) fn uninstall(home: &Path) -> Result<PathBuf, String> {
    let plist = plist_path(home);
    if let Ok(domain) = gui_domain() {
        let target = format!("{domain}/{SERVICE_LABEL}");
        let _ = launchctl(&["bootout", &target]);
    }
    match fs::remove_file(&plist) {
        Ok(()) => Ok(plist),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(plist),
        Err(e) => Err(format!("{}: {e}", plist.display())),
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn install(
    _home: &Path,
    _path_var: &str,
    _extra_env: &[(&str, String)],
    _allow_actions: bool,
) -> Result<PathBuf, String> {
    Err(
        "--install-service is only supported on macOS (launchd); run `aaf ui --service` under your own supervisor"
            .into(),
    )
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn uninstall(_home: &Path) -> Result<PathBuf, String> {
    Err("--uninstall-service is only supported on macOS (launchd)".into())
}

/// Unload and remove the launchd agent, returning a formatted [`CmdOutput`].
pub(crate) fn run_uninstall(ctx: &Ctx) -> CmdResult {
    let plist = uninstall(&ctx.home).map_err(CmdError::new)?;
    Ok(CmdOutput::ok(
        "ok",
        format!("uninstalled {SERVICE_LABEL} ({})", plist.display()),
    )
    .field("label", json!(SERVICE_LABEL))
    .field("plist", json!(plist.display().to_string())))
}

/// Loads `<data_dir>/ui-service.token`, migrating a legacy `<config_dir>/ui-service.token`
/// atomically (mode 0600) and removing the legacy file so agents cannot read it.
pub(crate) fn load_or_migrate_token(
    data_dir: &Path,
    legacy_config_dir: &Path,
) -> io::Result<String> {
    let dest = token_path(data_dir);
    let is_regular = |p: &Path| fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_file());
    if let Some(existing) = read_existing_regular_token(&dest)? {
        let legacy = token_path(legacy_config_dir);
        if legacy != dest && is_regular(&legacy) {
            let _ = fs::remove_file(&legacy);
        }
        return Ok(existing);
    }
    let legacy = token_path(legacy_config_dir);
    if legacy != dest
        && is_regular(&legacy)
        && let Ok(Some(migrated)) = read_existing_regular_token(&legacy)
    {
        ensure_private_dir(data_dir)?;
        let dest_existed = fs::symlink_metadata(&dest).is_ok();
        let saved = publish_token_atomic(data_dir, &dest, &migrated, dest_existed)?;
        let _ = fs::remove_file(&legacy);
        return Ok(saved);
    }
    load_or_create_token(data_dir)
}

/// Print the fixed bookmark URL for the Cockpit service (plus remote HTTPS URL & QR if configured).
pub(crate) fn run_service_url(
    ctx: &Ctx,
    port: u16,
    public_url: Option<&str>,
    include_qr: bool,
) -> CmdResult {
    let token = load_or_migrate_token(&ctx.data_dir, &ctx.config_dir)
        .map_err(|e| CmdError::new(format!("service token: {e}")))?;
    let origin = super::tailscale::resolve_public_origin(public_url, ctx).map_err(CmdError::new)?;
    let url = bookmark_url(port, &token);
    let studio = studio_url(&url);
    let token_file = token_path(&ctx.data_dir);
    let out = CmdOutput::ok(
        "ok",
        format!("Cockpit (read-only): {url}\nHealth (monitoring): {studio}"),
    )
    .field("url", json!(url))
    .field("health_url", json!(studio))
    .field("studio_url", json!(studio))
    .field("token_file", json!(token_file.display().to_string()))
    .field("read_only", json!(true));
    Ok(super::tailscale::attach_remote_output(
        out,
        origin.as_ref(),
        &token,
        true,
        include_qr,
    ))
}

/// Create the persistent token, install/restart launchd, and print the bookmark URL.
pub(crate) fn run_install(
    ctx: &Ctx,
    allow_actions: bool,
    public_url: Option<&str>,
    use_tailscale: bool,
    include_qr: bool,
) -> CmdResult {
    let token = load_or_migrate_token(&ctx.data_dir, &ctx.config_dir)
        .map_err(|e| CmdError::new(format!("service token: {e}")))?;
    let origin =
        super::tailscale::setup_remote_for_service(ctx, DEFAULT_PORT, public_url, use_tailscale)
            .map_err(CmdError::new)?;
    let path_var = ctx.path_var.to_string_lossy();
    let extra_env = non_default_service_env(ctx);
    let plist = install(&ctx.home, &path_var, &extra_env, allow_actions).map_err(CmdError::new)?;
    let url = bookmark_url(DEFAULT_PORT, &token);
    let studio = studio_url(&url);
    let log = log_path(&ctx.home);
    let token_file = token_path(&ctx.data_dir);
    let mode_lbl = if allow_actions {
        "board actions enabled, approve locked"
    } else {
        "read-only"
    };
    let out = CmdOutput::ok(
        "ok",
        format!(
            "installed {SERVICE_LABEL} ({})\nCockpit ({mode_lbl}): {url}\nHealth (monitoring): {studio}\nLog: {}",
            plist.display(),
            log.display()
        ),
    )
    .field("label", json!(SERVICE_LABEL))
    .field("plist", json!(plist.display().to_string()))
    .field("log", json!(log.display().to_string()))
    .field("token_file", json!(token_file.display().to_string()))
    .field("url", json!(url))
    .field("health_url", json!(studio))
    .field("studio_url", json!(studio))
    .field("read_only", json!(!allow_actions))
    .field("allow_actions", json!(allow_actions))
    .field("allow_approve", json!(false));
    Ok(super::tailscale::attach_remote_output(
        out,
        origin.as_ref(),
        &token,
        !allow_actions,
        include_qr,
    ))
}
