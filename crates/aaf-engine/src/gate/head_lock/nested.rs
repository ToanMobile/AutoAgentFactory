//! Locked files inside nested repositories and gitlinks, which git reports
//! as a single entry whatever they hold (audit GATE-089).

use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use aaf_domain::gate::ChangeKind;

use super::dir::gone;
use super::{LockMatchers, locked_path};
use crate::gate::changes::Change;
use crate::gate::proc::git_bytes;
use crate::gate::{GIT_TIMEOUT, GateError};

const GITLINK: &str = "160000";
const SYMLINK: &str = "120000";
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const MAX_WALK_ENTRIES: usize = 100_000;
/// Every git call reading gitlink objects: a replace ref must not swap them.
const NO_REPLACE: &str = "--no-replace-objects";
const LITERAL: [&str; 2] = [NO_REPLACE, "--literal-pathspecs"];

/// `(path, sha)` of the gitlinks in `ls-tree -z` (`sha_at` 2) or
/// `ls-files -s -z` (`sha_at` 1) output.
fn gitlinks(raw: &[u8], sha_at: usize) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(raw);
    let mut out = Vec::new();
    for line in text.split('\0') {
        if let Some((meta, path)) = line.split_once('\t') {
            let fields: Vec<&str> = meta.split(' ').collect();
            if fields.first() == Some(&GITLINK)
                && let Some(sha) = fields.get(sha_at)
            {
                out.push((path.to_string(), (*sha).to_string()));
            }
        }
    }
    out
}

/// `[path, path at base]` of `change`, without a trailing `/`.
fn paths(change: &Change) -> [&str; 2] {
    let path = change.path.trim_end_matches('/');
    let old = change.old_path.as_deref().unwrap_or(path);
    [path, old.trim_end_matches('/')]
}

/// The gitlink commit at each changed path: at base, in HEAD, in the index.
#[derive(Default)]
struct Links {
    base: HashMap<String, String>,
    head: HashMap<String, String>,
    index: HashMap<String, String>,
}

/// Finds the changes that hide locked files in a nested repository or a
/// gitlink, against `base`.
pub(super) struct Probe<'a> {
    root: &'a Path,
    base: &'a str,
    changes: &'a [Change],
    /// Read once, for every changed path, on first use.
    links: Option<Links>,
}

impl<'a> Probe<'a> {
    pub(super) fn new(root: &'a Path, base: &'a str, changes: &'a [Change]) -> Self {
        let links = None;
        Self {
            root,
            base,
            changes,
            links,
        }
    }

    fn read_links(&self) -> Result<Links, GateError> {
        let mut all: Vec<&str> = self.changes.iter().flat_map(paths).collect();
        all.sort_unstable();
        all.dedup();
        // An unborn HEAD (no commit yet) holds no gitlink.
        let verify = [NO_REPLACE, "rev-parse", "-q", "--verify", "HEAD^{commit}"];
        let head = aaf_fp::git_or_none(self.root, &verify, GIT_TIMEOUT)?;
        let mut links = Links::default();
        for chunk in all.chunks(500) {
            let ls = |cmd: &[&str], sha_at| -> Result<Vec<_>, GateError> {
                let args = [&LITERAL[..], cmd, &["--"], chunk].concat();
                Ok(gitlinks(&git_bytes(self.root, &args, GIT_TIMEOUT)?, sha_at))
            };
            if self.base != EMPTY_TREE {
                links.base.extend(ls(&["ls-tree", "-z", self.base], 2)?);
            }
            if let Some(head) = &head {
                links.head.extend(ls(&["ls-tree", "-z", head], 2)?);
            }
            links.index.extend(ls(&["ls-files", "-s", "-z"], 1)?);
        }
        Ok(links)
    }

    /// The gitlink commits of `change` at base (its old path), in HEAD and
    /// in the index, each `None` where that path is no gitlink.
    fn slots(&mut self, change: &Change) -> Result<[Option<String>; 3], GateError> {
        if self.links.is_none() {
            self.links = Some(self.read_links()?);
        }
        let links = self.links.get_or_insert_with(Links::default);
        let [path, old] = paths(change);
        Ok([
            links.base.get(old).cloned(),
            links.head.get(path).cloned(),
            links.index.get(path).cloned(),
        ])
    }

