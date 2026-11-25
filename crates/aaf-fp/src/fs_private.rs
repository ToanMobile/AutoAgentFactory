//! Owner-only (0700/0600) filesystem primitives and repo-scoped metadata keys
//! shared across `aaf-engine` infrastructure modules without depending on `db`.

use std::fs::{self, File};
use std::io;
use std::path::Path;

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

/// Creates `dir` and its missing parents with mode 0700. An existing `dir`
/// is tightened to 0700; parents that already existed are left alone.
pub fn create_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)?;
    restrict(dir, 0o700)
}

/// Opens `path` for writing without truncating it, creating it with mode
/// 0600. An existing file is tightened to 0600.
pub fn open_file(path: &Path) -> io::Result<File> {
    let mut options = File::options();
    options.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    restrict(path, 0o600)?;
    Ok(file)
}

/// Tightens `path` to 0600 when it exists.
pub fn restrict_file_if_present(path: &Path) -> io::Result<()> {
    match restrict(path, 0o600) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Meta key storing per-repository freeze state (`"1"` or `"probe"`).
pub fn repo_frozen_key(repo_root: &str) -> String {
    format!("repo_frozen:{}", crate::repo_id(Path::new(repo_root)))
}
