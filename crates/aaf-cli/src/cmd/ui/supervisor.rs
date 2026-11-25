//! Multi-repo background Auto-Runner Supervisor controlled from the Web UI.
//!
//! When enabled, periodically scans `aaf.db` for repositories with `Ready` tasks
//! (while the factory is not frozen), spawns `<exe> run` (drain-and-exit) per repo
//! with exponential backoff on failure, and keeps macOS awake with
//! `caffeinate -s -w <pid>` while at least one worker is active.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

use aaf_domain::ports::TaskStore;
use aaf_domain::task::TaskState;
use aaf_engine::factory::is_frozen;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::cmd::board::store::SharedRoDb;
use crate::ctx::Ctx;

const STATE_FILE: &str = "ui-supervisor.json";
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const BASE_BACKOFF_SECS: u64 = 15;
const MAX_BACKOFF_SECS: u64 = 300;
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedState {
    enabled: bool,
}

struct WorkerSlot {
    child: Child,
    repo_name: String,
}

struct Inner {
    enabled: bool,
    workers: BTreeMap<String, WorkerSlot>,
    failures: BTreeMap<String, u32>,
    backoff_until: BTreeMap<String, Instant>,
    caffeinate: Option<Child>,
    thread_started: bool,
}

pub(super) struct UiSupervisor {
    ctx: Ctx,
    scoped_repo: Option<String>,
    ro_db: SharedRoDb,
    state_path: PathBuf,
    stop_flag: AtomicBool,
    persist_lock: Mutex<()>,
    inner: Mutex<Inner>,
}

impl UiSupervisor {
    pub(super) fn new(ctx: Ctx, scoped_repo: Option<PathBuf>, allow_auto_start: bool) -> Arc<Self> {
        let state_path = ctx.data_dir.join(STATE_FILE);
        let initial_enabled = if allow_auto_start {
            read_persisted(&state_path).enabled
        } else {
            false
        };
        let scoped = scoped_repo.map(|p| {
            fs::canonicalize(&p)
                .unwrap_or(p)
                .to_string_lossy()
                .into_owned()
        });
        let sup = Arc::new(Self {
            ro_db: SharedRoDb::for_ctx(&ctx),
            ctx,
            scoped_repo: scoped,
            state_path,
            stop_flag: AtomicBool::new(false),
            persist_lock: Mutex::new(()),
            inner: Mutex::new(Inner {
                enabled: initial_enabled,
                workers: BTreeMap::new(),
                failures: BTreeMap::new(),
                backoff_until: BTreeMap::new(),
                caffeinate: None,
                thread_started: false,
            }),
        });
        if initial_enabled {
            Self::ensure_thread(&sup);
        }
        sup
    }

    pub(super) fn status_json(&self) -> Value {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        reap_finished(&mut guard);
        let active: Vec<String> = guard
            .workers
            .values()
            .map(|w| w.repo_name.clone())
            .collect();
        let now = Instant::now();
        let backoff: BTreeMap<String, u64> = guard
            .backoff_until
            .iter()
            .filter_map(|(repo, until)| {
                until.checked_duration_since(now).map(|d| {
                    let name = Path::new(repo)
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or(repo)
                        .to_string();
                    (name, d.as_secs())
                })
            })
            .collect();
        json!({
            "enabled": guard.enabled,
            "active_repos": active,
            "backoff": backoff,
            "caffeinate": guard.caffeinate.is_some(),
        })
    }

    pub(super) fn set_enabled(self: &Arc<Self>, enabled: bool) -> Result<Value, String> {
        let _persist_guard = self.persist_lock.lock().unwrap_or_else(|e| e.into_inner());
        write_persisted(&self.state_path, enabled)?;
        let to_stop = {
            let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            guard.enabled = enabled;
            if !enabled {
                Some(take_all_children(&mut guard))
            } else {
                None
            }
        };
        if let Some((workers, caffeinate)) = to_stop {
            stop_children_unlocked(workers, caffeinate);
        }
        if enabled {
            Self::ensure_thread(self);
        }
        Ok(self.status_json())
    }

    pub(super) fn shutdown(&self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        let (workers, caffeinate) = {
            let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            guard.enabled = false;
            take_all_children(&mut guard)
        };
        stop_children_unlocked(workers, caffeinate);
    }

    fn ensure_thread(this: &Arc<Self>) {
        let mut guard = this.inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard.thread_started {
            return;
        }
        guard.thread_started = true;
        drop(guard);
        let weak = Arc::downgrade(this);
        let _ = thread::Builder::new()
            .name("aaf-ui-supervisor".into())
            .spawn(move || loop_forever(weak));
    }
}

