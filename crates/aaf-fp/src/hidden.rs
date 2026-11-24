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

/// Tracked files on disk with a flag that makes git skip them, plus normal
/// tracked `.gitignore` paths (already covered by the fingerprint).
fn flagged(call: &GitCall<'_>) -> Result<(Vec<String>, BTreeSet<String>), FpError> {
    let listing = git_raw(call, &["ls-files", "-v", "-z"], &[], None)
        .map_err(|f| f.into_error("ls-files"))?;
    let mut flagged = Vec::new();
    let mut tracked_gitignores = BTreeSet::new();
    for record in records(&listing) {
        let Some((tag, path)) = record.split_once(' ') else {
            continue;
        };
        // `S` is skip-worktree; a lowercase tag is assume-unchanged.
        let hidden = tag == "S" || (!tag.is_empty() && tag.chars().all(|c| c.is_ascii_lowercase()));
        if hidden {
            // A file the sparse checkout left out is not on disk: no
            // command can read it, and the index entry stands for it.
            if call.repo.join(path).symlink_metadata().is_ok() {
                flagged.push(path.to_string());
            }
        } else if Path::new(path)
            .file_name()
            .is_some_and(|name| name == ".gitignore")
        {
            tracked_gitignores.insert(path.to_string());
        }
    }
    Ok((flagged, tracked_gitignores))
}

