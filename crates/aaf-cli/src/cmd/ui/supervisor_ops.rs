//! Process spawning, persistence, and repo-allowlist helpers for `UiSupervisor`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use aaf_domain::ports::{Clock, KvStore, META_COST_PAUSED_UNTIL};
use aaf_engine::clock::SystemClock;
use aaf_engine::db::Db;
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
    pub(super) spawned_at: Instant,
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

/// Returns `true` iff `META_COST_PAUSED_UNTIL` is still in the future AND the
/// repository's effective 24h `daily_token_cap` is still reached. Checking live
/// config + 24h usage avoids supervisor spin-loops during a cost pause while
/// waking immediately if the user raises or disables `daily_token_cap`.
pub(super) fn is_repo_cost_paused(ctx: &Ctx, db: &Db, repo_root: &str) -> bool {
    is_repo_cost_paused_at(ctx, db, repo_root, SystemClock.now())
}

pub(super) fn is_repo_cost_paused_at(ctx: &Ctx, db: &Db, repo_root: &str, now: i64) -> bool {
    let paused_until = db
        .get_meta(META_COST_PAUSED_UNTIL)
        .ok()
        .flatten()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0);
    if paused_until <= now {
        return false;
    }
    let Ok(cfg) = ctx.load_config(Path::new(repo_root)) else {
        return false;
    };
    let cap = cfg.repo.unblocker.cost_guard.daily_token_cap;
    if cap == 0 {
        return false;
    }
    db.cost_window_usage(now.saturating_sub(86_400))
        .is_ok_and(|(day_tokens, _)| day_tokens >= cap)
}

/// Returns `true` iff an external `aaf run` process currently holds the OS flock
/// for `repo_root`. A stale, missing, or non-regular lock file returns `false`.
pub(super) fn is_repo_lock_held(data_dir: &Path, repo_root: &str) -> bool {
    let lock_path = crate::cmd::run::detach::run_lock_path(data_dir, Path::new(repo_root));
    let Ok(meta) = fs::symlink_metadata(&lock_path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    let Ok(file) = File::open(&lock_path) else {
        return false;
    };
    match file.try_lock_shared() {
        Ok(()) => false,
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(_)) => false,
    }
}

/// Collects every repository path currently running an `aaf run` process
/// (either a live `UiSupervisor` worker or an external CLI runner holding the
/// repo OS flock). Includes both raw DB paths and canonical keys so UI matching
/// works across symlinks and trailing slashes.
pub(super) fn collect_running_repo_paths(
    data_dir: &Path,
    active_paths: &[String],
    known_repos: &[Value],
) -> Vec<String> {
    let active_set: BTreeSet<String> = active_paths.iter().map(|p| normalize_repo_key(p)).collect();
    let mut running: BTreeSet<String> = active_paths.iter().cloned().collect();
    for row in known_repos {
        let Some(raw) = row.get("path").and_then(Value::as_str) else {
            continue;
        };
        let norm = normalize_repo_key(raw);
        if active_set.contains(&norm) || is_repo_lock_held(data_dir, raw) {
            running.insert(raw.to_string());
            running.insert(norm);
        }
    }
    running.into_iter().collect()
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
    Ok(WorkerSlot {
        child,
        repo_name,
        spawned_at: Instant::now(),
    })
}

