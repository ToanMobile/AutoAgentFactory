//! Tailscale Serve discovery and setup helpers for `aaf ui`.
//!
//! `tailscale serve --bg http://127.0.0.1:<port>` is a persistent configuration in
//! `tailscaled` (not a child process tied to `aaf ui`). This module locates the
//! Tailscale CLI (PATH or macOS App Store bundle), reads the machine's MagicDNS
//! hostname, configures `tailscale serve --bg`, and resolves the Tier-1 SSOT
//! `cockpit_url` shared with `aaf notify`.

use std::ffi::OsStr;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use aaf_engine::web_ui::{TrustedOrigin, parse_trusted_origin, render_remote_pairing_card};
use serde_json::{Value, json};

use crate::ctx::Ctx;
use crate::output::CmdOutput;

const MACOS_APP_TAILSCALE: &str = "/Applications/Tailscale.app/Contents/MacOS/Tailscale";
static TMP_SEQ: AtomicU64 = AtomicU64::new(1);

fn is_executable_file(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).is_ok_and(|m| m.is_file() && (m.permissions().mode() & 0o111) != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Finds the `tailscale` CLI on `PATH` or inside macOS `/Applications/Tailscale.app`.
pub(crate) fn find_tailscale_bin(path_var: &OsStr) -> Option<PathBuf> {
    for dir in std::env::split_paths(path_var) {
        if !dir.is_absolute() {
            continue;
        }
        let cand = dir.join("tailscale");
        if is_executable_file(&cand) {
            return Some(cand);
        }
    }
    let mac_app = PathBuf::from(MACOS_APP_TAILSCALE);
    if is_executable_file(&mac_app) {
        return Some(mac_app);
    }
    None
}

/// Parses `Self.DNSName` from `tailscale status --json` into a validated HTTPS origin.
pub(crate) fn parse_dns_origin_from_status_json(raw: &str) -> Result<TrustedOrigin, String> {
    let val: Value = serde_json::from_str(raw)
        .map_err(|e| format!("failed to parse `tailscale status --json`: {e}"))?;
    let dns = val["Self"]["DNSName"]
        .as_str()
        .map(|s| s.trim().trim_end_matches('.'))
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            "Tailscale `Self.DNSName` is empty; enable MagicDNS in https://login.tailscale.com/admin/dns"
                .to_string()
        })?;
    parse_trusted_origin(&format!("https://{dns}"))
}

