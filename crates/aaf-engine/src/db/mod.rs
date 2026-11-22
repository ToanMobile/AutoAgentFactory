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
mod schema;
mod stores;
mod tasks;

use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use aaf_domain::ports::{PortError, PortResult};
use rusqlite::{Connection, ErrorCode, Transaction, TransactionBehavior};

pub use schema::SCHEMA_VERSION;

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
            }),
        })
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