    /// `gitlink:<base>:<head>:<index>` when `change` is a gitlink anywhere:
    /// bound into the fingerprint, an approval covers only those commits,
    /// whatever (or nothing) is on disk.
    pub(super) fn link(&mut self, change: &Change) -> Result<Option<String>, GateError> {
        let slots = self.slots(change)?;
        if slots.iter().all(Option::is_none) {
            return Ok(None);
        }
        let [base, head, index] = slots.map(Option::unwrap_or_default);
        Ok(Some(format!("gitlink:{base}:{head}:{index}")))
    }

    /// Whether `change` (not locked by its own path) holds a locked file. A
    /// gitlink whose repository cannot be read is locked: nothing shows what
    /// it holds.
    pub(super) fn locks(&mut self, change: &Change, m: &LockMatchers) -> Result<bool, GateError> {
        let [base, head, index] = self.slots(change)?;
        let [path, _] = paths(change);
        let full = self.root.join(path);
        let is_dir = match fs::symlink_metadata(&full) {
            Ok(meta) => Some(meta.is_dir()),
            Err(err) if gone(&err) => None,
            Err(err) => return Err(err.into()),
        };
        if base.is_none() && head.is_none() && index.is_none() {
            // An untracked nested repository: only its files on disk exist.
            return Ok(is_dir == Some(true) && walk_locked(self.root, path, m)?);
        }
        if base.is_some() && matches!(change.kind, ChangeKind::Deleted | ChangeKind::Renamed) {
            // Only missing on disk while HEAD and the index still hold the
            // base commit: no change to the gitlink at all.
            let kept = is_dir.is_none() && head == base && index == base;
            return Ok(!(change.kind == ChangeKind::Deleted && kept));
        }
        let mut heads: Vec<String> = [head, index].into_iter().flatten().collect();
        heads.dedup();
        let old = match (change.kind, base) {
            (ChangeKind::Modified, Some(base)) => base,
            _ => EMPTY_TREE.to_string(),
        };
        // A gitlink turned into a file, symlink or nothing on disk, or gone
        // from HEAD and the index, hides what it held.
        Ok(is_dir != Some(true)
            || heads.is_empty()
            || !own_repo(&full)
            || (old == EMPTY_TREE && walk_locked(self.root, path, m)?)
            || sub_locked(self.root, path, &old, &heads, m).unwrap_or(true))
    }
}

/// Whether `dir` is the top of a git repository of its own, not a plain
/// directory of the repository around it.
fn own_repo(dir: &Path) -> bool {
    let Ok(raw) = git_bytes(dir, &["rev-parse", "--show-toplevel"], GIT_TIMEOUT) else {
        return false;
    };
    let top = String::from_utf8_lossy(&raw)
        .trim_end_matches('\n')
        .to_string();
    matches!((fs::canonicalize(top), fs::canonicalize(dir)), (Ok(a), Ok(b)) if a == b)
}

fn locked_at(root: &Path, kind: ChangeKind, path: String, m: &LockMatchers) -> bool {
    let change = Change {
        kind,
        path,
        old_path: None,
    };
    change.path.contains('\u{FFFD}') || locked_path(root, &change, m).is_some()
}

/// Whether a `diff --raw -z` of the repository at `prefix` changes a locked
/// path, a gitlink, or a symlink at a locked directory (by its git mode: the
/// disk may no longer hold it).
fn raw_locked(root: &Path, prefix: &str, raw: &[u8], m: &LockMatchers) -> bool {
    let mut fields = raw.split(|b| *b == 0).filter(|f| !f.is_empty());
    while let Some(head) = fields.next() {
        let head = String::from_utf8_lossy(head);
        let Some(path) = fields.next() else {
            return true;
        };
        let path = format!("{prefix}/{}", String::from_utf8_lossy(path));
        let parts: Vec<&str> = head.trim_start_matches(':').split(' ').collect();
        let kind = match parts.get(4).and_then(|s| s.chars().next()) {
            Some('A') => ChangeKind::Added,
            Some('D') => ChangeKind::Deleted,
            _ => ChangeKind::Modified,
        };
        if parts.len() != 5
            || parts[..2].contains(&GITLINK)
            || (parts[..2].contains(&SYMLINK) && m.matches_dir_prefix(&path))
            || locked_at(root, kind, path, m)
        {
            return true;
        }
    }
    false
}