/// The `.gitignore` files among `sources` that the fingerprint covers:
/// tracked, or untracked and not ignored themselves.
fn covered(
    call: &GitCall<'_>,
    sources: &BTreeSet<&str>,
    tracked_gitignores: &BTreeSet<String>,
) -> Result<BTreeSet<String>, FpError> {
    let mut known = BTreeSet::new();
    let mut candidates = Vec::new();
    for &source in sources {
        let path = Path::new(source);
        if !path.is_relative() || !path.file_name().is_some_and(|name| name == ".gitignore") {
            continue;
        }
        if tracked_gitignores.contains(source) {
            known.insert(source.to_string());
        } else {
            candidates.push(format!(":(literal){source}"));
        }
    }
    if candidates.is_empty() {
        return Ok(known);
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
    known.extend(records(&listing).map(str::to_string));
    Ok(known)
}

/// First half of the ignored-file scan: lists ignored work-tree entries and
/// maps each to the ignore rule that hid it.
fn scan_ignored(call: &GitCall<'_>) -> Result<Option<(String, String)>, FpError> {
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
    if records(&listing).next().is_none() {
        return Ok(None);
    }
    let asked = Some(listing.clone().into_bytes());
    let answer = match git_raw(call, &["check-ignore", "-v", "-z", "--stdin"], &[], asked) {
        Ok(answer) => answer,
        // Exit 1: none of the paths has a rule of its own.
        Err(Failure::Exit { code: Some(1), .. }) => String::new(),
        Err(failure) => return Err(failure.into_error("check-ignore")),
    };
    Ok(Some((listing, answer)))
}

/// Second half of the ignored-file scan: filters out entries ignored by a
/// `.gitignore` that the fingerprint already covers.
fn resolve_ignored(
    call: &GitCall<'_>,
    scan: Option<(String, String)>,
    tracked_gitignores: &BTreeSet<String>,
) -> Result<Vec<IgnoredFile>, FpError> {
    let Some((listing, answer)) = scan else {
        return Ok(Vec::new());
    };
    let entries: Vec<&str> = records(&listing).collect();
    // Per path: source, line number, pattern, path.
    let fields: Vec<&str> = answer.split_terminator('\0').collect();
    if !fields.len().is_multiple_of(4) {
        return Err(FpError::Git("check-ignore: unexpected output".to_string()));
    }
    let source_of: BTreeMap<&str, &str> = fields.chunks(4).map(|rule| (rule[3], rule[0])).collect();
    let sources: BTreeSet<&str> = source_of.values().copied().collect();
    let covered = covered(call, &sources, tracked_gitignores)?;

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
    let (flagged_res, scan_res) = std::thread::scope(|scope| {
        let flagged_handle = scope.spawn(|| flagged(call));
        let scan_res = scan_ignored(call);
        let flagged_res = flagged_handle
            .join()
            .unwrap_or_else(|_| Err(FpError::Git("ls-files worker panicked".to_string())));
        (flagged_res, scan_res)
    });
    let (flagged, tracked_gitignores) = flagged_res?;
    let ignored = resolve_ignored(call, scan_res?, &tracked_gitignores)?;
    Ok(HiddenFiles { flagged, ignored })
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

    #[test]
    fn tracked_gitignore_skips_covered_subprocess_while_untracked_or_flagged_still_runs_it() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join(".gitignore"), "target/\n").unwrap();
        let log = dir.path().join("covered.log");

        // Case 1: `.gitignore` is normal tracked (`H .gitignore`) -> `covered()` must NOT run.
        let fake_git_tracked = dir.path().join("fake-git-tracked");
        let script_tracked = format!(
            "#!/bin/sh\n\
            case \"$*\" in\n\
              *\"ls-files -v -z\"*)\n\
                printf 'H .gitignore\\0'\n\
                exit 0 ;;\n\
              *\"ls-files --others --ignored\"*)\n\
                printf 'target/\\0'\n\
                exit 0 ;;\n\
              *\"check-ignore -v -z --stdin\"*)\n\
                printf '%s\\0%s\\0%s\\0%s\\0' '.gitignore' '1' 'target/' 'target/'\n\
                exit 0 ;;\n\
              *\"--cached\"*)\n\
                echo \"$*\" >> '{}'\n\
                exit 1 ;;\n\
            esac\n\
            exit 0\n",
            log.display()
        );
        fs::write(&fake_git_tracked, script_tracked).unwrap();
        fs::set_permissions(&fake_git_tracked, fs::Permissions::from_mode(0o755)).unwrap();

        let call = GitCall {
            program: Some(OsStr::new(&fake_git_tracked)),
            repo: &repo,
            timeout: GIT_TIMEOUT,
        };
        let found = hidden_files_using(&call).unwrap();
        assert!(found.is_empty(), "{found:?}");
        assert!(
            !log.exists(),
            "covered() subprocess must be skipped when all .gitignore sources are tracked"
        );

        // Case 2: untracked `sub/.gitignore` + flagged `h .gitignore` -> both go through covered().
        let fake_git_mixed = dir.path().join("fake-git-mixed");
        let script_mixed = format!(
            "#!/bin/sh\n\
            case \"$*\" in\n\
              *\"ls-files -v -z\"*)\n\
                printf 'h .gitignore\\0'\n\
                exit 0 ;;\n\
              *\"ls-files --others --ignored\"*)\n\
                printf 'target/\\0sub/out\\0'\n\
                exit 0 ;;\n\
              *\"check-ignore -v -z --stdin\"*)\n\
                printf '%s\\0%s\\0%s\\0%s\\0' '.gitignore' '1' 'target/' 'target/' 'sub/.gitignore' '1' 'out' 'sub/out'\n\
                exit 0 ;;\n\
              *\"--cached\"*)\n\
                echo \"$*\" >> '{}'\n\
                printf '.gitignore\\0'\n\
                exit 0 ;;\n\
            esac\n\
            exit 0\n",
            log.display()
        );
        fs::write(&fake_git_mixed, script_mixed).unwrap();
        fs::set_permissions(&fake_git_mixed, fs::Permissions::from_mode(0o755)).unwrap();

        let call_mixed = GitCall {
            program: Some(OsStr::new(&fake_git_mixed)),
            repo: &repo,
            timeout: GIT_TIMEOUT,
        };
        let found_mixed = hidden_files_using(&call_mixed).unwrap();
        assert_eq!(found_mixed.flagged, vec![".gitignore"]);
        assert_eq!(
            found_mixed.ignored,
            vec![IgnoredFile {
                path: "sub/out".to_string(),
                source: "sub/.gitignore".to_string(),
            }]
        );
        let invoked = fs::read_to_string(&log).unwrap();
        assert!(invoked.contains(":(literal).gitignore"), "{invoked}");
        assert!(invoked.contains(":(literal)sub/.gitignore"), "{invoked}");
    }
}
