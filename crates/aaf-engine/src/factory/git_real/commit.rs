//! [`RealGit`]'s commit of what an agent left in its worktree. The
//! repository's commit hooks stay on: nothing lands unless they accept it.
//! A failure says whose it is to fix: [`PortError::Conflict`] is the agent's
//! (a hook refused the change, an undeclared nested repository), an error
//! starting with [`COMMIT_DEFERRED`] may pass unchanged later (a hook or
//! signing outlasted its budget, another process held the commit lock), and
//! anything else is the factory's.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::PoisonError;
use std::time::Duration;

use aaf_domain::ports::{PortError, PortResult};

use super::{NO_HOOKS, RealGit, err};
use crate::factory::commit_lock::{self, CommitLock};
use crate::git::GitError;
use crate::git::cmd::{Output, failure, git, git_raw, git_timed};

/// Starts the message of a commit failure that a later try may get past
/// without any change to the tree.
pub(crate) const COMMIT_DEFERRED: &str = "commit deferred";
/// Message of the one commit of what the commit hooks rewrote.
const HOOK_FILES_MESSAGE: &str = "aaf: files changed by the repository's commit hooks";
/// Paths of a tree the hooks keep dirty named in the error.
const DIRTY_PATHS_SHOWN: usize = 10;
/// A signing probe that has not finished by then waits for a passphrase.
const SIGN_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// Hooks whose refusal stops a commit before it is made.
const REFUSING_HOOKS: [&str; 3] = ["pre-commit", "prepare-commit-msg", "commit-msg"];
/// Errors of git itself, never of a hook.
const GIT_INTERNAL: [&str; 4] = ["index.lock", "gpg", "tell me who you are", "no space"];
const SIGNING_FAILED: [&str; 3] = ["gpg failed", "failed to sign", "signing failed"];
/// Characters of a refusing hook's output handed to the agent.
const HOOK_OUTPUT_TAIL: usize = 1500;
const GITLINK_MODE: &str = "160000";

pub(super) fn deferred(why: String) -> PortError {
    PortError::Io(format!("{COMMIT_DEFERRED}: {why}"))
}

fn last_chars(text: &str, max: usize) -> &str {
    let start = text.char_indices().rev().nth(max).map_or(0, |(i, _)| i);
    &text[start..]
}

/// `path` as git printed it from `tree`: relative paths are `tree`'s.
fn in_tree(tree: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        tree.join(path)
    }
}

/// Whether `tree` differs from its `HEAD` after a commit.
fn dirty_after_commit(tree: &Path) -> PortResult<bool> {
    match crate::git::worktree::ensure_clean(tree) {
        Ok(()) => Ok(false),
        Err(GitError::Dirty(_)) => Ok(true),
        Err(error) => Err(err(error)),
    }
}

/// The error for a tree still dirty after the commit, naming the first
/// paths. `followed_up`: a follow-up commit of what the hooks wrote was made;
/// without one, what is left is what `git add` cannot stage.
fn hook_dirty_error(tree: &Path, followed_up: bool) -> PortError {
    // Untrimmed: the first line's status column may start with a space.
    let status = git_raw(tree, &["status", "--porcelain"])
        .map(|out| out.stdout)
        .unwrap_or_default();
    let paths: Vec<&str> = status
        .lines()
        .map(|line| line.get(3..).unwrap_or(line))
        .collect();
    let mut shown = paths
        .iter()
        .take(DIRTY_PATHS_SHOWN)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > DIRTY_PATHS_SHOWN {
        shown.push_str(&format!(" and {} more", paths.len() - DIRTY_PATHS_SHOWN));
    }
    if !followed_up {
        return PortError::Io(format!(
            "uncommitted changes in {}: changes `git add` cannot stage remain after \
             `git commit` (e.g. an embedded repository's own modified or untracked \
             files): {shown}",
            tree.display()
        ));
    }
    PortError::Io(format!(
        "uncommitted changes in {} after `git commit`: a commit hook of the repository \
         changed these files after the commit, again after one follow-up commit: {shown}",
        tree.display()
    ))
}

/// Paths `.gitmodules` in `tree` declares as submodules.
fn declared_submodules(tree: &Path) -> Vec<String> {
    let args = [
        "config",
        "--file",
        ".gitmodules",
        "--get-regexp",
        r"^submodule\..*\.path$",
    ];
    match git_raw(tree, &args) {
        Ok(out) if out.success() => out
            .stdout
            .lines()
            .filter_map(|line| line.split_once(' ').map(|(_, path)| path.to_string()))
            .collect(),
        _ => Vec::new(),
    }
}