/// Queries `tailscale status --json` and returns this machine's HTTPS origin.
pub(crate) fn detect_tailscale_origin(bin: &Path) -> Result<TrustedOrigin, String> {
    let out = Command::new(bin)
        .args(["status", "--json"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("failed to run `{}`: {e}", bin.display()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(format!(
            "`tailscale status --json` failed (is Tailscale running & logged in?): {stderr}"
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    parse_dns_origin_from_status_json(&stdout)
}

/// Configures persistent `tailscale serve --bg http://127.0.0.1:<port>` in `tailscaled`.
pub(crate) fn configure_tailscale_serve(bin: &Path, port: u16) -> Result<TrustedOrigin, String> {
    let origin = detect_tailscale_origin(bin)?;
    let target = format!("http://127.0.0.1:{port}");
    let out = Command::new(bin)
        .args(["serve", "--bg", &target])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("failed to run `tailscale serve --bg`: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let hint = if stderr.to_ascii_lowercase().contains("https")
            || stderr.to_ascii_lowercase().contains("cert")
        {
            " — enable HTTPS Certificates in https://login.tailscale.com/admin/dns"
        } else {
            ""
        };
        return Err(format!(
            "`tailscale serve --bg {target}` failed: {stderr}{hint}"
        ));
    }
    Ok(origin)
}

/// Resolves the Tier-1 `notifications.cockpit_url` from `~/.config/autofactory/config.toml`.
pub(crate) fn load_tier1_cockpit_origin(ctx: &Ctx) -> Result<Option<TrustedOrigin>, String> {
    let Ok(cfg) = ctx.load_config(&ctx.config_dir) else {
        return Ok(None);
    };
    let raw = cfg.global.notifications.cockpit_url.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    parse_trusted_origin(raw).map(Some)
}

/// Resolves the effective trusted public origin: explicit CLI `--public-url` wins,
/// otherwise falls back to Tier-1 `notifications.cockpit_url`.
pub(crate) fn resolve_public_origin(
    cli_public_url: Option<&str>,
    ctx: &Ctx,
) -> Result<Option<TrustedOrigin>, String> {
    if let Some(raw) = cli_public_url.map(str::trim).filter(|s| !s.is_empty()) {
        return parse_trusted_origin(raw).map(Some);
    }
    load_tier1_cockpit_origin(ctx)
}

/// Persists `cockpit_url` under `[notifications]` in Tier-1 `config.toml` (only when
/// empty or when explicitly requested via `--tailscale` / `--public-url` on `--install-service`).
pub(crate) fn save_tier1_cockpit_url(
    config_dir: &Path,
    canonical_url: &str,
) -> Result<PathBuf, String> {
    let path = config_dir.join("config.toml");
    fs::create_dir_all(config_dir).map_err(|e| format!("{}: {e}", config_dir.display()))?;
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(format!("refusing to follow symlink: {}", path.display()));
        }
        Ok(meta) if !meta.file_type().is_file() => {
            return Err(format!(
                "refusing non-regular config file: {}",
                path.display()
            ));
        }
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", path.display())),
    }
    let existing = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let updated = upsert_cockpit_url_toml(&existing, canonical_url);
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let stage_dir = config_dir.join(format!(".config.toml.tmp-{}-{seq}", std::process::id()));
    let mut dir_builder = DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        dir_builder.mode(0o700);
    }
    dir_builder
        .create(&stage_dir)
        .map_err(|e| format!("{}: {e}", stage_dir.display()))?;
    let tmp_cfg = stage_dir.join("config.toml");
    let res = (|| -> Result<(), String> {
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts
            .open(&tmp_cfg)
            .map_err(|e| format!("{}: {e}", tmp_cfg.display()))?;
        file.write_all(updated.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|e| format!("{}: {e}", tmp_cfg.display()))?;
        drop(file);
        aaf_engine::config::load_global(&stage_dir)
            .map_err(|e| format!("updated config.toml failed validation: {e}"))?;
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(format!("refusing to follow symlink: {}", path.display()));
        }
        fs::rename(&tmp_cfg, &path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(())
    })();
    let _ = fs::remove_file(&tmp_cfg);
    let _ = fs::remove_dir(&stage_dir);
    res?;
    Ok(path)
}

fn header_without_comment(trimmed: &str) -> Option<&str> {
    if !trimmed.starts_with('[') {
        return None;
    }
    let before_comment = trimmed.split('#').next().unwrap_or(trimmed).trim();
    before_comment.ends_with(']').then_some(before_comment)
}

fn is_notifications_header(header: &str) -> bool {
    !header.starts_with("[[")
        && header
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .is_some_and(|inner| inner.trim() == "notifications")
}

/// Inserts or updates `cockpit_url = "..."` inside `[notifications]` while preserving other settings.
pub(crate) fn upsert_cockpit_url_toml(existing: &str, canonical_url: &str) -> String {
    let escaped = canonical_url.replace('\\', "\\\\").replace('"', "\\\"");
    let quoted = format!("cockpit_url = \"{escaped}\"");
    let mut lines: Vec<String> = existing.lines().map(ToString::to_string).collect();
    let mut in_notif = false;
    let mut notif_end: Option<usize> = None;

    for (idx, line) in lines.iter_mut().enumerate() {
        let trimmed = line.trim();
        if let Some(header) = header_without_comment(trimmed) {
            if in_notif && notif_end.is_none() {
                notif_end = Some(idx);
            }
            in_notif = is_notifications_header(header);
            continue;
        }
        if in_notif
            && trimmed
                .split_once('=')
                .is_some_and(|(k, _)| k.trim() == "cockpit_url")
        {
            *line = quoted;
            let mut out = lines.join("\n");
            if !out.ends_with('\n') {
                out.push('\n');
            }
            return out;
        }
    }
    if in_notif && notif_end.is_none() {
        notif_end = Some(lines.len());
    }
    if let Some(pos) = notif_end {
        lines.insert(pos, quoted);
    } else {
        if !lines.is_empty() && !lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.push(String::new());
        }
        lines.push("[notifications]".to_string());
        lines.push(quoted);
    }
    let mut out = lines.join("\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Resolves public origin from `--tailscale`, `--public-url`, or Tier-1 config,
/// optionally configuring `tailscale serve --bg` and saving Tier-1 `cockpit_url`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn setup_remote_for_service(
    ctx: &Ctx,
    port: u16,
    cli_public_url: Option<&str>,
    use_tailscale: bool,
) -> Result<Option<TrustedOrigin>, String> {
    let origin = if use_tailscale {
        let bin = find_tailscale_bin(&ctx.path_var).ok_or_else(|| {
            "Tailscale CLI not found (install Tailscale from https://tailscale.com/download or `brew install tailscale`)".to_string()
        })?;
        Some(configure_tailscale_serve(&bin, port)?)
    } else if let Some(raw) = cli_public_url.map(str::trim).filter(|s| !s.is_empty()) {
        Some(parse_trusted_origin(raw)?)
    } else {
        load_tier1_cockpit_origin(ctx)?
    };
    if let Some(ref orig) = origin
        && (use_tailscale || cli_public_url.is_some_and(|s| !s.trim().is_empty()))
    {
        save_tier1_cockpit_url(&ctx.config_dir, &orig.canonical)?;
    }
    Ok(origin)
}

/// Attaches remote HTTPS URL, Studio URL, and optional QR card to a `CmdOutput`.
pub(crate) fn attach_remote_output(
    mut out: CmdOutput,
    origin: Option<&TrustedOrigin>,
    token: &str,
    read_only: bool,
    include_qr: bool,
) -> CmdOutput {
    let Some(orig) = origin else {
        return out;
    };
    let remote_url = orig.pairing_url(token);
    let remote_clean = orig.clean_url();
    let remote_studio = orig.studio_url(token);
    let card = render_remote_pairing_card(orig, token, read_only, include_qr);
    out.text.push('\n');
    out.text.push_str(&card);
    out.field("public_url", json!(orig.canonical))
        .field("remote_clean_url", json!(remote_clean))
        .field("remote_url", json!(remote_url))
        .field("remote_studio_url", json!(remote_studio))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_tailscale_status_json_dns_name() {
        let raw = r#"{"Self":{"DNSName":"alex-mac.tail12345.ts.net."}}"#;
        let origin = parse_dns_origin_from_status_json(raw).unwrap();
        assert_eq!(origin.canonical, "https://alex-mac.tail12345.ts.net");
        assert_eq!(origin.port, 443);
    }

    #[test]
    fn rejects_empty_dns_name_with_magicdns_hint() {
        let err = parse_dns_origin_from_status_json(r#"{"Self":{"DNSName":""}}"#).unwrap_err();
        assert!(err.contains("MagicDNS"), "{err}");
    }

    #[test]
    fn upsert_cockpit_url_preserves_existing_sections() {
        let src = "[notifications]\nwebhook_url = \"https://hooks.example\"\n\n[telegram]\nenabled = true\n";
        let out = upsert_cockpit_url_toml(src, "https://mac.tail1.ts.net");
        assert!(out.contains("webhook_url = \"https://hooks.example\""));
        assert!(out.contains("cockpit_url = \"https://mac.tail1.ts.net\""));
        assert!(out.contains("[telegram]\nenabled = true"));
        let replaced = upsert_cockpit_url_toml(&out, "https://mac2.tail1.ts.net");
        assert!(!replaced.contains("mac.tail1.ts.net"));
        assert!(replaced.contains("cockpit_url = \"https://mac2.tail1.ts.net\""));
    }

    #[test]
    fn upsert_cockpit_url_ends_section_before_array_of_tables() {
        let src = "[notifications]\nwebhook_url = \"https://hooks.example\"\n[[cross_audit.reviewers]]\nname = \"r1\"\n";
        let out = upsert_cockpit_url_toml(src, "https://mac.tail1.ts.net");
        let url_pos = out
            .find("cockpit_url = \"https://mac.tail1.ts.net\"")
            .unwrap();
        let arr_pos = out.find("[[cross_audit.reviewers]]").unwrap();
        assert!(
            url_pos < arr_pos,
            "cockpit_url must be inserted before [[cross_audit.reviewers]]:\n{out}"
        );
    }

    #[test]
    fn upsert_cockpit_url_recognizes_commented_header_and_exact_key() {
        let src = "[ notifications ] # remote alerts\ncockpit_url_extra = \"keep-me\"\n";
        let out = upsert_cockpit_url_toml(src, "https://mac.tail1.ts.net");
        assert!(
            out.contains("cockpit_url_extra = \"keep-me\""),
            "must preserve cockpit_url_extra:\n{out}"
        );
        assert!(out.contains("cockpit_url = \"https://mac.tail1.ts.net\""));
        assert_eq!(out.matches("[ notifications ]").count(), 1);
        assert!(!out.contains("\n[notifications]\n"));

        let escaped = upsert_cockpit_url_toml("", "https://a\\b\"c");
        assert!(escaped.contains("cockpit_url = \"https://a\\\\b\\\"c\""));
    }

    #[test]
    fn save_tier1_cockpit_url_refuses_symlinks_and_validates_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg_dir = tmp.path().join("cfg");
        fs::create_dir_all(&cfg_dir).unwrap();

        let saved = save_tier1_cockpit_url(&cfg_dir, "https://mac.tail1.ts.net").unwrap();
        assert_eq!(saved, cfg_dir.join("config.toml"));
        let loaded = aaf_engine::config::load_global(&cfg_dir).unwrap();
        assert_eq!(loaded.notifications.cockpit_url, "https://mac.tail1.ts.net");

        // Invalid TOML / unknown field in existing config must be refused without wiping.
        let bad_dir = tmp.path().join("bad");
        fs::create_dir_all(&bad_dir).unwrap();
        let bad_cfg = bad_dir.join("config.toml");
        fs::write(&bad_cfg, "unknown_top_level_key = 123\n").unwrap();
        let err = save_tier1_cockpit_url(&bad_dir, "https://mac.tail1.ts.net").unwrap_err();
        assert!(err.contains("validation"), "{err}");
        assert_eq!(
            fs::read_to_string(&bad_cfg).unwrap(),
            "unknown_top_level_key = 123\n"
        );

        #[cfg(unix)]
        {
            let target = tmp.path().join("outside.toml");
            fs::write(&target, "").unwrap();
            let sym_dir = tmp.path().join("sym");
            fs::create_dir_all(&sym_dir).unwrap();
            std::os::unix::fs::symlink(&target, sym_dir.join("config.toml")).unwrap();
            let err = save_tier1_cockpit_url(&sym_dir, "https://mac.tail1.ts.net").unwrap_err();
            assert!(err.contains("symlink"), "{err}");
            assert_eq!(fs::read_to_string(&target).unwrap(), "");
        }
    }

    #[cfg(unix)]
    #[test]
    fn find_tailscale_bin_skips_relative_and_non_executable_entries() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let non_exec_dir = tmp.path().join("noexec");
        let exec_dir = tmp.path().join("exec");
        fs::create_dir_all(&non_exec_dir).unwrap();
        fs::create_dir_all(&exec_dir).unwrap();

        let no_x = non_exec_dir.join("tailscale");
        fs::write(&no_x, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&no_x, fs::Permissions::from_mode(0o644)).unwrap();

        let with_x = exec_dir.join("tailscale");
        fs::write(&with_x, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&with_x, fs::Permissions::from_mode(0o755)).unwrap();

        let path_var =
            std::env::join_paths([Path::new("relative/bin"), &non_exec_dir, &exec_dir]).unwrap();
        assert_eq!(find_tailscale_bin(&path_var), Some(with_x));
    }
}
