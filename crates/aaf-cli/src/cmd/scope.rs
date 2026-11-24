//! Shared `--repo` / `--all` scope resolution for `aaf ui`, `aaf telegram`, and `aaf watch`.

use std::fs;
use std::path::{Path, PathBuf};

use crate::ctx::Ctx;
use crate::output::CmdError;

/// Resolves `(display_scope, optional_repo_root, config_base)` from `--repo` and `--all`.
pub(crate) fn resolve_scope(
    repo: Option<&Path>,
    all: bool,
    ctx: &Ctx,
) -> Result<(String, Option<PathBuf>, PathBuf), CmdError> {
    if all {
        return Ok(("all".to_string(), None, ctx.cwd.clone()));
    }
    let target = match repo {
        Some(r) => {
            let cand = if r.is_absolute() {
                r.to_path_buf()
            } else {
                ctx.cwd.join(r)
            };
            if !cand.exists() {
                return Err(CmdError::new(format!(
                    "--repo {} does not exist",
                    cand.display()
                )));
            }
            aaf_fp::repo_root(&cand).unwrap_or(cand)
        }
        None => match aaf_fp::repo_root(&ctx.cwd) {
            Ok(root) => root,
            Err(_) => return Ok(("all (outside git repo)".to_string(), None, ctx.cwd.clone())),
        },
    };
    let canonical = fs::canonicalize(&target).unwrap_or(target);
    Ok((
        canonical.display().to_string(),
        Some(canonical.clone()),
        canonical,
    ))
}
