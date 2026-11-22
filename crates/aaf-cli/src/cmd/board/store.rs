//! What `aaf board` and `aaf run` share: opening the store and naming the
//! repository of the current directory.

use std::fs;
use std::path::PathBuf;

use aaf_engine::db::Db;

use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput};

pub(crate) const DB_FILE: &str = "aaf.db";

pub(crate) fn err(context: &str, error: impl std::fmt::Display) -> CmdError {
    CmdError::new(format!("{context}: {error}"))
}

pub(crate) fn open_db(ctx: &Ctx) -> Result<Db, CmdError> {
    aaf_engine::db::private::create_dir(&ctx.data_dir)
        .map_err(|e| err(&format!("data dir {}", ctx.data_dir.display()), e))?;
    Db::open(&ctx.data_dir.join(DB_FILE)).map_err(|e| err("database", e))
}

/// Opens `aaf.db` strictly read-only when present, returning `Ok(None)` when
/// the file does not exist so read-only commands never create `AAF_DATA_DIR`
/// or `aaf.db`.
pub fn open_db_read_only(ctx: &Ctx) -> Result<Option<Db>, CmdError> {
    let path = ctx.data_dir.join(DB_FILE);
    if !path.is_file() {
        return Ok(None);
    }
    Db::open_read_only(&path)
        .map(Some)
        .map_err(|e| err("database", e))
}

/// Root of the git work tree the command was started in, resolved, as it is
/// stored in `tasks.repo_root`.
pub(crate) fn repo_root(ctx: &Ctx) -> Result<PathBuf, CmdError> {
    let root = aaf_fp::repo_root(&ctx.cwd).map_err(|e| err("not inside a git repository", e))?;
    fs::canonicalize(&root).map_err(|e| err(&root.display().to_string(), e))
}

/// Unblocking, unfreezing and starting the factory are the human's side of
/// the loop: a process started by an agent run is refused.
pub(crate) fn refuse_inside_agent_run(ctx: &Ctx, what: &str) -> Option<CmdOutput> {
    ctx.run_id.as_ref().map(|run_id| {
        let text = format!("{what} is a human action: refused inside agent run {run_id}");
        CmdOutput::policy("refused", text)
    })
}
