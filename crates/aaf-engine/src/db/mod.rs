//! SQLite store (docs/PLAN.md section C.7).
//!
//! [`Db`] is a cheap handle to one writer thread that owns the only
//! connection of this process. Every request is a closure sent over an
//! `mpsc` channel together with its reply channel, so statements never
//! interleave inside a process. Other processes are handled by SQLite itself:
//! `busy_timeout` plus `BEGIN IMMEDIATE` make a second writer wait instead of
//! failing half-way through a transaction.

mod history;
pub mod private;
mod queries;
mod schema;
pub mod snapshots;
mod stores;
mod tasks;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use aaf_domain::ports::{PortError, PortResult};
use rusqlite::{Connection, ErrorCode, OpenFlags, Transaction, TransactionBehavior};

pub use queries::BoardEnrichment;
pub use schema::SCHEMA_VERSION;
pub use snapshots::TurnSnapshot;

const BUSY_TIMEOUT: Duration = Duration::from_millis(5000);
/// Together these bound the wait in [`Db::open`] to about `BUSY_TIMEOUT`.
const OPEN_BUSY_RETRIES: u32 = 250;
const OPEN_BUSY_PAUSE: Duration = Duration::from_millis(20);

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("database {path} did not switch to WAL (journal_mode={mode})")]
    NotWal { path: String, mode: String },
    #[error(
        "database schema version {found} is not supported by this build (knows 0..={supported})"
    )]
    UnsupportedSchema { found: i64, supported: i64 },
    #[error("cannot start the database writer thread: {0}")]
    Thread(String),
    #[error("database file {path}: {source}")]
    File {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

struct Inner {
    /// `None` only while dropping.
    jobs: Option<mpsc::Sender<Job>>,
    writer: Option<JoinHandle<()>>,
    schema_version: i64,
    immutable: bool,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Closing the channel ends the writer loop; joining guarantees the
        // connection is closed (and the WAL checkpointed) once the last
        // handle is gone.
        self.jobs = None;
        if let Some(writer) = self.writer.take() {
            // Err means a job panicked on the writer thread. Its caller
            // already got `PortError::Io`; a destructor cannot report more.
            let _ = writer.join();
        }
    }
}

/// Handle to the store. Clones share the same writer thread.
#[derive(Clone)]
pub struct Db {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db").finish_non_exhaustive()
    }
}

impl Db {
    /// Opens (creating if needed) the database at `path`, applies pending
    /// migrations and starts the writer thread.
    pub fn open(path: &Path) -> Result<Db, DbError> {
        // SQLite would create the file with the umask's mode, and gives its
        // `-wal` and `-shm` files the mode of the database file: created
        // (or tightened) here first, all three are owner-only.
        let file = |path: &Path, source| DbError::File {
            path: path.display().to_string(),
            source,
        };
        let created = private::open_file(path).map(drop);
        if created.is_ok() {
            for suffix in ["-wal", "-shm"] {
                let mut side = path.as_os_str().to_os_string();
                side.push(suffix);
                let side = Path::new(&side);
                private::restrict_file_if_present(side).map_err(|e| file(side, e))?;
            }
        }
        // A path that cannot be opened at all is SQLite's error to report,
        // as before; a database SQLite can open but whose mode could not be
        // set is refused rather than left readable by others.
        let mut conn = Connection::open(path)?;
        created.map_err(|e| file(path, e))?;
        // Before any other statement, so the migration waits for a
        // concurrent process instead of failing.
        conn.busy_timeout(BUSY_TIMEOUT)?;
        let mut busy_retries = 0;
        loop {
            match configure(&mut conn, path) {
                Err(DbError::Sqlite(err))
                    if busy_retries < OPEN_BUSY_RETRIES
                        && err.sqlite_error_code() == Some(ErrorCode::DatabaseBusy) =>
                {
                    busy_retries += 1;
                    std::thread::sleep(OPEN_BUSY_PAUSE);
                }
                result => break result?,
            }
        }

        let (jobs, queue) = mpsc::channel::<Job>();
        let writer = std::thread::Builder::new()
            .name("aaf-db-writer".to_string())
            .spawn(move || {
                for job in queue {
                    job(&mut conn);
                }
            })
            .map_err(|e| DbError::Thread(e.to_string()))?;
        Ok(Db {
            inner: Arc::new(Inner {
                jobs: Some(jobs),
                writer: Some(writer),
                schema_version: SCHEMA_VERSION,
                immutable: false,
            }),
        })
    }

