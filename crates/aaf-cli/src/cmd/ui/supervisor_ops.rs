//! Process spawning, persistence, and repo-allowlist helpers for `UiSupervisor`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ctx::Ctx;

const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct PersistedState {
    pub(super) enabled: bool,
    /// When `Some(list)`, Auto-Run only spawns runners for repos matching `list`
    /// (canonical path or folder name). `None` means all repositories with Ready tasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) allowed_repos: Option<Vec<String>>,
}

pub(super) struct WorkerSlot {
    pub(super) child: Child,
    pub(super) repo_name: String,
}

pub(super) fn normalize_repo_key(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let p = Path::new(trimmed);
    if p.is_absolute() {
        fs::canonicalize(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .to_string_lossy()
            .into_owned()
    } else {
        trimmed.to_string()
    }
}

pub(super) fn is_repo_allowed(allowed: &Option<BTreeSet<String>>, repo_root: &str) -> bool {
    let Some(set) = allowed else {
        return true;
    };
    if set.contains(repo_root) {
        return true;
    }
    let canon = normalize_repo_key(repo_root);
    if set.contains(&canon) {
        return true;
    }
    Path::new(repo_root)
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|name| set.contains(name))
}

pub(super) fn parse_allowed_repos_note(note: &str) -> Result<Option<Vec<String>>, String> {
    let raw = note.trim();
    if raw.eq_ignore_ascii_case("all") || raw == "*" {
        return Ok(None);
    }
    if raw.is_empty() {
        return Ok(Some(Vec::new()));
    }
    if raw.starts_with('{') || raw.starts_with('[') {
        let v: Value =
            serde_json::from_str(raw).map_err(|e| format!("invalid supervisor_repos JSON: {e}"))?;
        if v.get("all").and_then(Value::as_bool) == Some(true) {
            return Ok(None);
        }
        let Some(arr) = v
            .as_array()
            .or_else(|| v.get("repos").and_then(Value::as_array))
        else {
            return Err(
                "invalid supervisor_repos payload: expected {\"all\":true} or {\"repos\":[...]}"
                    .into(),
            );
        };
        return Ok(Some(
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect(),
        ));
    }
    Ok(Some(
        raw.split([',', '\n'])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
    ))
}

pub(super) fn runner_env(ctx: &Ctx) -> Vec<(&'static str, std::ffi::OsString)> {
    let mut env = vec![
        ("PATH", ctx.path_var.clone()),
        ("HOME", ctx.home.as_os_str().to_os_string()),
        ("AAF_DATA_DIR", ctx.data_dir.as_os_str().to_os_string()),
        ("AAF_CONFIG_DIR", ctx.config_dir.as_os_str().to_os_string()),
        (
            "AAF_WORKTREE_DIR",
            ctx.worktrees_dir.as_os_str().to_os_string(),
        ),
    ];
    if let Some((_, val)) = ctx
        .aaf_env
        .iter()
        .find(|(k, v)| k == "AAF_SEED_CACHE" && !v.is_empty())
    {
        env.push(("AAF_SEED_CACHE", std::ffi::OsString::from(val)));
    }
    env
}

pub(super) fn spawn_repo_runner(
    exe: &Path,
    ctx: &Ctx,
    repo_root: &str,
) -> Result<WorkerSlot, String> {
    let repo_name = Path::new(repo_root)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("repo")
        .to_string();
    let log_dir = ctx.data_dir.join("supervisor-logs");
    let _ = fs::create_dir_all(&log_dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&log_dir, fs::Permissions::from_mode(0o700));
    }
    let safe_name: String = repo_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let short_id = &aaf_fp::sha256_hex(repo_root.as_bytes())[..8];
    let log_path = log_dir.join(format!("{safe_name}-{short_id}.log"));
    if fs::metadata(&log_path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let _ = fs::write(&log_path, b"");
    }
    let log_file = OpenOptions::new().create(true).append(true).open(&log_path);
    #[cfg(unix)]
    if let Ok(ref f) = log_file {
        use std::os::unix::fs::PermissionsExt;
        let _ = f.set_permissions(fs::Permissions::from_mode(0o600));
    }
    let mut cmd = Command::new(exe);
    cmd.args(["run"])
        .current_dir(repo_root)
        .stdin(Stdio::null());
    for (k, v) in runner_env(ctx) {
        cmd.env(k, v);
    }
    if let Ok(f) = log_file {
        if let Ok(f2) = f.try_clone() {
            cmd.stdout(Stdio::from(f)).stderr(Stdio::from(f2));
        } else {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    } else {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    let child = cmd.spawn().map_err(|e| e.to_string())?;
    Ok(WorkerSlot { child, repo_name })
}

pub(super) fn stop_children_unlocked(
    workers: BTreeMap<String, WorkerSlot>,
    caffeinate: Option<Child>,
) {
    for (_, mut slot) in workers {
        let pid = slot.child.id();
        let _ = Command::new("/bin/kill")
            .args(["-TERM", &pid.to_string()])
            .stdin(Stdio::null())
            .status();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = slot.child.try_wait() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = slot.child.kill();
        let _ = slot.child.wait();
    }
    if let Some(mut c) = caffeinate {
        let _ = c.kill();
        let _ = c.wait();
    }
}

pub(super) fn read_persisted(path: &Path) -> PersistedState {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub(super) fn write_persisted(path: &Path, state: &PersistedState) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let body = serde_json::to_vec(state).map_err(|e| e.to_string())?;
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp-{}-{seq}", std::process::id()));
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = f.set_permissions(fs::Permissions::from_mode(0o600));
    }
    f.write_all(&body).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    drop(f);
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_repo_allowed_handles_all_empty_and_scoped_sets() {
        assert!(is_repo_allowed(&None, "/Volumes/Data/Toan/OfficeReader"));

        let empty = Some(BTreeSet::new());
        assert!(!is_repo_allowed(&empty, "/Volumes/Data/Toan/OfficeReader"));

        let scoped = Some(BTreeSet::from([
            "/Volumes/Data/Toan/OfficeReader".to_string(),
            "GeelyEx2".to_string(),
        ]));
        assert!(is_repo_allowed(&scoped, "/Volumes/Data/Toan/OfficeReader"));
        assert!(is_repo_allowed(&scoped, "/Volumes/Data/Toan/GeelyEx2"));
        assert!(!is_repo_allowed(
            &scoped,
            "/Volumes/Data/Toan/Idle-Plaza-3D"
        ));
    }
}
