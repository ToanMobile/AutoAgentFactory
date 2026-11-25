//! The change set relative to the base: changed paths (including untracked
//! files) and the lines they add.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use aaf_domain::gate::ChangeKind;
use globset::{GlobBuilder, GlobSetBuilder};

use super::GateError;
use super::proc::git_bytes;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub kind: ChangeKind,
    /// Current path (the new path of a rename).
    pub path: String,
    /// Path at the base, for a rename.
    pub old_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChangeSet {
    /// Tracked changes followed by untracked files (as `Added`).
    pub changes: Vec<Change>,
    pub untracked: Vec<String>,
}

/// Lines a file gained relative to the base, with 1-based line numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddedLines {
    pub path: String,
    pub lines: Vec<(u32, String)>,
}

fn nul_fields(raw: &[u8]) -> Vec<String> {
    raw.split(|b| *b == 0)
        .filter(|field| !field.is_empty())
        .map(|field| String::from_utf8_lossy(field).into_owned())
        .collect()
}

fn parse_name_status(raw: &[u8]) -> Result<Vec<Change>, GateError> {
    let mut fields = nul_fields(raw).into_iter();
    let mut changes = Vec::new();
    let unparsable = || GateError::Git("diff --name-status: unparsable output".to_string());
    while let Some(status) = fields.next() {
        let path = fields.next().ok_or_else(unparsable)?;
        let change = match status.chars().next() {
            Some('A') => Change {
                kind: ChangeKind::Added,
                path,
                old_path: None,
            },
            // Type changes and unmerged paths are edits of an existing file.
            Some('M' | 'T' | 'U') => Change {
                kind: ChangeKind::Modified,
                path,
                old_path: None,
            },
            Some('D') => Change {
                kind: ChangeKind::Deleted,
                path,
                old_path: None,
            },
            Some('R') => Change {
                kind: ChangeKind::Renamed,
                path: fields.next().ok_or_else(unparsable)?,
                old_path: Some(path),
            },
            Some('C') => Change {
                kind: ChangeKind::Added,
                path: fields.next().ok_or_else(unparsable)?,
                old_path: None,
            },
            // Fail closed on a status this code does not know.
            _ => return Err(unparsable()),
        };
        changes.push(change);
    }
    Ok(changes)
}

/// `git diff --name-status -M <base>` (committed and uncommitted work) plus
/// untracked, non-ignored files.
pub fn collect(repo_root: &Path, base: &str, timeout: Duration) -> Result<ChangeSet, GateError> {
    let (diff_raw, untracked_raw) = std::thread::scope(|scope| {
        let untracked_handle = scope.spawn(|| {
            git_bytes(
                repo_root,
                &["ls-files", "--others", "--exclude-standard", "-z"],
                timeout,
            )
        });
        let diff_raw = git_bytes(
            repo_root,
            &[
                "diff",
                "--name-status",
                "-M",
                "-z",
                "--no-ext-diff",
                base,
                "--",
            ],
            timeout,
        );
        let untracked_raw = untracked_handle
            .join()
            .unwrap_or_else(|_| Err(GateError::Git("ls-files worker panicked".to_string())));
        (diff_raw, untracked_raw)
    });
    let mut changes = parse_name_status(&diff_raw?)?;
    let untracked = nul_fields(&untracked_raw?);
    changes.extend(untracked.iter().map(|path| Change {
        kind: ChangeKind::Added,
        path: path.clone(),
        old_path: None,
    }));
    Ok(ChangeSet { changes, untracked })
}