    /// Opens an existing database at `path` strictly read-only (`SQLITE_OPEN_READ_ONLY`
    /// plus `PRAGMA query_only = ON`). Never creates files, chmods, changes journal
    /// mode, or runs migrations — safe for read-only observers like `aaf watch`.
    ///
    /// Enforces `PRAGMA user_version <= SCHEMA_VERSION`. When no `-wal` sidecar
    /// exists and the parent directory is non-writable (or normal read-only
    /// open/probe fails with `ReadOnly`/`CannotOpen`), falls back to a
    /// `file:<path>?mode=ro&immutable=1` URI so zero `-shm`/`-wal` files are created.
    pub fn open_read_only(path: &Path) -> Result<Db, DbError> {
        let (mut conn, schema_version, immutable) = open_read_only_conn(path)?;
        let (jobs, queue) = mpsc::channel::<Job>();
        let writer = std::thread::Builder::new()
            .name("aaf-db-reader".to_string())
            .spawn(move || {
                for job in queue {
                    job(&mut conn);
                }
            })
            .map_err(|e| DbError::Thread(e.to_string()))?;
        Ok(Db {
            inner: Arc::new(Inner {
                jobs: Some(jobs),
                writer: Some(writer),
                schema_version,
                immutable,
            }),
        })
    }

    /// Schema version of the opened database (`SCHEMA_VERSION` for read-write
    /// handles, or the on-disk `PRAGMA user_version` for read-only handles).
    pub fn schema_version(&self) -> i64 {
        self.inner.schema_version
    }

    /// True when this read-only handle used SQLite's `immutable=1` URI fallback
    /// (such handles assume the file never changes and must not be cached).
    pub fn is_immutable(&self) -> bool {
        self.inner.immutable
    }

    /// Runs `f` on the writer thread and waits for its result.
    fn call<T, F>(&self, f: F) -> PortResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> PortResult<T> + Send + 'static,
    {
        let stopped = || PortError::Io("database writer thread has stopped".to_string());
        let (reply, result) = mpsc::channel();
        let job: Job = Box::new(move |conn| {
            // The caller only goes away without reading when its thread is
            // being torn down; there is nobody left to tell.
            let _ = reply.send(f(conn));
        });
        self.inner
            .jobs
            .as_ref()
            .ok_or_else(stopped)?
            .send(job)
            .map_err(|_| stopped())?;
        result.recv().map_err(|_| stopped())?
    }

    /// Runs `f` inside one `BEGIN IMMEDIATE` transaction; an error rolls back.
    fn write<T, F>(&self, f: F) -> PortResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction<'_>) -> PortResult<T> + Send + 'static,
    {
        self.call(move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_err)?;
            let value = f(&tx)?;
            tx.commit().map_err(sql_err)?;
            Ok(value)
        })
    }

    fn read<T, F>(&self, f: F) -> PortResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> PortResult<T> + Send + 'static,
    {
        self.call(move |conn| f(conn))
    }
}