pub(super) fn stop_children_unlocked(
    workers: BTreeMap<String, WorkerSlot>,
    caffeinate: Option<Child>,
) {
    let mut active: Vec<WorkerSlot> = workers.into_values().collect();
    for slot in &active {
        let pid = slot.child.id();
        let _ = Command::new("/bin/kill")
            .args(["-TERM", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while !active.is_empty() && Instant::now() < deadline {
        active.retain_mut(|slot| !matches!(slot.child.try_wait(), Ok(Some(_))));
        if !active.is_empty() {
            thread::sleep(Duration::from_millis(50));
        }
    }
    for mut slot in active {
        let _ = slot.child.kill();
        let _ = slot.child.wait();
    }
    if let Some(mut c) = caffeinate {
        let _ = c.kill();
        let _ = c.wait();
    }
}

pub(super) fn sync_caffeinate_child(caffeinate: &mut Option<Child>, need: bool) {
    #[cfg(target_os = "macos")]
    {
        if let Some(ref mut c) = *caffeinate
            && c.try_wait().ok().flatten().is_some()
        {
            *caffeinate = None;
        }
        if need && caffeinate.is_none() {
            let pid = std::process::id().to_string();
            if let Ok(child) = Command::new("/usr/bin/caffeinate")
                .args(["-s", "-w", &pid])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                *caffeinate = Some(child);
            }
        } else if !need && let Some(mut c) = caffeinate.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (caffeinate, need);
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
#[allow(clippy::unwrap_used, clippy::expect_used)]
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

    #[test]
    fn is_repo_lock_held_distinguishes_stale_file_from_active_flock() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let repo_dir = tmp.path().join("repo");
        fs::create_dir_all(&repo_dir).unwrap();
        let repo_str = repo_dir.to_string_lossy().into_owned();

        // 1. Non-existent lock file -> false.
        assert!(!is_repo_lock_held(&data_dir, &repo_str));

        // 2. Existing unlocked (stale) lock file -> false.
        let lock_path = crate::cmd::run::detach::run_lock_path(&data_dir, &repo_dir);
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        fs::write(&lock_path, b"").unwrap();
        assert!(!is_repo_lock_held(&data_dir, &repo_str));

        // 3. Lock file currently held by an exclusive flock -> true.
        let holder = File::options()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        holder.try_lock().unwrap();
        assert!(is_repo_lock_held(&data_dir, &repo_str));

        // 4. After unlocking and dropping the holder, lock is released -> false again.
        holder.unlock().unwrap();
        drop(holder);
        assert!(!is_repo_lock_held(&data_dir, &repo_str));
    }

    #[test]
    fn collect_running_repo_paths_includes_workers_and_locked_external_repos() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let repo_ext = tmp.path().join("repo-ext");
        let repo_idle = tmp.path().join("repo-idle");
        fs::create_dir_all(&repo_ext).unwrap();
        fs::create_dir_all(&repo_idle).unwrap();
        let ext_str = repo_ext.to_string_lossy().into_owned();
        let idle_str = repo_idle.to_string_lossy().into_owned();
        let lock_path = crate::cmd::run::detach::run_lock_path(&data_dir, &repo_ext);
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        let holder = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        holder.try_lock().unwrap();

        let known = vec![
            serde_json::json!({ "path": ext_str }),
            serde_json::json!({ "path": idle_str }),
        ];
        let running = collect_running_repo_paths(&data_dir, &[], &known);
        assert!(running.contains(&ext_str));
        assert!(!running.contains(&idle_str));

        holder.unlock().unwrap();
        drop(holder);
        let after = collect_running_repo_paths(&data_dir, &[], &known);
        assert!(after.is_empty());
    }

    #[test]
    fn is_repo_cost_paused_respects_expiry_and_cap_updates() {
        use aaf_domain::ports::{RunStore, TaskStore};
        use aaf_domain::run::{RunRecord, RunStatus};
        use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};

        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let cfg_dir = tmp.path().join("cfg");
        let data_dir = tmp.path().join("data");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&cfg_dir).unwrap();
        fs::create_dir_all(&data_dir).unwrap();
        let ctx = Ctx {
            cwd: repo.clone(),
            home: tmp.path().to_path_buf(),
            data_dir: data_dir.clone(),
            config_dir: cfg_dir.clone(),
            worktrees_dir: tmp.path().join("wt"),
            path_var: std::ffi::OsString::new(),
            run_id: None,
            stdin_is_tty: false,
            aaf_env: Vec::new(),
        };
        let db = Db::open(&data_dir.join("aaf.db")).unwrap();
        let now = 200_000_i64;
        let repo_str = repo.to_string_lossy().into_owned();
        let mut t = Task::new_root(
            "t1",
            &repo_str,
            "hist",
            ImmutableSpec {
                description: "d".into(),
                acceptance: vec!["a".into()],
            },
            Priority::P2,
            1,
        );
        t.state = TaskState::Done;
        db.insert_task(&t).unwrap();
        db.insert_run(&RunRecord {
            id: "r1".into(),
            task_id: "t1".into(),
            account_id: Some("a".into()),
            executor: "claude".into(),
            worktree_path: String::new(),
            branch: "aaf/t1".into(),
            status: RunStatus::Succeeded,
            exit_code: Some(0),
            tokens_in: 1_500,
            tokens_out: 500,
            started_at: now - 3_600,
            ended_at: Some(now - 3_500),
        })
        .unwrap();
        db.set_meta(META_COST_PAUSED_UNTIL, &(now + 3_600).to_string())
            .unwrap();
        fs::write(
            cfg_dir.join("config.toml"),
            "[unblocker.cost_guard]\ndaily_token_cap = 1000\n",
        )
        .unwrap();

        assert!(is_repo_cost_paused_at(&ctx, &db, &repo_str, now));
        assert!(!is_repo_cost_paused_at(&ctx, &db, &repo_str, now + 3_600));

        // Raising daily_token_cap above 24h usage unpauses supervisor immediately.
        fs::write(
            cfg_dir.join("config.toml"),
            "[unblocker.cost_guard]\ndaily_token_cap = 5000\n",
        )
        .unwrap();
        assert!(!is_repo_cost_paused_at(&ctx, &db, &repo_str, now));

        // Disabling cost_guard (cap = 0) unpauses supervisor immediately.
        fs::write(
            cfg_dir.join("config.toml"),
            "[unblocker.cost_guard]\ndaily_token_cap = 0\n",
        )
        .unwrap();
        assert!(!is_repo_cost_paused_at(&ctx, &db, &repo_str, now));
    }
}
