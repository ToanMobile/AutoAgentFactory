//! Read-only overlay, ancestor pinning, and read-denied masking for Bubblewrap.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::bwrap::{
    abs_path_str, canonical_or_self, is_leaf_symlink, push_flag_pair, push_flag_path,
};

pub(super) struct RoOverlay<'a> {
    argv: &'a mut Vec<String>,
    bound_rw: &'a mut BTreeSet<String>,
    bound_ro: &'a mut BTreeSet<String>,
    pinned: &'a mut BTreeSet<String>,
}

fn deepest_enclosing(set: &BTreeSet<String>, target_canon: &Path) -> Option<PathBuf> {
    set.iter()
        .map(|entry| canonical_or_self(Path::new(entry)))
        .filter(|cand| target_canon.starts_with(cand))
        .max_by_key(|cand| cand.components().count())
}

fn contains_canon(set: &BTreeSet<String>, target_canon: &Path) -> bool {
    target_canon.to_str().is_some_and(|text| set.contains(text))
        || set
            .iter()
            .any(|entry| canonical_or_self(Path::new(entry)) == target_canon)
}

/// RFC-126: create a missing `logs` dir (non-recursive, without following
/// symlinks) so `--ro-bind` always mounts; fail closed on error.
fn ensure_logs_dir(logs: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(logs) {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir(logs)
            .map_err(|err| format!("RFC-126: failed to create {}: {err}", logs.display())),
        Err(err) => Err(format!("RFC-126: failed to stat {}: {err}", logs.display())),
    }
}

/// SEC-129: resolves the active linked worktree's admin dir
/// (`<git_dir>/worktrees/<name>`, canonicalized) by walking up from `worktree`
/// to its `.git` gitfile, matching `executors/sandbox/git_dir.rs::linked_worktree_admin_dir`.
fn linked_worktree_admin_dir(worktree: &Path, git_dir: &Path) -> Option<PathBuf> {
    let dot_git = worktree
        .ancestors()
        .map(|dir| dir.join(".git"))
        .find(|candidate| candidate.symlink_metadata().is_ok())?;
    let text = match std::fs::metadata(&dot_git) {
        Ok(meta) if meta.is_file() => std::fs::read_to_string(&dot_git).unwrap_or_default(),
        _ => return None,
    };
    let target = text.lines().next()?.strip_prefix("gitdir:")?;
    let base = dot_git.parent().unwrap_or(worktree);
    let admins = std::fs::canonicalize(git_dir.join("worktrees")).ok()?;
    let target = std::fs::canonicalize(base.join(target.trim())).ok()?;
    Some(admins.join(target.strip_prefix(&admins).ok()?.iter().next()?))
}

/// SEC-129: matches primary-checkout pseudo-refs and merge/rebase/bisect/index
/// state at the root of a common git dir (mirrors macOS Seatbelt rules in
/// `executors/sandbox/git_dir.rs`).
fn is_common_pseudo_ref_name(name: &str) -> bool {
    if matches!(name, "rebase-merge" | "rebase-apply" | "sequencer") {
        return true;
    }
    let base = name.strip_suffix(".lock").unwrap_or(name);
    base.ends_with("HEAD")
        || matches!(
            base,
            "index"
                | "AUTO_MERGE"
                | "MERGE_MSG"
                | "MERGE_MODE"
                | "MERGE_RR"
                | "MERGE_AUTOSTASH"
                | "SQUASH_MSG"
                | "COMMIT_EDITMSG"
        )
        || base.starts_with("BISECT_")
        || base.starts_with("sharedindex.")
}

impl<'a> RoOverlay<'a> {
    pub(super) fn new(
        argv: &'a mut Vec<String>,
        bound_rw: &'a mut BTreeSet<String>,
        bound_ro: &'a mut BTreeSet<String>,
        pinned: &'a mut BTreeSet<String>,
    ) -> Self {
        Self {
            argv,
            bound_rw,
            bound_ro,
            pinned,
        }
    }

    pub(super) fn bind_rw(&mut self, path: &str) {
        push_flag_pair(self.argv, self.bound_rw, "--bind", path);
    }

    /// Self-binds intermediate dirs between a writable root and `path.parent()`
    /// so `mv` fails with `EBUSY` instead of bypassing RO overlays below them.
    fn pin_ancestors(&mut self, path: &Path) -> Result<(), String> {
        let Some(parent) = path.parent() else {
            return Ok(());
        };
        let parent_canon = canonical_or_self(parent);
        let Some(rw_root) = deepest_enclosing(self.bound_rw, &parent_canon) else {
            return Ok(());
        };
        if let Some(ro_root) = deepest_enclosing(self.bound_ro, &parent_canon)
            && ro_root.components().count() >= rw_root.components().count()
        {
            return Ok(());
        }
        let Ok(rel) = parent_canon.strip_prefix(&rw_root) else {
            return Ok(());
        };
        let mut cur = rw_root;
        for comp in rel.components() {
            cur.push(comp);
            let cur_canon = canonical_or_self(&cur);
            if !cur_canon.is_dir()
                || contains_canon(self.bound_ro, &cur_canon)
                || contains_canon(self.bound_rw, &cur_canon)
            {
                continue;
            }
            let text = abs_path_str(&cur_canon, "pin path")?;
            push_flag_pair(self.argv, self.pinned, "--bind", text);
        }
        Ok(())
    }

