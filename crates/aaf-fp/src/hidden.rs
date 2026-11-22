//! Files on disk that the fingerprint does not cover although nothing in
//! the work tree says so.
//!
//! `tree_fp` is `git add -A` into a copy of the index, so it leaves out
//! what git leaves out. Two of git's ways to leave a file out are invisible
//! in the work tree itself and writable by an agent working in it: an index
//! flag (skip-worktree, assume-unchanged) on a tracked file, and an ignore
//! rule that lives outside the tree (`info/exclude`, an excludes file named
//! by the configuration, an untracked `.gitignore` that ignores itself).
//! Commands run in the work tree read such files; a receipt for its
//! fingerprint would sign a tree that is not what ran.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use crate::proc::Failure;
use crate::{FpError, GitCall, git_raw};

/// A file on disk that an ignore rule outside the fingerprint hides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoredFile {
    /// Relative to the repository root; a directory ends with `/`.
    pub path: String,
    /// The file holding the rule, as git names it.
    pub source: String,
}

/// What [`hidden_files`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HiddenFiles {
    /// Tracked files that are on disk and marked skip-worktree or
    /// assume-unchanged: git does not look at their content.
    pub flagged: Vec<String>,
    pub ignored: Vec<IgnoredFile>,
}

/// How many file names one reason spells out.
const NAMED: usize = 5;

fn named(paths: &[&str]) -> String {
    let mut text = paths
        .iter()
        .take(NAMED)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > NAMED {
        text.push_str(&format!(" (+{} more)", paths.len() - NAMED));
    }
    text
}

impl HiddenFiles {
    pub fn is_empty(&self) -> bool {
        self.flagged.is_empty() && self.ignored.is_empty()
    }

    /// One sentence per cause, naming the files and the way out.
    pub fn reasons(&self) -> Vec<String> {
        let mut reasons = Vec::new();
        if !self.flagged.is_empty() {
            let paths: Vec<&str> = self.flagged.iter().map(String::as_str).collect();
            reasons.push(format!(
                "{} tracked file(s) on disk are marked skip-worktree or assume-unchanged, \
                 so their content is not in the fingerprint: {}; clear the mark with \
                 `git update-index --no-skip-worktree --no-assume-unchanged -- <file>`",
                paths.len(),
                named(&paths)
            ));
        }
        let mut by_source: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for file in &self.ignored {
            by_source.entry(&file.source).or_default().push(&file.path);
        }
        for (source, paths) in by_source {
            reasons.push(format!(
                "{} file(s) on disk are ignored only by {source}, which the fingerprint \
                 does not cover, so they are not in it either: {}; delete them or ignore \
                 them in a committed .gitignore",
                paths.len(),
                named(&paths)
            ));
        }
        reasons
    }
}

/// Where git found no rule for a path it lists as ignored.
const NO_RULE: &str = "a rule git does not name";

fn records(listing: &str) -> impl Iterator<Item = &str> {
    listing.split('\0').filter(|record| !record.is_empty())
}

/// Tracked files that are on disk with a flag that makes git skip them.
fn flagged(call: &GitCall<'_>) -> Result<Vec<String>, FpError> {
    let listing = git_raw(call, &["ls-files", "-v", "-z"], &[], None)
        .map_err(|f| f.into_error("ls-files"))?;
    Ok(records(&listing)
        .filter_map(|record| {
            let (tag, path) = record.split_once(' ')?;
            // `S` is skip-worktree; a lowercase tag is assume-unchanged.
            let hidden =
                tag == "S" || (!tag.is_empty() && tag.chars().all(|c| c.is_ascii_lowercase()));
            // A file the sparse checkout left out is not on disk: no
            // command can read it, and the index entry stands for it.
            (hidden && call.repo.join(path).symlink_metadata().is_ok()).then(|| path.to_string())
        })
        .collect())
}

/// The `.gitignore` files among `sources` that the fingerprint covers:
/// tracked, or untracked and not ignored themselves.
fn covered(call: &GitCall<'_>, sources: &BTreeSet<&str>) -> Result<BTreeSet<String>, FpError> {
    let candidates: Vec<String> = sources
        .iter()
        .filter(|source| {
            let path = Path::new(source);
            path.is_relative() && path.file_name().is_some_and(|name| name == ".gitignore")
        })
        .map(|source| format!(":(literal){source}"))
        .collect();
    if candidates.is_empty() {
        return Ok(BTreeSet::new());
    }
    let mut args = vec![
        "ls-files",
        "-z",
        "--cached",
        "--others",
        "--exclude-standard",
        "--",
    ];
    args.extend(candidates.iter().map(String::as_str));
    let listing = git_raw(call, &args, &[], None).map_err(|f| f.into_error("ls-files"))?;
    Ok(records(&listing).map(str::to_string).collect())
}

