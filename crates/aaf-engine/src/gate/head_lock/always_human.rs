//! Changes no reviewer pair may wave through: they edit what judges the PR
//! itself (gate config, CI, agent hooks, the commands the gate runs), or hide
//! their content from a path comparison (gitlink, nested repository, symlink).
//! `aaf board adopt` cancels such a PR with `needs_human_head_lock`.

use std::fs;
use std::path::Path;

use aaf_domain::config::RepoConfig;

use super::nested::Probe;
use crate::gate::changes::{Change, ChangeSet};
use crate::gate::proc::git_bytes;
use crate::gate::{GIT_TIMEOUT, GateError};

const SYMLINK: &str = "120000";
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
/// Directories whose whole subtree is always human-only (lowercase).
const HUMAN_DIRS: [&str; 3] = [".autofactory", ".github/workflows", ".github/actions"];
/// Files that register agent hooks (lowercase).
const HOOK_FILES: [&str; 5] = [
    ".claude/settings.json",
    ".claude/settings.local.json",
    ".codex/hooks.json",
    ".gemini/settings.json",
    ".opencode/hooks.json",
];

/// One path as the comparison sees it: lowercase, no `./`, no trailing `/`.
fn normalize(path: &str) -> String {
    let lower = path.to_ascii_lowercase();
    let lower = lower.trim_end_matches('/');
    lower.strip_prefix("./").unwrap_or(lower).to_string()
}

/// Why `path` (one name of a change) is always human-only, if it is.
fn path_reason(path: &str, commands: &[String]) -> Option<String> {
    let norm = normalize(path);
    if norm.is_empty() {
        return None;
    }
    for dir in HUMAN_DIRS {
        if norm == dir || norm.strip_prefix(dir).is_some_and(|r| r.starts_with('/')) {
            return Some(format!("it changes {dir}, which judges the PR itself"));
        }
    }
    let base = norm.rsplit('/').next().unwrap_or(&norm);
    if base == "action.yml" || base == "action.yaml" {
        return Some("it changes a GitHub action definition".to_string());
    }
    if HOOK_FILES.contains(&norm.as_str()) {
        return Some("it changes an agent hook registration file".to_string());
    }
    if commands.iter().any(|cmd| cmd.contains(&norm)) {
        return Some("a configured gate command names it".to_string());
    }
    // An untracked directory git lists with a trailing `/` is a repository of its own.
    path.ends_with('/')
        .then(|| "a nested repository is added or changed".to_string())
}

/// The first reason the changes can only be judged by a human, or `None`.
///
/// Pure and path-based: every name of every change (`path` and `old_path`) is
/// compared without regard to case, so a rename out of a protected path counts
/// as much as an edit in it. What a path cannot show (gitlink, symlink) is
/// [`always_human_link_reason`].
pub fn always_human_reason(changes: &ChangeSet, cfg: &RepoConfig) -> Option<String> {
    let commands: Vec<String> = [
        &cfg.commands.build,
        &cfg.commands.test_scoped,
        &cfg.commands.test_full,
        &cfg.commands.lint,
        &cfg.commands.lint_full,
    ]
    .into_iter()
    .filter(|c| !c.is_empty())
    .map(|c| c.to_ascii_lowercase())
    .collect();
    changes.changes.iter().find_map(|change| {
        std::iter::once(change.path.as_str())
            .chain(change.old_path.as_deref())
            .find_map(|path| path_reason(path, &commands).map(|why| format!("{path}: {why}")))
    })
}

/// The names of `changes` as repository paths (`path`, `old_path`), deduplicated.
fn names(changes: &[Change]) -> Vec<&str> {
    let mut all: Vec<&str> = changes
        .iter()
        .flat_map(|c| std::iter::once(c.path.as_str()).chain(c.old_path.as_deref()))
        .map(|p| p.trim_end_matches('/'))
        .filter(|p| !p.is_empty())
        .collect();
    all.sort_unstable();
    all.dedup();
    all
}

