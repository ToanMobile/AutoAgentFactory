//! `.gitignore` update and `--commit` helpers for `aaf init`.

use std::path::Path;

use aaf_engine::config::LOCAL_CONFIG_REL;

use crate::output::CmdError;

use super::settings::LOCAL_SETTINGS_REL;

const INIT_COMMIT_MESSAGE: &str = "chore(aaf): initialize AutoAgentFactory config and hooks";

fn gitignore_with_lines(existing: Option<&str>, required: &[&str]) -> Option<String> {
    let existing = existing.unwrap_or("");
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|entry| {
            !existing
                .lines()
                .map(str::trim)
                .any(|line| line.strip_prefix('/').unwrap_or(line) == *entry)
        })
        .collect();
    if missing.is_empty() {
        return None;
    }
    let mut text = existing.to_string();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    for entry in missing {
        text.push_str(entry);
        text.push('\n');
    }
    Some(text)
}

/// `.gitignore` with the local config and local Claude settings lines appended,
/// or `None` when lines already cover both.
pub(super) fn gitignore_with_local(existing: Option<&str>) -> Option<String> {
    gitignore_with_lines(existing, &[LOCAL_CONFIG_REL, LOCAL_SETTINGS_REL])
}

/// `.gitignore` updated so `.claude/settings.json` is not ignored by repo
/// ignore rules (used by `aaf init --commit` when `Tracking::Ignored`).
pub(crate) fn gitignore_unignore_settings(existing: Option<&str>) -> Option<String> {
    let existing = existing.unwrap_or("");
    let ignores_claude_dir = existing.lines().map(str::trim).any(|line| {
        let stripped = line.strip_prefix('/').unwrap_or(line);
        matches!(stripped, ".claude" | ".claude/")
    });
    // Git refuses to re-include a file inside an ignored parent directory, so
    // when `.claude/` itself is ignored we un-ignore the directory and keep its
    // contents ignored except `.claude/settings.json`.
    let block: &[&str] = if ignores_claude_dir {
        &["!.claude/", ".claude/*", "!.claude/settings.json"]
    } else {
        &["!.claude/settings.json"]
    };
    let tail: Vec<&str> = existing
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if tail.len() >= block.len() && &tail[tail.len() - block.len()..] == block {
        return None;
    }
    let mut text = existing.to_string();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    for line in block {
        text.push_str(line);
        text.push('\n');
    }
    Some(text)
}

pub(super) struct CommitOutcome {
    pub(super) committed: bool,
    pub(super) sha: Option<String>,
    pub(super) files: Vec<String>,
}

pub(super) fn commit_init_files(
    repo_root: &Path,
    paths: &[String],
) -> Result<CommitOutcome, CmdError> {
    let mut existing = Vec::new();
    for rel in paths {
        if repo_root.join(rel).is_file() && !existing.contains(rel) {
            existing.push(rel.clone());
        }
    }
    if existing.is_empty() {
        return Ok(CommitOutcome {
            committed: false,
            sha: aaf_fp::head_sha(repo_root),
            files: Vec::new(),
        });
    }

    let mut normal: Vec<&str> = Vec::new();
    let mut forced: Vec<&str> = Vec::new();
    for rel in &existing {
        let ignored = aaf_fp::git(
            repo_root,
            &["check-ignore", "-q", "--", rel.as_str()],
            &[],
            aaf_fp::GIT_TIMEOUT,
        )
        .is_ok();
        if ignored {
            forced.push(rel.as_str());
        } else {
            normal.push(rel.as_str());
        }
    }
    if !normal.is_empty() {
        let mut add_args = vec!["add", "--"];
        add_args.extend(normal.iter().copied());
        aaf_fp::git(repo_root, &add_args, &[], aaf_fp::GIT_TIMEOUT)
            .map_err(|e| CmdError::new(format!("cannot stage AAF init files: {e}")))?;
    }
    if !forced.is_empty() {
        let mut add_args = vec!["add", "-f", "--"];
        add_args.extend(forced.iter().copied());
        aaf_fp::git(repo_root, &add_args, &[], aaf_fp::GIT_TIMEOUT)
            .map_err(|e| CmdError::new(format!("cannot stage ignored AAF init files: {e}")))?;
    }

    // `git diff --cached --name-only` works on both normal HEAD and unborn HEAD (0 commits).
    let mut diff_args = vec![
        "-c",
        "core.quotepath=false",
        "diff",
        "--cached",
        "--name-only",
        "--",
    ];
    diff_args.extend(existing.iter().map(String::as_str));
    let staged = aaf_fp::git(repo_root, &diff_args, &[], aaf_fp::GIT_TIMEOUT)
        .map_err(|e| CmdError::new(format!("cannot inspect staged AAF init files: {e}")))?;
    let changed_files: Vec<String> = staged
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();

    if changed_files.is_empty() {
        return Ok(CommitOutcome {
            committed: false,
            sha: aaf_fp::head_sha(repo_root),
            files: Vec::new(),
        });
    }

    let mut commit_args = vec!["commit", "--only", "-m", INIT_COMMIT_MESSAGE, "--"];
    commit_args.extend(changed_files.iter().map(String::as_str));
    aaf_fp::git(repo_root, &commit_args, &[], aaf_fp::GIT_TIMEOUT)
        .map_err(|e| CmdError::new(format!("cannot commit AAF init files: {e}")))?;

    Ok(CommitOutcome {
        committed: true,
        sha: aaf_fp::head_sha(repo_root),
        files: changed_files,
    })
}
