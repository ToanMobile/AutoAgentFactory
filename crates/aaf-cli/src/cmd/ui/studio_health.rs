//! Read-only cached runtime & resource health snapshot for `/studio` and `/aaf_heath`.
//!
//! Zero `unsafe`, zero `tokio`, zero `unwrap`. Reports process RSS/FDs, SQLite
//! DB/WAL sizes, data-dir artifact footprint, and advisory closed-task worktree counts.

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use aaf_domain::ports::TaskStore;
use aaf_domain::task::TaskState;
use aaf_engine::db::Db;
use serde_json::{Value, json};

use crate::ctx::Ctx;

const HEALTH_CACHE_TTL: Duration = Duration::from_secs(15);
const WAL_WARN_BYTES: u64 = 64 * 1024 * 1024;
const RSS_WARN_MB: u64 = 256;
const FD_WARN_COUNT: u64 = 256;
const STREAMS_WARN_BYTES: u64 = 200 * 1024 * 1024;
const RECEIPTS_WARN_BYTES: u64 = 100 * 1024 * 1024;

#[derive(Default)]
pub(super) struct RuntimeHealthCache {
    entry: Mutex<Option<(Instant, Value)>>,
}

impl RuntimeHealthCache {
    pub(super) fn snapshot(
        &self,
        ctx: &Ctx,
        db: Option<&Db>,
        server_started_at: i64,
        now: i64,
    ) -> Value {
        {
            let guard = self.entry.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((at, val)) = guard.as_ref()
                && at.elapsed() < HEALTH_CACHE_TTL
            {
                return val.clone();
            }
        }
        let fresh = collect_runtime_health(ctx, db, server_started_at, now);
        let mut guard = self.entry.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some((Instant::now(), fresh.clone()));
        fresh
    }
}

pub(super) fn collect_runtime_health(
    ctx: &Ctx,
    db: Option<&Db>,
    server_started_at: i64,
    now: i64,
) -> Value {
    let pid = std::process::id();
    let uptime_secs = u64::try_from(now.saturating_sub(server_started_at).max(0)).unwrap_or(0);
    let rss_mb = current_process_rss_mb(pid);
    let open_fds = current_process_open_fds();

    let db_path = ctx.data_dir.join("aaf.db");
    let db_bytes = file_len(&db_path);
    let wal_bytes = file_len(&ctx.data_dir.join("aaf.db-wal"));
    let shm_bytes = file_len(&ctx.data_dir.join("aaf.db-shm"));

    let (streams_files, streams_bytes) = dir_flat_usage(&ctx.data_dir.join("streams"));
    let (receipts_files, receipts_bytes) = dir_flat_usage(&ctx.data_dir.join("receipts"));
    let (sup_log_files, sup_log_bytes) = dir_flat_usage(&ctx.data_dir.join("supervisor-logs"));
    let ui_log_bytes = file_len(&ctx.home.join("Library/Logs/aaf-ui.log"));

    let (wt_dir_count, orphan_closed_count) = scan_worktrees(&ctx.worktrees_dir, db);

    let mut alerts: Vec<Value> = Vec::new();
    if wal_bytes >= WAL_WARN_BYTES {
        alerts.push(json!({
            "severity": "high",
            "code": "sqlite_wal_bloat",
            "title": format!("SQLite WAL phình {} MiB (> 64 MiB)", wal_bytes / (1024 * 1024)),
            "detail": "Kết nối đọc giữ snapshot lâu có thể cản checkpoint WAL.",
            "active_recent": true,
        }));
    }
    if rss_mb.is_some_and(|mb| mb >= RSS_WARN_MB) {
        let mb = rss_mb.unwrap_or(0);
        alerts.push(json!({
            "severity": "high",
            "code": "ui_memory_high",
            "title": format!("Tiến trình aaf ui đang dùng {mb} MiB RSS (> {RSS_WARN_MB} MiB)"),
            "detail": "Kiểm tra xu hướng RSS theo thời gian và cache/SSE connections.",
            "active_recent": true,
        }));
    }
    if open_fds.is_some_and(|fds| fds >= FD_WARN_COUNT) {
        let fds = open_fds.unwrap_or(0);
        alerts.push(json!({
            "severity": "high",
            "code": "ui_fds_high",
            "title": format!("Tiến trình aaf ui đang mở {fds} file descriptors (> {FD_WARN_COUNT})"),
            "detail": "Có dấu hiệu rò rỉ socket SSE hoặc file handle.",
            "active_recent": true,
        }));
    }
    if streams_bytes >= STREAMS_WARN_BYTES {
        alerts.push(json!({
            "severity": "medium",
            "code": "streams_disk_bloat",
            "title": format!("Thư mục streams chiếm {} MiB ({streams_files} file)", streams_bytes / (1024 * 1024)),
            "detail": "Chạy `aaf gc --all` để dọn log stream cũ.",
            "active_recent": true,
        }));
    }
    if receipts_bytes >= RECEIPTS_WARN_BYTES {
        alerts.push(json!({
            "severity": "medium",
            "code": "receipts_disk_bloat",
            "title": format!("Thư mục receipts chiếm {} MiB ({receipts_files} file)", receipts_bytes / (1024 * 1024)),
            "detail": "Kiểm tra biên lai cũ trong data_dir/receipts.",
            "active_recent": true,
        }));
    }
    if orphan_closed_count > 0 {
        alerts.push(json!({
            "severity": "medium",
            "code": "orphan_worktrees",
            "title": format!("{orphan_closed_count} worktree của task đã Done/Cancelled vẫn còn trên đĩa"),
            "detail": "Chạy `aaf gc --all` để dọn worktree đã đóng.",
            "active_recent": true,
        }));
    }

    json!({
        "process": {
            "pid": pid,
            "server_started_at": server_started_at,
            "uptime_secs": uptime_secs,
            "rss_mb": rss_mb,
            "open_fds": open_fds,
        },
        "sqlite": {
            "db_bytes": db_bytes,
            "wal_bytes": wal_bytes,
            "shm_bytes": shm_bytes,
        },
        "disk": {
            "streams_files": streams_files,
            "streams_bytes": streams_bytes,
            "receipts_files": receipts_files,
            "receipts_bytes": receipts_bytes,
            "supervisor_log_files": sup_log_files,
            "supervisor_log_bytes": sup_log_bytes,
            "ui_log_bytes": ui_log_bytes,
        },
        "worktrees": {
            "dir": ctx.worktrees_dir.display().to_string(),
            "dir_count": wt_dir_count,
            "orphan_closed_count": orphan_closed_count,
        },
        "alerts": alerts,
    })
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |m| m.len())
}

