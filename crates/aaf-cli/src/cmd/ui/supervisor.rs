//! Multi-repo background Auto-Runner Supervisor controlled from the Web UI.
//!
//! When enabled, periodically scans `aaf.db` for repositories with `Ready`,
//! `Running`, or `Gating` tasks (while the factory is not frozen), spawns
//! `<exe> run` (drain-and-exit) per repo with exponential backoff on failure,
//! and keeps macOS awake with `caffeinate -s -w <pid>` while at least one
//! worker is active.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

use aaf_domain::ports::TaskStore;
use aaf_domain::task::TaskState;
use aaf_engine::factory::is_frozen;
use serde_json::{Value, json};

#[cfg(test)]
pub(super) use super::supervisor_ops::runner_env;
use super::supervisor_ops::{
    PersistedState, WorkerSlot, is_repo_allowed, is_repo_lock_held, normalize_repo_key,
    read_persisted, spawn_repo_runner, stop_children_unlocked, sync_caffeinate_child,
    write_persisted,
};
use crate::cmd::board::store::SharedRoDb;
use crate::ctx::Ctx;

const STATE_FILE: &str = "ui-supervisor.json";
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const BASE_BACKOFF_SECS: u64 = 15;
const MAX_BACKOFF_SECS: u64 = 300;
const RAPID_EXIT_SECS: u64 = 10;

struct Inner {
    enabled: bool,
    allowed_repos: Option<BTreeSet<String>>,
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
        let persisted = read_persisted(&state_path);
        let initial_enabled = if allow_auto_start {
            persisted.enabled
        } else {
            false
        };
        let initial_allowed = persisted.allowed_repos.map(|list| {
            list.into_iter()
                .map(|r| normalize_repo_key(&r))
                .filter(|r| !r.is_empty())
                .collect::<BTreeSet<String>>()
        });
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
                allowed_repos: initial_allowed,
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

