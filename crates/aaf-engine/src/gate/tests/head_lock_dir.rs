//! Head lock on gitlinks and nested repositories (audit GATE-078 F2, F3).

use std::fs;

use aaf_domain::config::RepoConfig;
use aaf_domain::gate::{ChangeKind, HeadLockReport, LockStatus, LockTier, LockedChange};

use super::super::head_lock::evaluate;
use super::super::{GIT_TIMEOUT, changes};
use super::support::Repo;

fn report(repo: &Repo) -> HeadLockReport {
    evaluate(repo.data_dir(), repo.root(), &RepoConfig::default(), "main")
}

/// A nested git repository at `dir` holding `files`, committed inside it.
fn nested(repo: &Repo, dir: &str, files: &[(&str, &str)]) {
    for (name, text) in files {
        repo.write(&format!("{dir}/{name}"), text);
    }
    repo.git(&["-C", dir, "init", "-q", "-b", "main"]);
    repo.git(&["-C", dir, "add", "-A"]);
    repo.git(&["-C", dir, "commit", "-q", "-m", "nested"]);
}

/// Commits `dir` as a gitlink, then a `.gitmodules` naming it.
fn gitlink(repo: &Repo, dir: &str, extra: &str) {
    repo.git(&["add", dir]);
    repo.commit("gitlink");
    let tree = repo.git(&["ls-tree", "HEAD", dir]);
    assert!(tree.starts_with("160000 commit "), "{tree}");
    let text = format!("[submodule \"{dir}\"]\n\tpath = {dir}\n\turl = ./{dir}\n{extra}");
    repo.write(".gitmodules", &text);
    repo.commit("gitmodules");
}

/// Blocked with a real fingerprint and exactly one Hard entry `path`/`change`.
fn assert_only_hard(report: &HeadLockReport, path: &str, change: ChangeKind) {
    assert_eq!(report.status, LockStatus::Blocked, "{report:?}");
    assert_eq!(report.lock_fp.len(), 64);
    let tier = LockTier::Hard;
    let path = path.to_string();
    assert_eq!(report.locked_changes.len(), 1, "{report:?}");
    assert_eq!(
        report.locked_changes,
        vec![LockedChange { path, change, tier }]
    );
}

const CARGO_CONFIG: (&str, &str) = ("config.toml", "[build]\njobs = 1\n");

#[test]
fn gitlink_hidden_by_gitmodules_ignore_all_is_still_locked() {
    let repo = Repo::new();
    nested(&repo, ".cargo", &[CARGO_CONFIG]);
    gitlink(&repo, ".cargo", "\tignore = all\n");
    let set = changes::collect(repo.root(), "main", GIT_TIMEOUT).unwrap();
    let mut added = set.changes.iter().filter(|c| c.kind == ChangeKind::Added);
    assert!(added.any(|c| c.path == ".cargo"), "{:?}", set.changes);
    assert_only_hard(&report(&repo), ".cargo", ChangeKind::Added);
}

#[test]
fn nested_repo_replacing_an_existing_test_file_is_hard_locked() {
    let repo = Repo::new();
    fs::remove_file(repo.path("tests/a_test.rs")).unwrap();
    let files = [("checks.rs", "fn checks() {}\n")];
    nested(&repo, "tests/a_test.rs", &files);
    // Git reports a typechange (Modified); a path-only tier would say Soft,
    // the `dir:` hash forces Hard.
    assert_only_hard(&report(&repo), "tests/a_test.rs", ChangeKind::Modified);
}

#[test]
fn submodule_gitdir_location_does_not_change_lock_fp() {
    let repo = Repo::new();
    nested(&repo, ".cargo", &[CARGO_CONFIG]);
    let out = tempfile::TempDir::new().unwrap();
    let (a, b) = (out.path().join("a"), out.path().join("b"));
    let point = |to: &std::path::Path| {
        let text = format!("gitdir: {}\n", to.display());
        fs::write(repo.path(".cargo/.git"), text).unwrap();
    };
    fs::rename(repo.path(".cargo/.git"), &a).unwrap();
    point(&a);
    gitlink(&repo, ".cargo", "");
    let first = report(&repo);
    assert_only_hard(&first, ".cargo", ChangeKind::Added);

    fs::rename(&a, &b).unwrap();
    point(&b);
    let git_dir = repo.git(&["-C", ".cargo", "rev-parse", "--absolute-git-dir"]);
    assert!(git_dir.ends_with("/b"), "{git_dir}");
    let second = report(&repo);
    assert_only_hard(&second, ".cargo", ChangeKind::Added);
    assert_eq!(second.lock_fp, first.lock_fp);
}

#[cfg(unix)]
#[test]
fn locked_dir_stat_error_other_than_not_found_fails_closed() {
    let repo = Repo::new();
    repo.write("blocker", "a regular file\n");
    let mut builder = globset::GlobSetBuilder::new();
    builder.add(globset::Glob::new("{blocker,missing}/x").unwrap());
    let set = builder.build().unwrap();
    // `blocker/x` stats as ENOTDIR (not NotFound): treated as locked.
    assert!(changes::matches_locked_dir(&set, repo.root(), "blocker/x"));
    assert!(!changes::matches_locked_dir(&set, repo.root(), "missing/x"));
}