impl Drop for UiSupervisor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn loop_forever(weak: Weak<UiSupervisor>) {
    while let Some(sup) = weak.upgrade() {
        if sup.stop_flag.load(Ordering::Relaxed) {
            return;
        }
        tick(&sup);
        drop(sup);
        for _ in 0..25 {
            let Some(sup) = weak.upgrade() else {
                return;
            };
            if sup.stop_flag.load(Ordering::Relaxed) {
                return;
            }
            drop(sup);
            thread::sleep(POLL_INTERVAL / 25);
        }
    }
}

fn tick(sup: &UiSupervisor) {
    {
        let mut guard = sup.inner.lock().unwrap_or_else(|e| e.into_inner());
        reap_finished(&mut guard);
        if !guard.enabled {
            sync_caffeinate(&mut guard);
            return;
        }
    }

    let Ok(Some(db)) = sup.ro_db.get() else {
        return;
    };
    if is_frozen(&db).unwrap_or(false) {
        let mut guard = sup.inner.lock().unwrap_or_else(|e| e.into_inner());
        sync_caffeinate(&mut guard);
        return;
    }
    let ready_tasks = db
        .list_tasks(sup.scoped_repo.as_deref(), Some(TaskState::Ready))
        .unwrap_or_default();
    let running_repos: BTreeSet<String> = db
        .running_runs(None)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| db.get_task(&r.task_id).ok().map(|t| t.repo_root))
        .collect();

    let mut ready_repos = BTreeSet::new();
    for t in ready_tasks {
        if Path::new(&t.repo_root).is_dir() {
            ready_repos.insert(t.repo_root);
        }
    }

    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let now = Instant::now();
    let mut guard = sup.inner.lock().unwrap_or_else(|e| e.into_inner());
    reap_finished(&mut guard);
    if !guard.enabled {
        sync_caffeinate(&mut guard);
        return;
    }
    for repo in ready_repos {
        if guard.workers.contains_key(&repo) || running_repos.contains(&repo) {
            continue;
        }
        if guard.backoff_until.get(&repo).is_some_and(|u| *u > now) {
            continue;
        }
        match spawn_repo_runner(&exe, &sup.ctx, &repo) {
            Ok(slot) => {
                guard.workers.insert(repo, slot);
            }
            Err(_) => record_failure(&mut guard, repo),
        }
    }
    sync_caffeinate(&mut guard);
}

fn spawn_repo_runner(exe: &Path, ctx: &Ctx, repo_root: &str) -> Result<WorkerSlot, String> {
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
        .stdin(Stdio::null())
        .env("PATH", &ctx.path_var)
        .env("HOME", &ctx.home)
        .env("AAF_DATA_DIR", &ctx.data_dir)
        .env("AAF_CONFIG_DIR", &ctx.config_dir)
        .env("AAF_WORKTREE_DIR", &ctx.worktrees_dir);
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

fn compute_backoff_secs(failures: u32) -> u64 {
    let shift = failures.saturating_sub(1).min(5);
    (BASE_BACKOFF_SECS.saturating_mul(1_u64 << shift)).min(MAX_BACKOFF_SECS)
}

fn record_failure(inner: &mut Inner, repo: String) {
    let cnt = inner.failures.entry(repo.clone()).or_insert(0);
    *cnt = cnt.saturating_add(1);
    let secs = compute_backoff_secs(*cnt);
    inner
        .backoff_until
        .insert(repo, Instant::now() + Duration::from_secs(secs));
}

fn reap_finished(inner: &mut Inner) {
    let mut done = Vec::new();
    for (repo, slot) in &mut inner.workers {
        if let Ok(Some(status)) = slot.child.try_wait() {
            done.push((repo.clone(), status.success()));
        }
    }
    for (repo, ok) in done {
        inner.workers.remove(&repo);
        if ok {
            inner.failures.remove(&repo);
            inner.backoff_until.remove(&repo);
        } else {
            record_failure(inner, repo);
        }
    }
}

fn sync_caffeinate(inner: &mut Inner) {
    #[cfg(target_os = "macos")]
    {
        if let Some(ref mut c) = inner.caffeinate
            && c.try_wait().ok().flatten().is_some()
        {
            inner.caffeinate = None;
        }
        let need = inner.enabled && !inner.workers.is_empty();
        if need && inner.caffeinate.is_none() {
            let pid = std::process::id().to_string();
            if let Ok(child) = Command::new("/usr/bin/caffeinate")
                .args(["-s", "-w", &pid])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                inner.caffeinate = Some(child);
            }
        } else if !need && let Some(mut c) = inner.caffeinate.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = inner;
    }
}

fn take_all_children(inner: &mut Inner) -> (BTreeMap<String, WorkerSlot>, Option<Child>) {
    (std::mem::take(&mut inner.workers), inner.caffeinate.take())
}

fn stop_children_unlocked(workers: BTreeMap<String, WorkerSlot>, caffeinate: Option<Child>) {
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

fn read_persisted(path: &Path) -> PersistedState {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_persisted(path: &Path, enabled: bool) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let body = serde_json::to_vec(&PersistedState { enabled }).map_err(|e| e.to_string())?;
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