/// Undoes git's C-style quoting of unusual path names.
fn unquote(quoted: &str) -> String {
    let inner = quoted
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(quoted);
    let mut bytes = Vec::with_capacity(inner.len());
    let mut chars = inner.bytes().peekable();
    while let Some(byte) = chars.next() {
        if byte != b'\\' {
            bytes.push(byte);
            continue;
        }
        match chars.next() {
            Some(b'a') => bytes.push(0x07),
            Some(b'b') => bytes.push(0x08),
            Some(b'f') => bytes.push(0x0c),
            Some(b'v') => bytes.push(0x0b),
            Some(b'n') => bytes.push(b'\n'),
            Some(b't') => bytes.push(b'\t'),
            Some(b'r') => bytes.push(b'\r'),
            Some(digit @ b'0'..=b'7') => {
                let mut value = u32::from(digit - b'0');
                for _ in 0..2 {
                    match chars.peek() {
                        Some(next @ b'0'..=b'7') => {
                            value = value * 8 + u32::from(next - b'0');
                            chars.next();
                        }
                        _ => break,
                    }
                }
                bytes.push(u8::try_from(value).unwrap_or(b'?'));
            }
            Some(other) => bytes.push(other),
            None => {}
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn destination_path(header: &str) -> Option<String> {
    // Git appends a tab after a name that contains spaces.
    let header = header.strip_suffix('\t').unwrap_or(header);
    if header == "/dev/null" {
        return None;
    }
    let path = if header.starts_with('"') {
        unquote(header)
    } else {
        header.to_string()
    };
    path.strip_prefix("b/").map(str::to_string)
}

/// `@@ -a[,b] +c[,d] @@` to `(b, c, d)`.
fn hunk_header(line: &str) -> Option<(u32, u32, u32)> {
    let mut parts = line.split(' ');
    let (_, old, new) = (parts.next()?, parts.next()?, parts.next()?);
    let range = |text: &str, sign: char| -> Option<(u32, u32)> {
        let text = text.strip_prefix(sign)?;
        match text.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((text.parse().ok()?, 1)),
        }
    };
    let (_, old_count) = range(old, '-')?;
    let (new_start, new_count) = range(new, '+')?;
    Some((old_count, new_start, new_count))
}

/// Extracts added lines from a `-U0` unified diff. Hunk bodies are consumed
/// by their declared line counts, so content that looks like a diff header
/// cannot redirect findings to another file.
pub(crate) fn parse_added(diff: &str) -> Vec<AddedLines> {
    let mut out: Vec<AddedLines> = Vec::new();
    let mut current: Option<AddedLines> = None;
    let mut lines = diff.split('\n');
    while let Some(line) = lines.next() {
        if line.starts_with("diff --git ") {
            out.extend(current.take());
        } else if let Some(header) = line.strip_prefix("+++ ") {
            out.extend(current.take());
            current = destination_path(header).map(|path| AddedLines {
                path,
                lines: Vec::new(),
            });
        } else if line.starts_with("@@ ")
            && let Some((mut old_left, mut number, mut new_left)) = hunk_header(line)
        {
            while old_left + new_left > 0 {
                let Some(body) = lines.next() else { break };
                if let Some(text) = body.strip_prefix('+') {
                    if let Some(file) = current.as_mut() {
                        let text = text.strip_suffix('\r').unwrap_or(text);
                        file.lines.push((number, text.to_string()));
                    }
                    number = number.saturating_add(1);
                    new_left = new_left.saturating_sub(1);
                } else if body.starts_with('-') {
                    old_left = old_left.saturating_sub(1);
                } else if !body.starts_with('\\') {
                    // Not a hunk line: the header lied, stop trusting it.
                    break;
                }
            }
        }
    }
    out.extend(current);
    out.retain(|file| !file.lines.is_empty());
    out
}

fn untracked_lines(repo_root: &Path, path: &str) -> Result<Option<AddedLines>, GateError> {
    let full = repo_root.join(path);
    // Symlinks are not followed: their target may be outside the repo.
    if !fs::symlink_metadata(&full).is_ok_and(|meta| meta.is_file()) {
        return Ok(None);
    }
    let mut file = fs::File::open(&full)?;
    let mut bytes = Vec::new();
    // Same heuristic git uses to call a file binary: read at most 8000 bytes
    // first so large untracked binaries are rejected without buffering them.
    file.by_ref().take(8000).read_to_end(&mut bytes)?;
    if bytes.contains(&0) {
        return Ok(None);
    }
    file.read_to_end(&mut bytes)?;
    let lines = String::from_utf8_lossy(&bytes)
        .lines()
        .zip(1u32..)
        .map(|(text, number)| (number, text.to_string()))
        .collect();
    Ok(Some(AddedLines {
        path: path.to_string(),
        lines,
    }))
}

/// Whether the first 8000 bytes of the diffed (re-encoded) lines hold a NUL.
pub(crate) fn lines_head_has_nul(file: &AddedLines) -> bool {
    let mut seen = 0;
    for (_, text) in &file.lines {
        let Some(room) = 8000usize.checked_sub(seen).filter(|room| *room > 0) else {
            return false;
        };
        if text.bytes().take(room).any(|b| b == 0) {
            return true;
        }
        seen += text.len() + 1;
    }
    false
}

/// Paths per `git check-attr` call, well under any argument-length limit.
const ATTR_CHUNK: usize = 500;

/// `diff` and `working-tree-encoding` of each of `count` paths, in argv
/// order, from `check-attr -z` output (`path NUL attr NUL value NUL`, both
/// attributes per path in that order). Read by position, never by the echoed
/// path: macOS `core.precomposeunicode` echoes NFC for an NFD argument
/// (audit a13 R3-L2-1). `None` unless the output is exactly that.
fn parse_check_attr(raw: &[u8], count: usize) -> Option<Vec<[Option<String>; 2]>> {
    let mut fields: Vec<&[u8]> = raw.split(|b| *b == 0).collect();
    if fields.last().is_some_and(|last| last.is_empty()) {
        fields.pop();
    }
    let (triples, rest) = fields.as_chunks::<3>();
    if !rest.is_empty() || triples.len() != 2 * count {
        return None;
    }
    let text = |field: &[u8]| Some(String::from_utf8_lossy(field).into_owned());
    let (pairs, _) = triples.as_chunks::<2>();
    pairs
        .iter()
        .map(|[[_, diff, d], [_, enc, e]]| {
            (*diff == b"diff" && *enc == b"working-tree-encoding").then(|| [text(d), text(e)])
        })
        .collect()
}

/// How git diffs a path with a NUL, by its `diff` and
/// `working-tree-encoding` attributes: as text, as binary, or as the
/// `diff.<driver>.binary` config of a driver (`default` when unspecified)
/// decides. A missing attribute counts as text.
enum AttrVerdict {
    Text,
    Binary,
    Driver(String),
}

fn attr_verdict(attrs: Option<&[Option<String>; 2]>) -> AttrVerdict {
    let Some([Some(diff), Some(encoding)]) = attrs else {
        return AttrVerdict::Text;
    };
    match (encoding.as_str(), diff.as_str()) {
        ("unspecified" | "unset", "set") => AttrVerdict::Text,
        ("unspecified" | "unset", "unset") => AttrVerdict::Binary,
        ("unspecified" | "unset", "unspecified") => AttrVerdict::Driver("default".to_string()),
        ("unspecified" | "unset", driver) => AttrVerdict::Driver(driver.to_string()),
        _ => AttrVerdict::Text,
    }
}

/// Which of `paths` git was told to diff as text despite a NUL: a
/// `working-tree-encoding` (the disk bytes are not what git diffs), a set
/// `diff` attribute, or a `diff=<driver>` whose `diff.<driver>.binary` is
/// false. One `check_attr` per [`ATTR_CHUNK`] paths and one `is_false` per
/// distinct config key; a failed lookup counts as text.
fn diffed_as_text(
    paths: &[&str],
    mut check_attr: impl FnMut(&[&str]) -> Option<Vec<u8>>,
    mut is_false: impl FnMut(&str) -> bool,
) -> HashSet<String> {
    let mut asked: HashMap<String, bool> = HashMap::new();
    let mut text = HashSet::new();
    for chunk in paths.chunks(ATTR_CHUNK) {
        let attrs = check_attr(chunk).and_then(|raw| parse_check_attr(&raw, chunk.len()));
        for (i, path) in chunk.iter().enumerate() {
            let verdict = match &attrs {
                Some(attrs) => attr_verdict(attrs.get(i)),
                None => AttrVerdict::Text,
            };
            let as_text = match verdict {
                AttrVerdict::Text => true,
                AttrVerdict::Binary => false,
                AttrVerdict::Driver(driver) => binary_set_false(&driver, |key| {
                    *asked
                        .entry(key.to_string())
                        .or_insert_with(|| is_false(key))
                }),
            };
            if as_text {
                text.insert((*path).to_string());
            }
        }
    }
    text
}

/// Whether `diff.<driver>.binary` or `diff.default.binary` is false, as
/// answered by `is_false` for each key. Git uses `default` for a driver with
/// no config at all; asking it for any driver errs towards scanning.
fn binary_set_false(driver: &str, mut is_false: impl FnMut(&str) -> bool) -> bool {
    let fallback = (driver != "default").then_some("default");
    std::iter::once(driver).chain(fallback).any(|driver| {
        let key = format!("diff.{driver}.binary");
        is_false(&key)
    })
}

/// Whether `file` looks binary by the heuristic git itself uses: a NUL in
/// the first 8000 bytes of the diffed lines and of the file on disk.
/// Symlinks, missing paths and read errors count as text: their lines are
/// scanned rather than skipped.
fn has_nul_heads(repo_root: &Path, file: &AddedLines) -> bool {
    if !lines_head_has_nul(file) {
        return false;
    }
    let full = repo_root.join(&file.path);
    if !fs::symlink_metadata(&full).is_ok_and(|meta| meta.is_file()) {
        return false;
    }
    let mut head = Vec::with_capacity(8000);
    fs::File::open(&full)
        .and_then(|file| file.take(8000).read_to_end(&mut head))
        .is_ok_and(|_| head.contains(&0))
}

/// Drops the real binaries from `files`: NUL heads ([`has_nul_heads`]) and
/// no attribute telling git to diff them as text anyway ([`diffed_as_text`]).
fn drop_binaries(repo_root: &Path, files: &mut Vec<AddedLines>, timeout: Duration) {
    let candidates: Vec<&str> = files
        .iter()
        .filter(|file| has_nul_heads(repo_root, file))
        .map(|file| file.path.as_str())
        .collect();
    if candidates.is_empty() {
        return;
    }
    let text = diffed_as_text(
        &candidates,
        |chunk| {
            let mut args = vec!["check-attr", "-z", "diff", "working-tree-encoding", "--"];
            args.extend_from_slice(chunk);
            git_bytes(repo_root, &args, timeout).ok()
        },
        |key| {
            let args = ["config", "--type=bool", "--get", key];
            git_bytes(repo_root, &args, timeout).is_ok_and(|out| out.trim_ascii() == b"false")
        },
    );
    let binary: HashSet<String> = candidates
        .into_iter()
        .filter(|path| !text.contains(*path))
        .map(str::to_string)
        .collect();
    files.retain(|file| !binary.contains(&file.path));
}

/// Added lines of tracked changes relative to `base`, filtered by `skip` and
/// excluding real binaries. Independent of [`collect`], so the gate can run it
/// in parallel with `collect` and `store_compliance::scan`.
pub fn diff_added_tracked(
    repo_root: &Path,
    base: &str,
    skip: &dyn Fn(&str) -> bool,
    timeout: Duration,
) -> Result<Vec<AddedLines>, GateError> {
    let raw = git_bytes(
        repo_root,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "-U0",
            "-M",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            // A `-diff` or `binary` attribute (in `.gitattributes` or
            // `.git/info/attributes`) would otherwise hide every added line
            // behind "Binary files differ". Real binaries are dropped below.
            "--text",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            base,
            "--",
        ],
        timeout,
    )?;
    let mut files = parse_added(&String::from_utf8_lossy(&raw));
    files.retain(|file| !skip(&file.path));
    drop_binaries(repo_root, &mut files, timeout);
    Ok(files)
}

/// Appends whole-file added lines for untracked text files not filtered by `skip`.
pub(crate) fn append_untracked_lines(
    repo_root: &Path,
    files: &mut Vec<AddedLines>,
    untracked: &[String],
    skip: &dyn Fn(&str) -> bool,
) -> Result<(), GateError> {
    for path in untracked.iter().filter(|path| !skip(path)) {
        files.extend(untracked_lines(repo_root, path)?);
    }
    Ok(())
}

/// Added lines of tracked changes plus the whole content of untracked text
/// files. `skip` filters paths that are never scanned.
pub fn added_lines(
    repo_root: &Path,
    base: &str,
    untracked: &[String],
    skip: &dyn Fn(&str) -> bool,
    timeout: Duration,
) -> Result<Vec<AddedLines>, GateError> {
    let mut files = diff_added_tracked(repo_root, base, skip, timeout)?;
    append_untracked_lines(repo_root, &mut files, untracked, skip)?;
    Ok(files)
}

/// Extract directory prefixes from non-test locked globs so a symlink placed
/// at any parent directory of a locked config (e.g. `ln -s helper .cargo` or
/// `ln -s helper .github/workflows`) is locked even though git reports the
/// symlink entry itself rather than files beneath it.
pub(super) fn symlink_dir_prefixes(non_test_globs: &[String]) -> Vec<String> {
    let mut prefixes = Vec::new();
    for glob in non_test_globs {
        let parts: Vec<&str> = glob.split('/').collect();
        for end in 1..parts.len() {
            let prefix = &parts[..end];
            if prefix.iter().all(|seg| *seg == "**" || *seg == "*") {
                continue;
            }
            let joined = prefix.join("/");
            if !prefixes.contains(&joined) {
                prefixes.push(joined);
            }
        }
    }
    prefixes
}

/// Scans gitignored untracked files via `git ls-files --others --ignored
/// --exclude-standard -z` (fail-closed on git error) and appends `Added`
/// changes for any path not already in `existing` that matches `globs`
/// (case-insensitive) or is a symlink shadowing a directory prefix of `globs`.
pub(super) fn with_ignored_matching<'a>(
    repo_root: &Path,
    globs: &[String],
    existing: &'a [Change],
    timeout: Duration,
) -> Result<Cow<'a, [Change]>, GateError> {
    if globs.is_empty() {
        return Ok(Cow::Borrowed(existing));
    }
    let build_set = |patterns: &[String]| -> Result<globset::GlobSet, GateError> {
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            let glob = GlobBuilder::new(pattern)
                .literal_separator(true)
                .case_insensitive(true)
                .build()
                .map_err(|err| GateError::Config(format!("proof.capture_lock_globs: {err}")))?;
            builder.add(glob);
        }
        builder
            .build()
            .map_err(|err| GateError::Config(format!("proof.capture_lock_globs: {err}")))
    };
    let file_set = build_set(globs)?;
    let dir_set = build_set(&symlink_dir_prefixes(globs))?;
    let raw = git_bytes(
        repo_root,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
        ],
        timeout,
    )?;
    let seen: HashSet<&str> = existing.iter().map(|c| c.path.as_str()).collect();
    let mut matched = Vec::new();
    for path in nul_fields(&raw) {
        if seen.contains(path.as_str()) {
            continue;
        }
        let is_symlink_dir = dir_set.is_match(&path)
            && fs::symlink_metadata(repo_root.join(&path))
                .is_ok_and(|meta| meta.file_type().is_symlink());
        if file_set.is_match(&path) || is_symlink_dir {
            matched.push(Change {
                kind: ChangeKind::Added,
                path,
                old_path: None,
            });
        }
    }
    if matched.is_empty() {
        return Ok(Cow::Borrowed(existing));
    }
    let mut all = existing.to_vec();
    all.extend(matched);
    Ok(Cow::Owned(all))
}

#[cfg(test)]
mod binary_config_tests;