fn open_read_only_conn(path: &Path) -> Result<(Connection, i64, bool), DbError> {
    let can_use_immutable = path.is_file() && !wal_sidecar_exists(path);
    if can_use_immutable && parent_dir_read_only(path) {
        let (conn, version) = open_immutable_ro(path)?;
        return Ok((conn, version, true));
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let first = Connection::open_with_flags(path, flags)
        .map_err(DbError::from)
        .and_then(|conn| {
            let version = configure_read_only(&conn)?;
            Ok((conn, version, false))
        });
    match first {
        Ok(triple) => Ok(triple),
        Err(err) if can_use_immutable && is_ro_fallback_error(&err) => {
            let (conn, version) = open_immutable_ro(path)?;
            Ok((conn, version, true))
        }
        Err(err) => Err(err),
    }
}

fn open_immutable_ro(path: &Path) -> Result<(Connection, i64), DbError> {
    let uri = immutable_ro_uri(path)?;
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(&uri, flags)?;
    let version = configure_read_only(&conn)?;
    Ok((conn, version))
}

fn configure_read_only(conn: &Connection) -> Result<i64, DbError> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "query_only", "ON")?;
    let found: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if !(0..=SCHEMA_VERSION).contains(&found) {
        return Err(DbError::UnsupportedSchema {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    let mut stmt = conn.prepare("SELECT 1 FROM sqlite_master LIMIT 1")?;
    let _ = stmt.exists([])?;
    Ok(found)
}

fn is_ro_fallback_error(err: &DbError) -> bool {
    matches!(
        err,
        DbError::Sqlite(sql_err)
            if matches!(
                sql_err.sqlite_error_code(),
                Some(ErrorCode::ReadOnly | ErrorCode::CannotOpen)
            )
    )
}

fn wal_sidecar_exists(path: &Path) -> bool {
    let mut wal = path.as_os_str().to_os_string();
    wal.push("-wal");
    std::fs::symlink_metadata(&wal).is_ok()
}

#[cfg(unix)]
fn parent_dir_read_only(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.parent()
        .and_then(|parent| std::fs::metadata(parent).ok())
        .is_some_and(|meta| meta.permissions().mode() & 0o222 == 0)
}

#[cfg(not(unix))]
fn parent_dir_read_only(path: &Path) -> bool {
    path.parent()
        .and_then(|parent| std::fs::metadata(parent).ok())
        .is_some_and(|meta| meta.permissions().readonly())
}

fn immutable_ro_uri(path: &Path) -> Result<String, DbError> {
    let raw = path.to_str().ok_or_else(|| DbError::File {
        path: path.display().to_string(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "database path is not valid UTF-8 for SQLite URI",
        ),
    })?;
    let mut uri = String::with_capacity(raw.len() + 32);
    if raw.starts_with('/') {
        uri.push_str("file://");
    } else {
        uri.push_str("file:");
    }
    let normalized = if raw.starts_with("//") {
        let leading = raw.bytes().take_while(|b| *b == b'/').count();
        &raw[leading - 1..]
    } else {
        raw
    };
    for byte in normalized.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            uri.push(byte as char);
        } else {
            let _ = std::fmt::Write::write_fmt(&mut uri, format_args!("%{byte:02X}"));
        }
    }
    uri.push_str("?mode=ro&immutable=1");
    Ok(uri)
}

/// Connection pragmas and migrations. Safe to repeat on the same connection.
///
/// Two processes that open a fresh file together both hold a shared lock and
/// both ask for the exclusive one to switch to WAL. SQLite resolves that
/// deadlock by failing one of them with `SQLITE_BUSY` at once, without
/// consulting the busy handler, so [`Db::open`] repeats this function: by the
/// next attempt the winner has finished and the switch is a no-op.
fn configure(conn: &mut Connection, path: &Path) -> Result<(), DbError> {
    let mode: String =
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(DbError::NotWal {
            path: path.display().to_string(),
            mode,
        });
    }
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    schema::migrate(conn)
}

/// Maps a SQLite failure onto the port error vocabulary.
fn sql_err(err: rusqlite::Error) -> PortError {
    if let Some(failure) = err.sqlite_error()
        && failure.code == ErrorCode::ConstraintViolation
    {
        return if failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY {
            PortError::Invalid(format!("references a row that does not exist: {err}"))
        } else {
            PortError::Conflict(err.to_string())
        };
    }
    PortError::Io(err.to_string())
}

