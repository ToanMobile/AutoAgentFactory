//! The change set relative to the base: changed paths (including untracked
//! files) and the lines they add.

use std::fs;
use std::path::Path;
use std::time::Duration;

use aaf_domain::gate::ChangeKind;

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
    let raw = git_bytes(
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
    )?;
    let mut changes = parse_name_status(&raw)?;
    let raw = git_bytes(
        repo_root,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        timeout,
    )?;
    let untracked = nul_fields(&raw);
    changes.extend(untracked.iter().map(|path| Change {
        kind: ChangeKind::Added,
        path: path.clone(),
        old_path: None,
    }));
    Ok(ChangeSet { changes, untracked })
}

/// Undoes git's C-style quoting of unusual path names.
fn unquote(quoted: &str) -> String {
    let inner = quoted.trim_matches('"');
    let mut bytes = Vec::with_capacity(inner.len());
    let mut chars = inner.bytes().peekable();
    while let Some(byte) = chars.next() {
        if byte != b'\\' {
            bytes.push(byte);
            continue;
        }
        match chars.next() {
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
    let bytes = fs::read(&full)?;
    // Same heuristic git uses to call a file binary.
    if bytes.iter().take(8000).any(|b| *b == 0) {
        return Ok(None);
    }
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

/// Added lines of tracked changes plus the whole content of untracked text
/// files. `skip` filters paths that are never scanned.
pub fn added_lines(
    repo_root: &Path,
    base: &str,
    untracked: &[String],
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
            "--src-prefix=a/",
            "--dst-prefix=b/",
            base,
            "--",
        ],
        timeout,
    )?;
    let mut files = parse_added(&String::from_utf8_lossy(&raw));
    files.retain(|file| !skip(&file.path));
    for path in untracked.iter().filter(|path| !skip(path)) {
        files.extend(untracked_lines(repo_root, path)?);
    }
    Ok(files)
}
