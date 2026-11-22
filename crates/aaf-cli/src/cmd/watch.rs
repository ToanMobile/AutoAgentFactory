//! `aaf watch`: 100% read-only live terminal dashboard (zero network ports).

use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use aaf_domain::ports::Clock;
use aaf_engine::db::Db;
use aaf_engine::watch::{WatchQuery, WatchSnapshot, collect_watch_snapshot, render_watch_text};
use serde_json::json;

use super::board::store::{DB_FILE, err, open_db_read_only};
use crate::ctx::{Ctx, SystemClock};
use crate::output::{CmdError, CmdOutput, CmdResult};

const POLL_TICK: Duration = Duration::from_millis(100);

#[derive(Debug, clap::Args)]
pub struct WatchArgs {
    /// Inspect a specific repository path instead of the current directory.
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// Aggregate across all repositories in the database.
    #[arg(long, conflicts_with = "repo")]
    pub all: bool,
    /// Refresh interval in seconds (default 2).
    #[arg(long, default_value_t = 2)]
    pub interval: u64,
    /// Collect and print a single snapshot, then exit 0 (no sleep).
    #[arg(long)]
    pub once: bool,
}

fn resolve_scope(
    args: &WatchArgs,
    ctx: &Ctx,
) -> Result<(String, Option<PathBuf>, PathBuf), CmdError> {
    if args.all {
        return Ok(("all".to_string(), None, ctx.cwd.clone()));
    }
    if let Some(repo) = &args.repo {
        let candidate = if repo.is_absolute() {
            repo.clone()
        } else {
            ctx.cwd.join(repo)
        };
        if !candidate.exists() {
            return Err(CmdError::new(format!(
                "--repo {} does not exist",
                candidate.display()
            )));
        }
        let root = aaf_fp::repo_root(&candidate).unwrap_or(candidate);
        let canonical = fs::canonicalize(&root).unwrap_or(root);
        let scope = canonical.display().to_string();
        return Ok((scope, Some(canonical.clone()), canonical));
    }
    match aaf_fp::repo_root(&ctx.cwd) {
        Ok(root) => {
            let canonical = fs::canonicalize(&root).unwrap_or(root);
            let scope = canonical.display().to_string();
            Ok((scope, Some(canonical.clone()), canonical))
        }
        Err(_) => Ok(("all (outside git repo)".to_string(), None, ctx.cwd.clone())),
    }
}

fn snapshot_output(snap: &WatchSnapshot) -> CmdResult {
    Ok(CmdOutput::ok("ok", render_watch_text(snap))
        .field("scope", json!(snap.scope))
        .field("repo_root", json!(snap.repo_root))
        .field("now", json!(snap.now))
        .field("frozen", json!(snap.frozen))
        .field("consecutive_root_fails", json!(snap.consecutive_root_fails))
        .field(
            "tasks",
            serde_json::to_value(&snap.tasks).map_err(|e| err("tasks", e))?,
        )
        .field(
            "heavy_slots",
            serde_json::to_value(&snap.heavy_slots).map_err(|e| err("heavy_slots", e))?,
        )
        .field(
            "accounts",
            serde_json::to_value(&snap.accounts).map_err(|e| err("accounts", e))?,
        )
        .field(
            "receipt",
            serde_json::to_value(&snap.receipt).map_err(|e| err("receipt", e))?,
        ))
}

fn spawn_quit_listener(quit: Arc<AtomicBool>) {
    let _ = thread::Builder::new()
        .name("aaf-watch-stdin".to_string())
        .spawn(move || {
            let mut stdin = io::stdin();
            let mut buf = [0u8; 64];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        quit.store(true, Ordering::Relaxed);
                        break;
                    }
                    Ok(n) => {
                        if buf[..n].iter().any(|b| matches!(b, b'q' | b'Q')) {
                            quit.store(true, Ordering::Relaxed);
                            break;
                        }
                    }
                }
            }
        });
}

pub fn run(args: &WatchArgs, ctx: &Ctx, json_mode: bool) -> CmdResult {
    let (scope, scoped_repo, config_root) = resolve_scope(args, ctx)?;
    let cfg = ctx.load_config(&config_root)?;
    let db_path = ctx.data_dir.join(DB_FILE);
    // Strictly read-only: never create `data_dir` or `aaf.db` if absent,
    // and open SQLite read-only (`SQLITE_OPEN_READ_ONLY` + `query_only = ON`).
    let mut db = open_db_read_only(ctx)?;

    let collect = |db: Option<&Db>| -> Result<WatchSnapshot, CmdError> {
        let query = WatchQuery {
            scope: &scope,
            repo_root: scoped_repo.as_deref(),
            data_dir: &ctx.data_dir,
            selection: cfg.global.accounts.selection,
            executor: &cfg.repo.run.executor,
            now: SystemClock.now(),
        };
        collect_watch_snapshot(&query, db).map_err(|e| CmdError::new(e.to_string()))
    };

    if args.once || json_mode {
        let snap = collect(db.as_ref())?;
        return snapshot_output(&snap);
    }

    let quit = Arc::new(AtomicBool::new(false));
    if ctx.stdin_is_tty {
        spawn_quit_listener(Arc::clone(&quit));
    }
    let stdout_tty = io::stdout().is_terminal();
    let interval_secs = args.interval.max(1);
    let ticks = interval_secs.saturating_mul(10);

    loop {
        if db.is_none() && db_path.is_file() {
            db = Db::open_read_only(&db_path).ok();
        }
        let snap = collect(db.as_ref())?;
        let text = render_watch_text(&snap);
        let mut stdout = io::stdout();
        let write_res = if stdout_tty {
            write!(
                stdout,
                "\x1b[2J\x1b[H{text}\n\n[refresh {interval_secs}s | press q + Enter or Ctrl-C to quit]\n"
            )
        } else {
            writeln!(stdout, "{text}")
        }
        .and_then(|()| stdout.flush());
        if let Err(io_err) = write_res {
            if io_err.kind() == io::ErrorKind::BrokenPipe {
                return Ok(CmdOutput::ok("ok", ""));
            }
            return Err(err("stdout", io_err));
        }

        for _ in 0..ticks {
            if quit.load(Ordering::Relaxed) {
                return Ok(CmdOutput::ok("ok", ""));
            }
            thread::sleep(POLL_TICK);
        }
    }
}