/// Paths of `raw` (`ls-files -s -z` or `ls-tree -z` output) stored with mode 120000.
fn symlink_paths(raw: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(raw)
        .split('\0')
        .filter_map(|line| {
            let (meta, path) = line.split_once('\t')?;
            (meta.split(' ').next() == Some(SYMLINK)).then(|| path.to_string())
        })
        .collect()
}

/// The first reason the changes hide their content from a path comparison: a
/// gitlink or submodule (`base`, HEAD or index), a symlink (mode 120000 at
/// `base`, HEAD or index, or on disk) or a nested repository. Fails closed:
/// a git error is an error, never "nothing found".
pub fn always_human_link_reason(
    repo_root: &Path,
    base: &str,
    changes: &ChangeSet,
) -> Result<Option<String>, GateError> {
    let mut probe = Probe::new(repo_root, base, &changes.changes);
    for change in &changes.changes {
        if change.path.ends_with('/') {
            return Ok(Some(format!(
                "{}: a nested repository is added or changed",
                change.path
            )));
        }
        if probe.link(change)?.is_some() {
            return Ok(Some(format!(
                "{}: a gitlink or submodule is added, moved or changed",
                change.path
            )));
        }
    }
    let all = names(&changes.changes);
    for path in &all {
        if let Ok(meta) = fs::symlink_metadata(repo_root.join(path))
            && meta.is_symlink()
        {
            return Ok(Some(format!("{path}: a symlink is added or changed")));
        }
    }
    let head = aaf_fp::git_or_none(
        repo_root,
        &["rev-parse", "-q", "--verify", "HEAD^{commit}"],
        GIT_TIMEOUT,
    )?;
    let mut trees: Vec<&str> = Vec::new();
    if base != EMPTY_TREE {
        trees.push(base);
    }
    if let Some(head) = head.as_deref() {
        trees.push(head);
    }
    for chunk in all.chunks(500) {
        let mut found = Vec::new();
        for tree in &trees {
            let args = [
                &["--literal-pathspecs", "ls-tree", "-z", tree, "--"][..],
                chunk,
            ]
            .concat();
            found.extend(symlink_paths(&git_bytes(repo_root, &args, GIT_TIMEOUT)?));
        }
        let args = [
            &["--literal-pathspecs", "ls-files", "-s", "-z", "--"][..],
            chunk,
        ]
        .concat();
        found.extend(symlink_paths(&git_bytes(repo_root, &args, GIT_TIMEOUT)?));
        if let Some(path) = found.into_iter().next() {
            return Ok(Some(format!("{path}: a symlink is added or changed")));
        }
    }
    Ok(None)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use aaf_domain::gate::ChangeKind;

    use super::*;

    fn change(kind: ChangeKind, path: &str, old: Option<&str>) -> Change {
        Change {
            kind,
            path: path.into(),
            old_path: old.map(Into::into),
        }
    }

    fn set(changes: Vec<Change>) -> ChangeSet {
        ChangeSet {
            changes,
            untracked: Vec::new(),
        }
    }

    fn reason(path: &str) -> Option<String> {
        let mut cfg = RepoConfig::default();
        cfg.commands.test_scoped = "bash scripts/run-tests.sh".to_string();
        always_human_reason(&set(vec![change(ChangeKind::Modified, path, None)]), &cfg)
    }

    #[test]
    fn every_listed_kind_matches_once() {
        for path in [
            ".autofactory/config.toml",
            ".github/workflows/ci.yml",
            ".github/actions/setup/action.yml",
            "tools/deploy/action.yml",
            "action.yaml",
            ".claude/settings.json",
            ".claude/settings.local.json",
            ".codex/hooks.json",
            ".gemini/settings.json",
            ".opencode/hooks.json",
            "scripts/run-tests.sh",
            "vendor/nested/",
        ] {
            assert!(reason(path).is_some(), "{path} must need a human");
        }
    }

    #[test]
    fn a_rename_is_judged_by_its_old_path_too() {
        let cfg = RepoConfig::default();
        let renamed = set(vec![change(
            ChangeKind::Renamed,
            "docs/ci.yml",
            Some(".github/workflows/ci.yml"),
        )]);
        let why = always_human_reason(&renamed, &cfg).unwrap();
        assert!(why.starts_with(".github/workflows/ci.yml:"), "{why}");
    }

    #[test]
    fn case_does_not_hide_a_protected_path() {
        assert!(reason(".GitHub/Workflows/x.yml").is_some());
        assert!(reason(".AutoFactory/Config.toml").is_some());
        assert!(reason("Tools/ACTION.YML").is_some());
        assert!(reason(".Claude/Settings.JSON").is_some());
        assert!(reason("Scripts/Run-Tests.sh").is_some());
    }

    #[test]
    fn the_directory_name_itself_counts() {
        assert!(reason(".autofactory").is_some());
        assert!(reason(".github/workflows").is_some());
        assert!(reason(".AUTOFACTORY/").is_some());
    }

    #[test]
    fn near_misses_and_plain_files_do_not_match() {
        for path in [
            "src/lib.rs",
            ".autofactory-notes/readme.md",
            ".github/CODEOWNERS",
            ".github/workflows-old/x.yml",
            "docs/action.yml.md",
            ".claude/commands/x.md",
        ] {
            assert!(reason(path).is_none(), "{path} is an ordinary file");
        }
        let mut cfg = RepoConfig::default();
        cfg.commands.build = String::new();
        let empty = set(vec![change(
            ChangeKind::Added,
            "scripts/run-tests.sh",
            None,
        )]);
        assert!(always_human_reason(&empty, &cfg).is_none());
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(["-c", "protocol.file.allow=always"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-q", "-m", "base"]);
        dir
    }

    fn head(dir: &Path) -> String {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn a_plain_file_has_no_link_reason() {
        let dir = repo();
        let base = head(dir.path());
        fs::write(dir.path().join("b.txt"), "b\n").unwrap();
        git(dir.path(), &["add", "b.txt"]);
        let changes = set(vec![change(ChangeKind::Added, "b.txt", None)]);
        assert_eq!(
            always_human_link_reason(dir.path(), &base, &changes).unwrap(),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_in_the_index_or_on_disk_or_at_base_is_found() {
        let dir = repo();
        let base = head(dir.path());
        std::os::unix::fs::symlink("a.txt", dir.path().join("ln")).unwrap();
        let changes = set(vec![change(ChangeKind::Added, "ln", None)]);
        // On disk only.
        let why = always_human_link_reason(dir.path(), &base, &changes).unwrap();
        assert!(why.unwrap().starts_with("ln:"));
        // In the index and HEAD, after the link is gone from disk.
        git(dir.path(), &["add", "ln"]);
        git(dir.path(), &["commit", "-q", "-m", "link"]);
        let linked = head(dir.path());
        fs::remove_file(dir.path().join("ln")).unwrap();
        let deleted = set(vec![change(ChangeKind::Deleted, "ln", None)]);
        assert!(
            always_human_link_reason(dir.path(), &linked, &deleted)
                .unwrap()
                .is_some(),
            "a symlink at base"
        );
        assert!(
            always_human_link_reason(dir.path(), &base, &deleted)
                .unwrap()
                .is_some(),
            "a symlink at HEAD"
        );
    }

    #[test]
    fn a_gitlink_is_found() {
        let dir = repo();
        let base = head(dir.path());
        let sub = repo();
        git(
            dir.path(),
            &[
                "submodule",
                "add",
                "-q",
                sub.path().to_str().unwrap(),
                "vendor/sub",
            ],
        );
        let changes = set(vec![change(ChangeKind::Added, "vendor/sub", None)]);
        let why = always_human_link_reason(dir.path(), &base, &changes).unwrap();
        assert!(why.unwrap().contains("gitlink"));
    }

    #[test]
    fn an_untracked_nested_repository_is_found_by_its_trailing_slash() {
        let dir = repo();
        let base = head(dir.path());
        let changes = set(vec![change(ChangeKind::Added, "inner/", None)]);
        let why = always_human_link_reason(dir.path(), &base, &changes).unwrap();
        assert!(why.unwrap().contains("nested"));
    }
}