fn dir_flat_usage(dir: &Path) -> (u64, u64) {
    let Ok(entries) = fs::read_dir(dir) else {
        return (0, 0);
    };
    let (mut files, mut bytes) = (0u64, 0u64);
    for entry in entries.flatten() {
        if let Ok(m) = entry.metadata()
            && m.is_file()
        {
            files = files.saturating_add(1);
            bytes = bytes.saturating_add(m.len());
        }
    }
    (files, bytes)
}

fn scan_worktrees(worktrees_dir: &Path, db: Option<&Db>) -> (u64, u64) {
    let Ok(repos) = fs::read_dir(worktrees_dir) else {
        return (0, 0);
    };
    let closed_ids: HashSet<String> = db
        .and_then(|d| d.list_tasks(None, None).ok())
        .map(|tasks| {
            tasks
                .into_iter()
                .filter(|t| matches!(t.state, TaskState::Done | TaskState::Cancelled))
                .map(|t| t.id)
                .collect()
        })
        .unwrap_or_default();
    let (mut total, mut orphan_closed) = (0u64, 0u64);
    for repo_ent in repos.flatten() {
        if !repo_ent.file_type().is_ok_and(|ft| ft.is_dir()) {
            continue;
        }
        let Ok(children) = fs::read_dir(repo_ent.path()) else {
            continue;
        };
        for child in children.flatten() {
            if !child.file_type().is_ok_and(|ft| ft.is_dir()) {
                continue;
            }
            total = total.saturating_add(1);
            if let Some(name) = child.file_name().to_str()
                && closed_ids.contains(name)
            {
                orphan_closed = orphan_closed.saturating_add(1);
            }
        }
    }
    (total, orphan_closed)
}

fn current_process_open_fds() -> Option<u64> {
    #[cfg(target_os = "macos")]
    let fd_dir = Path::new("/dev/fd");
    #[cfg(target_os = "linux")]
    let fd_dir = Path::new("/proc/self/fd");
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    return None;

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let count = fs::read_dir(fd_dir).ok()?.count() as u64;
        Some(count.saturating_sub(1))
    }
}

fn current_process_rss_mb(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let _ = pid;
        let text = fs::read_to_string("/proc/self/statm").ok()?;
        let pages: u64 = text.split_whitespace().nth(1)?.parse().ok()?;
        Some(pages.saturating_mul(4096) / (1024 * 1024))
    }
    #[cfg(target_os = "macos")]
    {
        let out = Command::new("/bin/ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let kb: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
        Some(kb / 1024)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = pid;
        None
    }
}
