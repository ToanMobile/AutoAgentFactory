//! `git push` force/delete/lease checks and `HEAD` branch lookup.

use std::fs;

use crate::{Ctx, paths};

use super::{FIX_FORCE, FIX_PROTECTED, GitCall, Verdict, abbrev, is_protected_with, no_verify};

pub(super) fn push(call: &GitCall, ctx: &Ctx, extra_base: Option<&str>) -> Verdict {
    let (mut force, mut lease, mut delete, mut dry_run) = (false, false, false, false);
    let (mut skip_hooks, mut all, mut repo_option) = (false, false, false);
    let mut positional: Vec<&str> = Vec::new();
    let mut only_positional = false;
    let mut at = 0;
    while let Some(arg) = call.rest.get(at).map(String::as_str) {
        at += 1;
        if only_positional || !arg.starts_with('-') || arg == "-" {
            positional.push(arg);
        } else if arg == "--" {
            only_positional = true;
        } else if arg.starts_with("--") {
            let flag = arg.split('=').next().unwrap_or(arg);
            let is_repo = abbrev(flag, "--repo", 5);
            if is_repo
                || abbrev(flag, "--receive-pack", 6)
                || abbrev(flag, "--exec", 3)
                || abbrev(flag, "--push-option", 4)
            {
                // The value is attached or the next word; either way it is
                // not a flag, even when it looks like `--dry-run`.
                repo_option |= is_repo;
                if flag == arg {
                    at += 1;
                }
            } else if flag == "--dry-run" {
                dry_run = true;
            } else if flag.starts_with("--no-d") {
                // `--no-dry-run` and its abbreviations: the last one wins,
                // as in git. An ambiguous prefix is read the unsafe way.
                dry_run = false;
            } else if abbrev(flag, "--no-verify", 6) {
                skip_hooks = true;
            } else if flag.starts_with("--force-w") {
                lease = true;
            } else if flag.starts_with("--force-i") {
                // --force-if-includes only tightens a lease.
            } else if abbrev(flag, "--force", 3)
                || flag.starts_with("--force")
                || abbrev(flag, "--mirror", 3)
            {
                force = true;
            } else if abbrev(flag, "--delete", 4) {
                delete = true;
            } else if abbrev(flag, "--all", 4) || abbrev(flag, "--branches", 3) {
                all = true;
            }
        } else {
            for (index, flag) in arg.char_indices().skip(1) {
                match flag {
                    'f' => force = true,
                    'n' => dry_run = true,
                    'd' => delete = true,
                    'o' => {
                        if index + 1 == arg.len() {
                            at += 1;
                        }
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
    if dry_run {
        return None;
    }
    if skip_hooks {
        return no_verify(call.rest);
    }
    // With `--repo` naming the remote, no positional is known to be the
    // remote: each one is judged as a refspec.
    let first_refspec = if repo_option { 0 } else { 1 };
    let refspecs = positional.get(first_refspec..).unwrap_or(&[]);
    if force {
        return Some((
            "push --force overwrites remote history".to_string(),
            FIX_FORCE,
        ));
    }
    if let Some(spec) = refspecs.iter().find(|spec| spec.starts_with('+')) {
        return Some((format!("refspec {spec} is a force push"), FIX_FORCE));
    }
    let target = |spec: &str| -> String {
        let spec = spec.trim_start_matches('+');
        let dst = spec.rsplit(':').next().unwrap_or(spec);
        dst.strip_prefix("refs/heads/").unwrap_or(dst).to_string()
    };
    let deleted = refspecs
        .iter()
        .filter(|spec| delete || spec.starts_with(':'))
        .map(|spec| target(spec))
        .find(|branch| is_protected_with(branch, extra_base));
    if let Some(branch) = deleted {
        return Some((
            format!("this deletes the protected branch {branch}"),
            FIX_PROTECTED,
        ));
    }
    if !lease {
        return None;
    }
    if all {
        return Some((
            "--force-with-lease on every branch includes protected ones".to_string(),
            FIX_PROTECTED,
        ));
    }
    let mut targets: Vec<String> = refspecs.iter().map(|spec| target(spec)).collect();
    if targets.is_empty() {
        targets.push("HEAD".to_string());
    }
    for branch in targets {
        let branch = if branch == "HEAD" || branch == "@" {
            match current_branch(call, ctx) {
                Some(current) => current,
                None => {
                    return Some((
                        "cannot tell which branch --force-with-lease would overwrite".to_string(),
                        "name the remote and the feature branch explicitly",
                    ));
                }
            }
        } else {
            branch
        };
        if branch.contains('$') || paths::has_glob(&branch) {
            return Some((
                format!("--force-with-lease target {branch} is not a literal branch"),
                "write the branch name literally",
            ));
        }
        if is_protected_with(&branch, extra_base) {
            return Some((
                format!("--force-with-lease would overwrite the protected branch {branch}"),
                FIX_PROTECTED,
            ));
        }
    }
    None
}

/// Branch checked out where the git call runs, read from `HEAD` (no git spawn).
pub(crate) fn current_branch(call: &GitCall, ctx: &Ctx) -> Option<String> {
    let dir = call.resolve_dir(ctx);
    let git_dir = if let Some(explicit) = call.explicit_git_dir() {
        paths::expand(explicit, &dir, ctx.env.home.as_deref())
    } else if ctx.line.git_dir_moved {
        return None;
    } else {
        let root = (ctx.env.repo_root)(&dir)?;
        let dot_git = root.join(".git");
        if dot_git.is_dir() {
            dot_git
        } else {
            // A linked worktree: `.git` is a file pointing at the real git dir.
            let pointer = fs::read_to_string(&dot_git).ok()?;
            root.join(pointer.trim().strip_prefix("gitdir:")?.trim())
        }
    };
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    head.trim()
        .strip_prefix("ref: refs/heads/")
        .map(str::to_string)
}