/// Paths `git diff --cached --raw -z` shows becoming a gitlink.
fn staged_gitlinks(raw: &str) -> Vec<String> {
    let mut links = Vec::new();
    let mut fields = raw.split('\0').filter(|f| !f.is_empty());
    while let Some(header) = fields.next() {
        let parts: Vec<&str> = header.trim_start_matches(':').split(' ').collect();
        let status = parts.last().copied().unwrap_or_default();
        let paths = if status.starts_with('R') || status.starts_with('C') {
            2
        } else {
            1
        };
        let names: Vec<&str> = fields.by_ref().take(paths).collect();
        let new_link = parts.get(1) == Some(&GITLINK_MODE) && parts.first() != Some(&GITLINK_MODE);
        if let (true, Some(name)) = (new_link, names.last()) {
            links.push((*name).to_string());
        }
    }
    links
}

/// O12-R10-3 (d): a repository the agent created inside its worktree would
/// land as a bare gitlink, its files nowhere. It is unstaged and handed back.
fn refuse_nested_repositories(tree: &Path) -> PortResult<()> {
    let raw = git(tree, &["diff", "--cached", "--raw", "-z"]).map_err(err)?;
    let declared = declared_submodules(tree);
    let nested: Vec<String> = staged_gitlinks(&raw)
        .into_iter()
        .filter(|path| !declared.contains(path))
        .collect();
    if nested.is_empty() {
        return Ok(());
    }
    for path in &nested {
        let spec = format!(":(literal){path}");
        git(tree, &["reset", "--quiet", "--", &spec]).map_err(err)?;
    }
    Err(PortError::Conflict(format!(
        "nothing was committed: {} is a nested git repository of its own inside the \
         worktree and would be committed as a bare submodule link without its files. \
         Remove that nested repository directory, or recreate its files without a git \
         repository of their own",
        nested
            .iter()
            .map(|p| format!("`{p}`"))
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

/// O12-R10-3 (e): with signing on, a key that needs a passphrase nobody can
/// type would hang the commit until its budget ends. One quick signature of
/// the current tree (an object no ref points to) says so first.
fn probe_signing(tree: &Path) -> PortResult<()> {
    let sign = git_raw(tree, &["config", "--bool", "commit.gpgsign"]).map_err(err)?;
    if sign.stdout.trim() != "true" {
        return Ok(());
    }
    let args = [
        "commit-tree",
        "-S",
        "HEAD^{tree}",
        "-m",
        "aaf: signing probe",
    ];
    match git_timed(tree, &args, SIGN_PROBE_TIMEOUT) {
        Ok(out) if out.success() => Ok(()),
        Ok(out) => Err(deferred(format!(
            "signing a commit failed: {}",
            last_chars(out.stderr.trim(), HOOK_OUTPUT_TAIL)
        ))),
        Err(GitError::Timeout { .. }) => Err(deferred(format!(
            "signing a commit did not finish within {} s (a passphrase prompt or an \
             unreachable signing agent)",
            SIGN_PROBE_TIMEOUT.as_secs()
        ))),
        Err(other) => Err(err(other)),
    }
}

/// Whether one of the hooks that can refuse a commit is installed for `tree`.
fn refusing_hook_installed(tree: &Path) -> bool {
    REFUSING_HOOKS.iter().any(|name| {
        let hook = format!("hooks/{name}");
        git(tree, &["rev-parse", "--git-path", &hook]).is_ok_and(|path| {
            fs::metadata(in_tree(tree, &path))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    })
}

/// A commit killed at its budget leaves its index lock behind; nothing else
/// commits in a task worktree while the commit locks are held.
fn drop_stale_index_lock(tree: &Path) {
    if let Ok(path) = git(tree, &["rev-parse", "--git-path", "index.lock"]) {
        let _ = fs::remove_file(in_tree(tree, &path));
    }
}

fn hook_output(out: &Output) -> String {
    let both = format!("{}\n{}", out.stdout.trim(), out.stderr.trim());
    last_chars(both.trim(), HOOK_OUTPUT_TAIL).to_string()
}

impl RealGit {
    /// LEAD C: hooks inside the task's worktree that the task changed are the
    /// agent's own code; run by the commit, they would run unsandboxed.
    fn agent_written_hooks(&self, tree: &Path) -> PortResult<bool> {
        let out = git(tree, &["rev-parse", "--git-path", "hooks"]).map_err(err)?;
        let hooks = in_tree(tree, &out);
        let lexical = hooks
            .components()
            .all(|c| !matches!(c, Component::ParentDir))
            .then(|| hooks.strip_prefix(tree).ok().map(Path::to_path_buf))
            .flatten();
        let rel = match lexical {
            Some(rel) => rel,
            None => match (fs::canonicalize(&hooks), fs::canonicalize(tree)) {
                (Ok(hooks), Ok(root)) => match hooks.strip_prefix(&root) {
                    Ok(rel) => rel.to_path_buf(),
                    Err(_) => return Ok(false),
                },
                _ => return Ok(false),
            },
        };
        let rel = rel.to_string_lossy();
        let spec = if rel.is_empty() {
            ".".to_string()
        } else {
            format!(":(literal){rel}")
        };
        let start = self.start();
        let base = git(tree, &["merge-base", &start, "HEAD"]).unwrap_or(start);
        let diff = git_raw(tree, &["diff", "--quiet", &base, "--", &spec]).map_err(err)?;
        if diff.code != Some(0) {
            return Ok(true);
        }
        let others = git(tree, &["ls-files", "--others", "-z", "--", &spec]).map_err(err)?;
        Ok(!others.is_empty())
    }

    /// One `git commit` in `tree` under the commit budget; `hooks_off` only
    /// for hooks the agent wrote.
    fn run_commit(&self, tree: &Path, message: &str, hooks_off: bool) -> PortResult<()> {
        let mut args = Vec::with_capacity(6);
        if hooks_off {
            args.extend(["-c", NO_HOOKS]);
        }
        args.extend(["commit", "--quiet", "-m", message]);
        let out = git_timed(tree, &args, self.commit_timeout).map_err(|error| match error {
            GitError::Timeout { command } => {
                drop_stale_index_lock(tree);
                deferred(format!(
                    "`{command}` timed out: the repository's commit hooks ran longer than \
                     the commit budget of {} s (possibly while waiting for a heavy slot)",
                    self.commit_timeout.as_secs()
                ))
            }
            other => err(other),
        })?;
        if out.success() {
            return Ok(());
        }
        let stderr = out.stderr.to_ascii_lowercase();
        if SIGNING_FAILED.iter().any(|m| stderr.contains(m)) {
            return Err(deferred(format!(
                "signing the commit failed: {}",
                last_chars(out.stderr.trim(), HOOK_OUTPUT_TAIL)
            )));
        }
        if !hooks_off
            && !GIT_INTERNAL.iter().any(|m| stderr.contains(m))
            && refusing_hook_installed(tree)
        {
            return Err(PortError::Conflict(format!(
                "a commit hook of the repository refused the change: {}",
                hook_output(&out)
            )));
        }
        Err(err(failure(&args, &out)))
    }

    pub(super) fn commit_tree(&self, tree: &Path, message: &str) -> PortResult<()> {
        git(tree, &["add", "-A"]).map_err(err)?;
        if git(tree, &["status", "--porcelain"])
            .map_err(err)?
            .is_empty()
        {
            return Ok(());
        }
        refuse_nested_repositories(tree)?;
        let hooks_off = self.agent_written_hooks(tree)?;
        probe_signing(tree)?;
        // One `git commit` per repository at a time so pre-commit hooks serialize.
        let _queue = self.commit.lock().unwrap_or_else(PoisonError::into_inner);
        let lock = commit_lock::lock_path(&self.locks_dir, &self.repo_root);
        let _lock = CommitLock::acquire(&lock, self.commit_lock_timeout).map_err(|e| match e {
            PortError::Io(why) if why.contains("still committing") => deferred(why),
            other => other,
        })?;
        self.run_commit(tree, message, hooks_off)?;
        // The commit runs the repository's hooks; one that rewrites the
        // working tree afterwards would let the gate judge a tree other
        // than the commit that lands. What they wrote is committed once
        // (hooks on, same locks); a tree still dirty after that is an error.
        if !dirty_after_commit(tree)? {
            return Ok(());
        }
        git(tree, &["add", "-A"]).map_err(err)?;
        // Nothing staged (a change `add` cannot take): no commit to make.
        let args = ["diff", "--cached", "--quiet"];
        let staged = git_raw(tree, &args).map_err(err)?;
        let followed_up = match staged.code {
            Some(0) => false,
            Some(1) => {
                self.run_commit(tree, HOOK_FILES_MESSAGE, hooks_off)?;
                true
            }
            _ => return Err(err(failure(&args, &staged))),
        };
        if !dirty_after_commit(tree)? {
            return Ok(());
        }
        Err(hook_dirty_error(tree, followed_up))
    }
}