    pub(super) fn is_enabled(&self) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).enabled
    }

    pub(super) fn status_json(&self) -> Value {
        let known_repos = self.collect_known_repos();
        self.reap_workers();
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        sync_caffeinate(&mut guard);
        let active: Vec<String> = guard
            .workers
            .values()
            .map(|w| w.repo_name.clone())
            .collect();
        let active_paths: Vec<String> = guard.workers.keys().cloned().collect();
        let allowed_list: Option<Vec<String>> = guard
            .allowed_repos
            .as_ref()
            .map(|s| s.iter().cloned().collect());
        let now = Instant::now();
        let backoff: BTreeMap<String, u64> = if guard.enabled {
            guard
                .backoff_until
                .iter()
                .filter_map(|(repo, until)| {
                    until.checked_duration_since(now).map(|d| {
                        let name = Path::new(repo)
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or(repo)
                            .to_string();
                        (name, d.as_secs().max(1))
                    })
                })
                .collect()
        } else {
            BTreeMap::new()
        };
        json!({
            "enabled": guard.enabled,
            "active_repos": active,
            "active_repo_paths": active_paths,
            "allowed_repos": allowed_list,
            "known_repos": known_repos,
            "backoff": backoff,
            "caffeinate": guard.caffeinate.is_some(),
        })
    }

    fn reap_workers(&self) {
        let rapid_ok = {
            let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            reap_finished_raw(&mut guard)
        };
        if rapid_ok.is_empty() {
            return;
        }
        let outcomes: Vec<(String, bool)> = rapid_ok
            .into_iter()
            .map(|repo| {
                let pending = repo_has_pending_tasks(&self.ro_db, &repo);
                (repo, pending)
            })
            .collect();
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        apply_rapid_exit_outcomes(&mut guard, outcomes);
    }

    fn collect_known_repos(&self) -> Vec<Value> {
        let Ok(Some(db)) = self.ro_db.get() else {
            return Vec::new();
        };
        let mut rows: Vec<(String, String, usize, usize)> = db
            .repo_task_summary(self.scoped_repo.as_deref())
            .unwrap_or_default()
            .into_iter()
            .map(|(path, active, ready)| {
                let name = Path::new(&path)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&path)
                    .to_string();
                (path, name, active, ready)
            })
            .collect();
        rows.sort_by(|a, b| {
            a.1.to_lowercase()
                .cmp(&b.1.to_lowercase())
                .then(a.0.cmp(&b.0))
        });
        rows.into_iter()
            .map(|(path, name, active, ready)| {
                json!({
                    "path": path,
                    "name": name,
                    "active": active,
                    "ready": ready,
                })
            })
            .collect()
    }

    pub(super) fn handle_action(
        self: &Arc<Self>,
        action: &str,
        note: &str,
    ) -> Result<Value, String> {
        match action {
            "supervisor_toggle" => {
                let enable = match note.trim().to_ascii_lowercase().as_str() {
                    "on" | "true" | "1" => true,
                    "off" | "false" | "0" => false,
                    _ => return Err("invalid supervisor_toggle: expected 'on' or 'off'".into()),
                };
                self.set_enabled(enable)
            }
            "supervisor_repos" => {
                let parsed = super::supervisor_ops::parse_allowed_repos_note(note)?;
                self.set_allowed_repos(parsed)
            }
            other => Err(format!("invalid supervisor action: {other}")),
        }
    }

    pub(super) fn set_enabled(self: &Arc<Self>, enabled: bool) -> Result<Value, String> {
        let _persist_guard = self.persist_lock.lock().unwrap_or_else(|e| e.into_inner());
        let allowed_vec = {
            let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            guard
                .allowed_repos
                .as_ref()
                .map(|s| s.iter().cloned().collect::<Vec<String>>())
        };
        write_persisted(
            &self.state_path,
            &PersistedState {
                enabled,
                allowed_repos: allowed_vec,
            },
        )?;
        let to_stop = {
            let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            guard.enabled = enabled;
            guard.failures.clear();
            guard.backoff_until.clear();
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

    pub(super) fn set_allowed_repos(
        self: &Arc<Self>,
        repos: Option<Vec<String>>,
    ) -> Result<Value, String> {
        let _persist_guard = self.persist_lock.lock().unwrap_or_else(|e| e.into_inner());
        let normalized_set = repos.map(|list| {
            list.into_iter()
                .map(|r| normalize_repo_key(&r))
                .filter(|r| !r.is_empty())
                .collect::<BTreeSet<String>>()
        });
        let persisted_vec = normalized_set
            .as_ref()
            .map(|s| s.iter().cloned().collect::<Vec<String>>());
        let current_enabled = {
            let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            guard.enabled
        };
        write_persisted(
            &self.state_path,
            &PersistedState {
                enabled: current_enabled,
                allowed_repos: persisted_vec,
            },
        )?;
        let removed_workers = {
            let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            guard.allowed_repos = normalized_set;
            guard.failures.clear();
            guard.backoff_until.clear();
            let disallowed_keys: Vec<String> = guard
                .workers
                .keys()
                .filter(|repo| !is_repo_allowed(&guard.allowed_repos, repo))
                .cloned()
                .collect();
            let mut stopped = BTreeMap::new();
            for key in disallowed_keys {
                if let Some(slot) = guard.workers.remove(&key) {
                    stopped.insert(key, slot);
                }
            }
            sync_caffeinate(&mut guard);
            stopped
        };
        if !removed_workers.is_empty() {
            stop_children_unlocked(removed_workers, None);
        }
        if current_enabled {
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
    sup.reap_workers();
    let active_before_scan: BTreeSet<String> = {
        let mut guard = sup.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !guard.enabled {
            sync_caffeinate(&mut guard);
            return;
        }
        guard.workers.keys().cloned().collect()
    };

    let Ok(Some(db)) = sup.ro_db.get() else {
        return;
    };
    if is_frozen(&db).unwrap_or(false) {
        let mut guard = sup.inner.lock().unwrap_or_else(|e| e.into_inner());
        sync_caffeinate(&mut guard);
        return;
    }
    let mut candidate_repos = BTreeSet::new();
    for state in [TaskState::Ready, TaskState::Running, TaskState::Gating] {
        for t in db
            .list_tasks(sup.scoped_repo.as_deref(), Some(state))
            .unwrap_or_default()
        {
            if Path::new(&t.repo_root).is_dir() {
                candidate_repos.insert(t.repo_root);
            }
        }
    }

    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    sup.reap_workers();
    let now = Instant::now();
    let mut guard = sup.inner.lock().unwrap_or_else(|e| e.into_inner());
    if !guard.enabled {
        sync_caffeinate(&mut guard);
        return;
    }
    for repo in candidate_repos {
        if !is_repo_allowed(&guard.allowed_repos, &repo) {
            continue;
        }
        if active_before_scan.contains(&repo) || guard.workers.contains_key(&repo) {
            continue;
        }
        if guard.backoff_until.get(&repo).is_some_and(|u| *u > now) {
            continue;
        }
        if is_repo_lock_held(&sup.ctx.data_dir, &repo) {
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

fn repo_has_pending_tasks(ro_db: &SharedRoDb, repo_root: &str) -> bool {
    let Ok(Some(db)) = ro_db.get() else {
        return false;
    };
    [TaskState::Ready, TaskState::Running, TaskState::Gating]
        .into_iter()
        .any(|state| {
            db.list_tasks(Some(repo_root), Some(state))
                .is_ok_and(|tasks| !tasks.is_empty())
        })
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

/// Reaps exited workers. Returns repos that exited `0` faster than `RAPID_EXIT_SECS`
/// so the caller can check outside the mutex whether work still remains.
fn reap_finished_raw(inner: &mut Inner) -> Vec<String> {
    let mut done = Vec::new();
    for (repo, slot) in &mut inner.workers {
        if let Ok(Some(status)) = slot.child.try_wait() {
            let rapid = slot.spawned_at.elapsed() < Duration::from_secs(RAPID_EXIT_SECS);
            done.push((repo.clone(), status.success(), rapid));
        }
    }
    let mut rapid_ok = Vec::new();
    for (repo, ok, rapid) in done {
        inner.workers.remove(&repo);
        if ok && !rapid {
            inner.failures.remove(&repo);
            inner.backoff_until.remove(&repo);
        } else if ok {
            rapid_ok.push(repo);
        } else {
            record_failure(inner, repo);
        }
    }
    rapid_ok
}

fn apply_rapid_exit_outcomes(inner: &mut Inner, outcomes: Vec<(String, bool)>) {
    for (repo, still_pending) in outcomes {
        if still_pending {
            record_failure(inner, repo);
        } else {
            inner.failures.remove(&repo);
            inner.backoff_until.remove(&repo);
        }
    }
}

fn sync_caffeinate(inner: &mut Inner) {
    let need = inner.enabled && !inner.workers.is_empty();
    sync_caffeinate_child(&mut inner.caffeinate, need);
}

fn take_all_children(inner: &mut Inner) -> (BTreeMap<String, WorkerSlot>, Option<Child>) {
    (std::mem::take(&mut inner.workers), inner.caffeinate.take())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::process::{Command, Stdio};

    use super::*;

    fn empty_inner() -> Inner {
        Inner {
            enabled: true,
            allowed_repos: None,
            workers: BTreeMap::new(),
            failures: BTreeMap::new(),
            backoff_until: BTreeMap::new(),
            caffeinate: None,
            thread_started: false,
        }
    }

    fn spawn_exit_zero() -> Child {
        let mut child = Command::new("/usr/bin/true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // Wait briefly so try_wait() observes exit status immediately in reap_finished.
        for _ in 0..50 {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        child
    }

    #[test]
    fn rapid_exit_zero_only_backs_off_when_pending_work_remains() {
        let mut inner = empty_inner();
        let repo = "/tmp/demo-repo".to_string();

        // 1. Rapid exit 0 while tasks still remain -> triggers backoff to stop 5s busy-loop.
        inner.workers.insert(
            repo.clone(),
            WorkerSlot {
                child: spawn_exit_zero(),
                repo_name: "demo-repo".into(),
                spawned_at: Instant::now(),
            },
        );
        let rapid = reap_finished_raw(&mut inner);
        assert_eq!(rapid, vec![repo.clone()]);
        apply_rapid_exit_outcomes(&mut inner, vec![(repo.clone(), true)]);
        assert_eq!(inner.failures.get(&repo), Some(&1));
        assert!(inner.backoff_until.contains_key(&repo));

        // 2. Rapid exit 0 when repo drained cleanly (no pending tasks) -> clears backoff.
        inner.workers.insert(
            repo.clone(),
            WorkerSlot {
                child: spawn_exit_zero(),
                repo_name: "demo-repo".into(),
                spawned_at: Instant::now(),
            },
        );
        let rapid = reap_finished_raw(&mut inner);
        assert_eq!(rapid, vec![repo.clone()]);
        apply_rapid_exit_outcomes(&mut inner, vec![(repo.clone(), false)]);
        assert!(!inner.failures.contains_key(&repo));
        assert!(!inner.backoff_until.contains_key(&repo));

        // 3. Sustained run exiting 0 (>= RAPID_EXIT_SECS) -> immediately clears backoff.
        record_failure(&mut inner, repo.clone());
        inner.workers.insert(
            repo.clone(),
            WorkerSlot {
                child: spawn_exit_zero(),
                repo_name: "demo-repo".into(),
                spawned_at: Instant::now() - Duration::from_secs(RAPID_EXIT_SECS + 1),
            },
        );
        let rapid = reap_finished_raw(&mut inner);
        assert!(rapid.is_empty());
        assert!(!inner.failures.contains_key(&repo));
        assert!(!inner.backoff_until.contains_key(&repo));
    }
}
