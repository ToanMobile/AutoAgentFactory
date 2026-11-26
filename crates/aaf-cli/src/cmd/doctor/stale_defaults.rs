//! `aaf doctor --fix`: drops the `[run]` lines an older `aaf init` wrote with
//! the then-defaults (`auto_merge = false`, `max_parallel = 1`) from
//! `.autofactory/config.toml`, so the repository follows today's defaults
//! (auto-merge on, `max_parallel = 0` = automatic). A line whose value a
//! person changed is left alone, and nothing is committed.

use std::fs;
use std::path::{Path, PathBuf};

use aaf_engine::config::REPO_CONFIG_REL;

use crate::ctx::{Ctx, RepoState};

/// The `[run]` lines old `aaf init` wrote, as `key = value` after trimming.
const STALE: [(&str, &str); 2] = [("auto_merge", "false"), ("max_parallel", "1")];

/// `text` without the stale lines of its `[run]` table, and the keys removed.
pub fn strip(text: &str) -> (String, Vec<&'static str>) {
    let mut out = String::with_capacity(text.len());
    let mut removed = Vec::new();
    let mut in_run = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_run = trimmed == "[run]";
        } else if in_run && let Some((key, value)) = trimmed.split_once('=') {
            let (key, value) = (key.trim(), value.trim());
            if let Some((stale_key, _)) = STALE.iter().find(|(k, v)| *k == key && *v == value) {
                removed.push(*stale_key);
                continue;
            }
        }
        out.push_str(line);
    }
    (out, removed)
}

/// Fixes the committed config of `repo`; `Ok(None)` when there was nothing to do.
pub fn fix_repo(repo: &Path) -> Result<Option<(PathBuf, Vec<&'static str>)>, String> {
    let path = repo.join(REPO_CONFIG_REL);
    let Ok(text) = fs::read_to_string(&path) else {
        return Ok(None);
    };
    let (fixed, removed) = strip(&text);
    if removed.is_empty() {
        return Ok(None);
    }
    fs::write(&path, fixed).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Some((path, removed)))
}

/// `--fix`: one advisory `fix` check per config changed (or that failed),
/// in this repository, or outside one in every repository aaf has tasks for.
pub(super) fn checks(fix: bool, ctx: &Ctx, repo_state: &RepoState) -> Vec<super::Check> {
    if !fix {
        return Vec::new();
    }
    let repos = match repo_state {
        RepoState::Root(root) => vec![root.clone()],
        _ => known_repo_roots(ctx),
    };
    let mut out: Vec<super::Check> = repos
        .iter()
        .filter_map(|repo| match fix_repo(repo) {
            Ok(None) => None,
            Ok(Some((path, removed))) => Some(Ok(format!(
                "{}: removed {} (commit the file to keep it)",
                path.display(),
                removed.join(", ")
            ))),
            Err(why) => Some(Err(why)),
        })
        .map(|res| super::advisory("fix", res))
        .collect();
    if out.is_empty() {
        out.push(super::advisory("fix", Ok("nothing to change".to_string())));
    }
    out
}

/// Distinct repository roots of the tasks in `aaf.db` that still exist.
fn known_repo_roots(ctx: &Ctx) -> Vec<std::path::PathBuf> {
    use aaf_domain::ports::TaskStore;
    let Ok(Some(db)) = crate::cmd::board::store::open_db_read_only(ctx) else {
        return Vec::new();
    };
    let Ok(tasks) = db.list_tasks(None, None) else {
        return Vec::new();
    };
    // `AAF_DOCTOR_FIX_SKIP`: `:`-separated path fragments never to touch.
    let skip: Vec<String> = std::env::var("AAF_DOCTOR_FIX_SKIP")
        .unwrap_or_default()
        .split(':')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let roots: std::collections::BTreeSet<std::path::PathBuf> = tasks
        .into_iter()
        .filter(|t| !skip.iter().any(|s| t.repo_root.contains(s.as_str())))
        .map(|t| std::path::PathBuf::from(t.repo_root))
        .filter(|p| p.is_dir())
        .collect();
    roots.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::strip;

    #[test]
    fn drops_only_the_old_init_values_in_run() {
        let text = "[gate]\nmax_parallel = 1\n[run]\nexecutor = \"claude\"\n\
                    auto_merge = false\nmax_parallel = 1\nrun_timeout_secs = 3600\n";
        let (out, removed) = strip(text);
        assert_eq!(removed, vec!["auto_merge", "max_parallel"]);
        assert_eq!(
            out,
            "[gate]\nmax_parallel = 1\n[run]\nexecutor = \"claude\"\nrun_timeout_secs = 3600\n"
        );
    }

    #[test]
    fn keeps_values_a_person_chose() {
        let text = "[run]\nauto_merge = true\nmax_parallel = 3\n";
        let (out, removed) = strip(text);
        assert!(removed.is_empty());
        assert_eq!(out, text);
    }
}