/// Files on disk that are ignored by a rule the fingerprint does not cover.
fn ignored(call: &GitCall<'_>) -> Result<Vec<IgnoredFile>, FpError> {
    // `--directory`: a directory that is ignored as a whole (`target/`,
    // `node_modules/`) is one entry, and git does not walk into it.
    let list = [
        "ls-files",
        "--others",
        "--ignored",
        "--exclude-standard",
        "--directory",
        "-z",
    ];
    let listing = git_raw(call, &list, &[], None).map_err(|f| f.into_error("ls-files"))?;
    let entries: Vec<&str> = records(&listing).collect();
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let asked = Some(listing.clone().into_bytes());
    let answer = match git_raw(call, &["check-ignore", "-v", "-z", "--stdin"], &[], asked) {
        Ok(answer) => answer,
        // Exit 1: none of the paths has a rule of its own.
        Err(Failure::Exit { code: Some(1), .. }) => String::new(),
        Err(failure) => return Err(failure.into_error("check-ignore")),
    };
    // Per path: source, line number, pattern, path.
    let fields: Vec<&str> = answer.split_terminator('\0').collect();
    if !fields.len().is_multiple_of(4) {
        return Err(FpError::Git("check-ignore: unexpected output".to_string()));
    }
    let source_of: BTreeMap<&str, &str> = fields.chunks(4).map(|rule| (rule[3], rule[0])).collect();
    let sources: BTreeSet<&str> = source_of.values().copied().collect();
    let covered = covered(call, &sources)?;

    let mut hidden = Vec::new();
    for (at, path) in entries.iter().enumerate() {
        let source = match source_of.get(path) {
            Some(source) if covered.contains(*source) => continue,
            Some(source) => source,
            // A directory git lists only because everything in it is
            // ignored: its files follow (the listing is sorted) and each
            // is judged by its own rule.
            None if path.ends_with('/')
                && entries
                    .get(at + 1)
                    .is_some_and(|next| next.starts_with(path)) =>
            {
                continue;
            }
            None => NO_RULE,
        };
        hidden.push(IgnoredFile {
            path: (*path).to_string(),
            source: source.to_string(),
        });
    }
    Ok(hidden)
}

/// The files on disk in `repo_root` that [`crate::tree_fp`] does not cover
/// for a reason the work tree does not show. A gate must not run, and a
/// receipt must not stand, while this is not empty.
///
/// Not reported, because the work tree itself says so and the rule is part
/// of the fingerprint: files ignored by a `.gitignore` that is tracked (or
/// untracked and not ignored). Not reported either: tracked files a sparse
/// checkout left out, which are not on disk.
pub fn hidden_files(repo_root: &Path, timeout: Duration) -> Result<HiddenFiles, FpError> {
    hidden_files_using(&GitCall::plain(repo_root, timeout))
}

pub(crate) fn hidden_files_using(call: &GitCall<'_>) -> Result<HiddenFiles, FpError> {
    Ok(HiddenFiles {
        flagged: flagged(call)?,
        ignored: ignored(call)?,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::ffi::OsStr;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::GIT_TIMEOUT;

    #[test]
    fn empty_ls_files_tag_is_not_treated_as_assume_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        for f in [
            "leading_space.txt",
            "normal.txt",
            "assumed.txt",
            "sparse.txt",
        ] {
            fs::write(repo.join(f), "x\n").unwrap();
        }
        let fake_git = dir.path().join("fake-git");
        let script = "#!/bin/sh\n\
            for arg in \"$@\"; do\n\
              if [ \"$arg\" = \"-v\" ]; then\n\
                printf ' leading_space.txt\\0H normal.txt\\0h assumed.txt\\0S sparse.txt\\0'\n\
                exit 0\n\
              fi\n\
            done\n\
            exit 0\n";
        fs::write(&fake_git, script).unwrap();
        fs::set_permissions(&fake_git, fs::Permissions::from_mode(0o755)).unwrap();

        let call = GitCall {
            program: Some(OsStr::new(&fake_git)),
            repo: &repo,
            timeout: GIT_TIMEOUT,
        };
        let found = hidden_files_using(&call).unwrap();
        assert_eq!(found.flagged, vec!["assumed.txt", "sparse.txt"]);
    }
}
