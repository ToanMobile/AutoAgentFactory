//! Native `.claude/settings.local.json` repo-account assignment helpers.

use std::fs;
use std::path::Path;

use serde_json::Value;

use super::{MAX_SETTINGS_BYTES, SETTINGS_TMP_SEQ, read_regular_bounded};

pub(in crate::cmd::ui) fn read_repo_local_account(repo_root: &Path) -> Option<String> {
    let claude_dir = repo_root.join(".claude");
    let dmeta = fs::symlink_metadata(&claude_dir).ok()?;
    if dmeta.file_type().is_symlink() || !dmeta.file_type().is_dir() {
        return None;
    }
    let raw =
        read_regular_bounded(&claude_dir.join("settings.local.json"), MAX_SETTINGS_BYTES).ok()?;
    let val: Value = serde_json::from_str(&raw).ok()?;
    val.get("account")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

pub(in crate::cmd::ui) fn write_repo_local_account_native(
    repo_root: &Path,
    account: Option<&str>,
) -> Result<(), String> {
    use std::io::Write;
    let claude_dir = repo_root.join(".claude");
    match fs::symlink_metadata(&claude_dir) {
        Ok(m) if m.file_type().is_symlink() || !m.file_type().is_dir() => {
            return Err("refused: .claude path is a symlink or non-directory".into());
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(&claude_dir).map_err(|err| format!("create .claude failed: {err}"))?;
        }
        Err(e) => return Err(format!("stat .claude failed: {e}")),
    }
    let settings_path = claude_dir.join("settings.local.json");
    let mut obj: serde_json::Map<String, Value> =
        match read_regular_bounded(&settings_path, MAX_SETTINGS_BYTES) {
            Ok(raw) if !raw.trim().is_empty() => serde_json::from_str::<Value>(&raw)
                .ok()
                .and_then(|v| v.as_object().cloned())
                .ok_or_else(|| {
                    "invalid .claude/settings.local.json: expected JSON object".to_string()
                })?,
            Ok(_) => serde_json::Map::new(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::Map::new(),
            Err(e) => return Err(format!("refused: cannot read settings.local.json ({e})")),
        };
    match account {
        Some(acc) => {
            obj.insert("account".into(), Value::String(acc.to_string()));
        }
        None => {
            obj.remove("account");
        }
    }
    let mut body = serde_json::to_string_pretty(&Value::Object(obj)).map_err(|e| e.to_string())?;
    body.push('\n');
    let seq = SETTINGS_TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = claude_dir.join(format!("settings.local.tmp-{}-{seq}", std::process::id()));
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = f.set_permissions(fs::Permissions::from_mode(0o600));
    }
    f.write_all(body.as_bytes()).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    drop(f);
    if let Err(e) = fs::rename(&tmp, &settings_path) {
        let _ = fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    Ok(())
}
