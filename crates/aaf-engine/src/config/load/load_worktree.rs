//! Reads `[worktree]` strictly from the committed base commit via `git show`.

use std::io;
use std::path::{Path, PathBuf};

use aaf_domain::config::RepoConfig;
use aaf_domain::worktree_config::WorktreeConfig;

use super::{ConfigError, REPO_CONFIG_REL, parse_toml_at};

/// Reads `[worktree]` strictly from `git show <base_sha>:.autofactory/config.toml`.
///
/// Returns `Ok(WorktreeConfig::default())` when `.autofactory/config.toml` does
/// not exist at `base_sha`. Fails closed on invalid `base_sha`, git errors,
/// non-blob entries (e.g. symlinks), or invalid TOML.
#[allow(dead_code)] // Re-export in `config/mod.rs` (outside slice file scope).
pub fn load_base_worktree_config(
    repo_root: &Path,
    base_sha: &str,
) -> Result<WorktreeConfig, ConfigError> {
    let sha = base_sha.trim();
    if sha.is_empty()
        || sha.starts_with('-')
        || sha.contains(':')
        || sha.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(ConfigError::Io {
            path: PathBuf::from(REPO_CONFIG_REL),
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid base_sha {base_sha:?}"),
            ),
        });
    }
    let spec = format!("{sha}:{REPO_CONFIG_REL}");
    let spec_path = PathBuf::from(&spec);
    let io_other = |msg: String| ConfigError::Io {
        path: spec_path.clone(),
        source: io::Error::other(msg),
    };
    let listed = crate::git::cmd::git_raw(repo_root, &["ls-tree", sha, "--", REPO_CONFIG_REL])
        .map_err(|err| io_other(err.to_string()))?;
    if !listed.success() {
        return Err(io_other(format!(
            "git ls-tree {sha} failed: {}",
            listed.stderr.trim()
        )));
    }
    let line = listed.stdout.trim();
    if line.is_empty() {
        return Ok(WorktreeConfig::default());
    }
    let mode = line.split_whitespace().next().unwrap_or("");
    if !matches!(mode, "100644" | "100755") {
        return Err(ConfigError::Parse {
            path: spec_path,
            line: 0,
            column: 0,
            message: format!(
                "{REPO_CONFIG_REL} at {sha} must be a regular file blob (got mode {mode:?})"
            ),
        });
    }
    let shown = crate::git::cmd::git_raw(repo_root, &["show", &spec])
        .map_err(|err| io_other(err.to_string()))?;
    if !shown.success() {
        return Err(io_other(format!(
            "git show {spec} failed: {}",
            shown.stderr.trim()
        )));
    }
    Ok(parse_toml_at::<RepoConfig>(&spec_path, &shown.stdout)?.worktree)
}