/// SQLite integers are signed 64-bit; larger values are refused rather than
/// silently wrapped.
fn to_i64(value: u64, what: &str) -> PortResult<i64> {
    i64::try_from(value).map_err(|_| PortError::Invalid(format!("{what} {value} is too large")))
}

fn to_u64(value: i64, what: &str) -> PortResult<u64> {
    u64::try_from(value).map_err(|_| corrupt(what, &value.to_string()))
}

fn corrupt(what: &str, value: &str) -> PortError {
    PortError::Io(format!("corrupt row: {what} = {value:?}"))
}

#[cfg(unix)]
fn file_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    meta.is_file().then(|| (meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn file_identity(path: &Path) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    meta.is_file().then(|| (meta.len(), 0))
}

/// Caches a single read-only [`Db`] handle keyed by `(dev, ino)` so long-lived
/// loops (`aaf ui`, `aaf cockpit`, `aaf telegram`) reuse one reader thread and
/// connection. Never caches `immutable=1` fallback or older-schema handles and automatically
/// drops the cached handle if the file disappears or its inode changes.
#[derive(Debug)]
pub struct RoDbCache {
    path: PathBuf,
    cached: Mutex<Option<((u64, u64), Db)>>,
}

impl RoDbCache {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            cached: Mutex::new(None),
        }
    }

    pub fn open(&self) -> Result<Option<Db>, DbError> {
        let mut guard = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        let Some(id) = file_identity(&self.path) else {
            *guard = None;
            return Ok(None);
        };
        if let Some((cached_id, db)) = guard.as_ref()
            && *cached_id == id
        {
            return Ok(Some(db.clone()));
        }
        *guard = None;
        let db = Db::open_read_only(&self.path)?;
        // An older-schema handle goes stale once the daemon migrates the same
        // inode in place, so only current-schema handles are reused.
        if !db.is_immutable()
            && db.schema_version() >= SCHEMA_VERSION
            && file_identity(&self.path) == Some(id)
        {
            *guard = Some((id, db.clone()));
        }
        Ok(Some(db))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn open_read_only_rejects_future_schema_version() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("future.db");
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .unwrap();
        }
        let err = Db::open_read_only(&db_path).unwrap_err();
        assert!(
            matches!(
                err,
                DbError::UnsupportedSchema {
                    found,
                    supported: SCHEMA_VERSION,
                } if found == SCHEMA_VERSION + 1
            ),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn ro_db_cache_reuses_handle_and_drops_on_delete_or_replace() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("aaf.db");
        let cache = RoDbCache::new(db_path.clone());
        assert!(cache.open().unwrap().is_none());

        drop(Db::open(&db_path).unwrap());
        let first = cache.open().unwrap().unwrap();
        assert!(!first.is_immutable());
        let second = cache.open().unwrap().unwrap();
        assert!(Arc::ptr_eq(&first.inner, &second.inner));

        // Replacing the file with a new inode drops the old handle and opens a fresh one.
        let replacement = tmp.path().join("replacement.db");
        drop(Db::open(&replacement).unwrap());
        std::fs::rename(&replacement, &db_path).unwrap();
        let third = cache.open().unwrap().unwrap();
        assert!(!Arc::ptr_eq(&first.inner, &third.inner));

        // Removing the file clears the cache and returns None.
        std::fs::remove_file(&db_path).unwrap();
        assert!(cache.open().unwrap().is_none());
    }

    #[test]
    fn ro_db_cache_reports_current_schema_after_in_place_migration() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("aaf.db");
        {
            // An older (pre-v1) database file: valid SQLite, user_version 0.
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch("CREATE TABLE legacy (x INTEGER);")
                .unwrap();
        }
        let cache = RoDbCache::new(db_path.clone());
        let old = cache.open().unwrap().unwrap();
        assert!(old.schema_version() < SCHEMA_VERSION);
        drop(old);

        // The daemon migrates the same inode in place.
        drop(Db::open(&db_path).unwrap());
        let fresh = cache.open().unwrap().unwrap();
        assert_eq!(fresh.schema_version(), SCHEMA_VERSION);
    }
}