/// Whether the repository at `prefix` changed a locked path, a gitlink or a
/// nested repository since `old`: in each commit of `heads`, in its index,
/// on disk, or behind a `skip-worktree` / `assume-unchanged` flag.
fn sub_locked(
    root: &Path,
    prefix: &str,
    old: &str,
    heads: &[String],
    m: &LockMatchers,
) -> Result<bool, GateError> {
    let sub = root.join(prefix);
    let diff = [
        NO_REPLACE,
        "diff",
        "--raw",
        "-z",
        "--no-renames",
        "--no-ext-diff",
        "--ignore-submodules=none",
    ];
    let mut runs: Vec<Vec<&str>> = heads
        .iter()
        .filter(|h| *h != old)
        .map(|h| [&diff[..], &[old, h.as_str()]].concat())
        .collect();
    runs.push([&diff[..], &[old]].concat());
    runs.push([&diff[..], &["--cached", old]].concat());
    for args in runs {
        if raw_locked(root, prefix, &git_bytes(&sub, &args, GIT_TIMEOUT)?, m) {
            return Ok(true);
        }
    }
    // `S` is skip-worktree, a lowercase tag assume-unchanged: git no longer
    // compares those files on disk.
    let raw = git_bytes(&sub, &[NO_REPLACE, "ls-files", "-v", "-z"], GIT_TIMEOUT)?;
    for entry in raw.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let entry = String::from_utf8_lossy(entry);
        let Some((tag, path)) = entry.split_once(' ') else {
            return Ok(true);
        };
        let path = format!("{prefix}/{path}");
        let flagged = tag == "S" || tag.chars().any(|c| c.is_ascii_lowercase());
        if flagged
            && (m.matches_any(&path) || m.matches_dir_prefix(&path) || path.contains('\u{FFFD}'))
        {
            return Ok(true);
        }
    }
    let others = [
        NO_REPLACE,
        "ls-files",
        "--others",
        "--exclude-standard",
        "-z",
    ];
    let raw = git_bytes(&sub, &others, GIT_TIMEOUT)?;
    Ok(String::from_utf8_lossy(&raw)
        .split('\0')
        .filter(|p| !p.is_empty())
        .any(|p| {
            p.ends_with('/') || locked_at(root, ChangeKind::Added, format!("{prefix}/{p}"), m)
        }))
}

/// Whether a file under `prefix` on disk would be locked if added to the
/// repository around it. A symlink to a directory is locked: its files are
/// not walked. Fails closed past [`MAX_WALK_ENTRIES`].
fn walk_locked(root: &Path, prefix: &str, m: &LockMatchers) -> Result<bool, GateError> {
    let mut stack = vec![prefix.to_string()];
    let mut seen = 0usize;
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(root.join(&dir))? {
            let entry = entry?;
            if entry.file_name() == ".git" {
                continue;
            }
            seen += 1;
            if seen > MAX_WALK_ENTRIES {
                return Err(GateError::Config(format!(
                    "nested repository {prefix} exceeds {MAX_WALK_ENTRIES} entries"
                )));
            }
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                return Ok(true);
            };
            let rel = format!("{dir}/{name}");
            let kind = entry.file_type()?;
            if kind.is_dir() {
                stack.push(rel.clone());
            } else if kind.is_symlink() {
                match fs::metadata(root.join(&rel)) {
                    Ok(meta) if meta.is_dir() => return Ok(true),
                    Err(err) if err.kind() != ErrorKind::NotFound => return Ok(true),
                    _ => {}
                }
            }
            if locked_at(root, ChangeKind::Added, rel, m) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