    fn mask_one(&mut self, target: &Path, is_dir: bool) -> Result<(), String> {
        self.pin_ancestors(target)?;
        let s = abs_path_str(target, "read_denied path")?.to_string();
        if self.bound_ro.insert(s.clone()) {
            if is_dir {
                self.argv.extend(["--tmpfs".into(), s]);
            } else {
                self.argv
                    .extend(["--ro-bind".into(), "/dev/null".into(), s]);
            }
        }
        Ok(())
    }

    pub(super) fn mask_read_denied(&mut self, path: &Path) -> Result<(), String> {
        let leaf_sym = is_leaf_symlink(path);
        if !leaf_sym && path.exists() {
            self.mask_one(path, path.is_dir())?;
        }
        if let Ok(resolved) = std::fs::canonicalize(path)
            && (leaf_sym || resolved != path)
            && resolved.exists()
        {
            self.mask_one(&resolved, resolved.is_dir())?;
        }
        Ok(())
    }

    pub(super) fn bind_with_target(&mut self, path: &Path) -> Result<(), String> {
        self.pin_ancestors(path)?;
        push_flag_path(self.argv, self.bound_ro, "--ro-bind", path)?;
        if let Ok(resolved) = std::fs::canonicalize(path)
            && resolved != path
            && resolved.exists()
        {
            self.pin_ancestors(&resolved)?;
            push_flag_path(self.argv, self.bound_ro, "--ro-bind", &resolved)?;
        }
        Ok(())
    }

    pub(super) fn protect_existing(&mut self, path: &Path) -> Result<(), String> {
        if path.exists() {
            self.bind_with_target(path)?;
        }
        Ok(())
    }

    /// SEC-129: lock existing pseudo-refs and merge/rebase/bisect/index state
    /// at the root of the common git dir for linked worktrees.
    fn protect_common_pseudo_refs(&mut self, git_dir: &Path) -> Result<(), String> {
        let Ok(entries) = std::fs::read_dir(git_dir) else {
            return Ok(());
        };
        let mut matches: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(is_common_pseudo_ref_name)
            })
            .map(|e| e.path())
            .collect();
        matches.sort();
        for path in matches {
            self.protect_existing(&path)?;
        }
        Ok(())
    }

    pub(super) fn protect_git_dir(
        &mut self,
        git_dir: &Path,
        worktree: Option<&Path>,
        is_primary_checkout: bool,
    ) -> Result<(), String> {
        for rel in ["hooks", "refs/aaf", "config", "config.worktree"] {
            self.protect_existing(&git_dir.join(rel))?;
        }
        for rel in ["commondir", "info/exclude", "packed-refs"] {
            self.protect_existing(&git_dir.join(rel))?;
        }
        if !is_primary_checkout {
            for rel in ["HEAD", "index", "refs/heads/main", "refs/heads/master"] {
                self.protect_existing(&git_dir.join(rel))?;
            }
            for rel in ["refs/heads/develop", "refs/heads/release", "refs/remotes"] {
                self.protect_existing(&git_dir.join(rel))?;
            }
            // SEC-129: lock existing pseudo-refs and primary checkout state at
            // the root of the common git dir.
            self.protect_common_pseudo_refs(git_dir)?;
            // RFC-126: lock common reflog tree for linked worktrees; create
            // missing `logs` so `--ro-bind` always mounts.
            let logs = git_dir.join("logs");
            ensure_logs_dir(&logs)?;
            self.protect_existing(&logs)?;
        }
        let worktrees = git_dir.join("worktrees");
        if worktrees.is_dir() {
            self.bind_with_target(&worktrees)?;
            let own_admin = (!is_primary_checkout)
                .then(|| worktree.and_then(|wt| linked_worktree_admin_dir(wt, git_dir)))
                .flatten();
            if let Ok(entries) = std::fs::read_dir(&worktrees) {
                let mut children: Vec<PathBuf> = entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect();
                children.sort();
                for child in children {
                    let is_own = !is_leaf_symlink(&child)
                        && own_admin
                            .as_ref()
                            .is_some_and(|own| canonical_or_self(&child) == *own);
                    if !is_primary_checkout && !is_own {
                        // SEC-129: sibling (or unresolvable) worktree admin dirs stay RO.
                        self.bind_with_target(&child)?;
                        continue;
                    }
                    push_flag_path(self.argv, self.bound_rw, "--bind", &child)?;
                    for rel in ["hooks", "config", "config.worktree", "commondir", "gitdir"] {
                        self.protect_existing(&child.join(rel))?;
                    }
                    if !is_primary_checkout {
                        // RFC-126: lock own worktree's reflog dir too.
                        let logs = child.join("logs");
                        ensure_logs_dir(&logs)?;
                        self.protect_existing(&logs)?;
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn protect_prefix(&mut self, prefix_path: &Path) -> Result<(), String> {
        let (Some(parent), Some(prefix_name)) = (
            prefix_path.parent(),
            prefix_path.file_name().and_then(|n| n.to_str()),
        ) else {
            return Ok(());
        };
        if let Ok(entries) = std::fs::read_dir(parent) {
            let mut matches: Vec<PathBuf> = entries
                .flatten()
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|n| n.starts_with(prefix_name))
                })
                .map(|e| e.path())
                .collect();
            matches.sort();
            for path in matches {
                self.protect_existing(&path)?;
            }
        }
        Ok(())
    }
}
