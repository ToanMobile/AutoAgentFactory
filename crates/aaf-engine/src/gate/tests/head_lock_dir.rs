//! Head lock on gitlinks and nested repositories (audit GATE-078 F2, F3).

use std::fs;

use aaf_domain::config::RepoConfig;
use aaf_domain::gate::{ChangeKind, HeadLockReport, LockStatus, LockTier, LockedChange};

use super::super::head_lock::{approve, content_hash, evaluate};
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

const SUB_CARGO: (&str, &str) = ("Cargo.toml", "[package]\nname = \"sub\"\n");

/// `main` with a gitlink `vendor/sub` (a `Cargo.toml` and `src/lib.rs`
/// inside), `feature` reset to it.
fn base_with_gitlink() -> Repo {
    let repo = Repo::new();
    repo.git(&["checkout", "-q", "main"]);
    nested(
        &repo,
        "vendor/sub",
        &[SUB_CARGO, ("src/lib.rs", "fn a() {}\n")],
    );
    repo.git(&["add", "vendor/sub"]);
    repo.commit("gitlink");
    repo.git(&["checkout", "-q", "-B", "feature"]);
    repo
}

/// Commits `files` inside `vendor/sub`, then the bumped gitlink.
fn bump(repo: &Repo, files: &[(&str, &str)]) {
    for (name, text) in files {
        repo.write(&format!("vendor/sub/{name}"), text);
    }
    repo.git(&["-C", "vendor/sub", "add", "-A"]);
    repo.git(&["-C", "vendor/sub", "commit", "-q", "-m", "bump"]);
    repo.git(&["add", "vendor/sub"]);
    repo.commit("bump");
}

#[test]
fn gitlink_bump_lock_is_approvable_and_bound_to_its_sha() {
    let repo = base_with_gitlink();
    bump(&repo, &[("src/lib.rs", "fn b() {}\n")]);
    assert_eq!(report(&repo).status, LockStatus::Clear);
    bump(&repo, &[("Cargo.toml", "[package]\nname = \"sub2\"\n")]);
    assert_only_hard(&report(&repo), "vendor/sub", ChangeKind::Modified);
    let clock = super::support::FixedClock(42);
    let cfg = RepoConfig::default();
    approve(repo.data_dir(), repo.root(), &cfg, "r", false, &clock).unwrap();
    assert_eq!(report(&repo).status, LockStatus::Approved);
    bump(&repo, &[("src/lib.rs", "fn c() {}\n")]);
    assert_only_hard(&report(&repo), "vendor/sub", ChangeKind::Modified);
}

#[test]
fn deleted_gitlink_is_hard_locked() {
    let repo = base_with_gitlink();
    repo.git(&["rm", "-q", "--cached", "vendor/sub"]);
    fs::remove_dir_all(repo.path("vendor/sub")).unwrap();
    repo.commit("drop gitlink");
    assert_only_hard(&report(&repo), "vendor/sub", ChangeKind::Deleted);
}

#[test]
fn added_gitlink_under_tests_holding_a_locked_file_is_hard() {
    let repo = Repo::new();
    nested(&repo, "tests/fx", &[("conftest.py", "x = 1\n")]);
    assert_only_hard(&report(&repo), "tests/fx/", ChangeKind::Added);
    repo.git(&["add", "tests/fx"]);
    repo.commit("gitlink");
    assert_only_hard(&report(&repo), "tests/fx", ChangeKind::Added);
}

#[test]
fn committed_gitlink_with_its_files_removed_on_disk_stays_locked() {
    let repo = Repo::new();
    nested(&repo, "crates/evil", &[SUB_CARGO]);
    repo.git(&["add", "crates/evil"]);
    repo.commit("gitlink");
    fs::remove_file(repo.path("crates/evil/Cargo.toml")).unwrap();
    assert_only_hard(&report(&repo), "crates/evil", ChangeKind::Added);
}

#[test]
fn content_hash_counts_only_a_missing_path_as_deleted() {
    let repo = Repo::new();
    // ENOTDIR: a parent turned into a file.
    assert_eq!(content_hash(repo.root(), "README.md/x").unwrap(), "deleted");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        repo.write("shut/x", "x\n");
        let perm = |mode| fs::Permissions::from_mode(mode);
        fs::set_permissions(repo.path("shut"), perm(0o000)).unwrap();
        let denied = content_hash(repo.root(), "shut/x");
        fs::set_permissions(repo.path("shut"), perm(0o755)).unwrap();
        assert!(denied.is_err(), "{denied:?}");
    }
}

#[test]
fn gitlink_dir_removed_on_disk_is_locked_only_when_bumped() {
    let unchanged = base_with_gitlink();
    fs::remove_dir_all(unchanged.path("vendor/sub")).unwrap();
    assert_eq!(report(&unchanged).status, LockStatus::Clear);
    let repo = base_with_gitlink();
    bump(&repo, &[("Cargo.toml", "[package]\nname = \"sub2\"\n")]);
    fs::remove_dir_all(repo.path("vendor/sub")).unwrap();
    assert_only_hard(&report(&repo), "vendor/sub", ChangeKind::Deleted);
}

#[test]
fn uninitialized_gitlink_lock_is_approvable_and_bound_to_its_sha() {
    let repo = base_with_gitlink();
    bump(&repo, &[("src/lib.rs", "fn b() {}\n")]);
    let out = tempfile::TempDir::new().unwrap();
    let fresh = out.path().join("fresh");
    let fresh_arg = fresh.to_str().unwrap();
    repo.git(&["worktree", "add", "-q", "--detach", fresh_arg, "HEAD"]);
    // A checkout (as the merge verification makes) leaves the gitlink an
    // empty directory: nothing shows what the bump changed.
    let mut entries = fs::read_dir(fresh.join("vendor/sub")).unwrap();
    assert!(entries.next().is_none());
    let cfg = RepoConfig::default();
    let fresh_report = || evaluate(repo.data_dir(), &fresh, &cfg, "main");
    assert_only_hard(&fresh_report(), "vendor/sub", ChangeKind::Modified);
    let clock = super::support::FixedClock(42);
    approve(repo.data_dir(), &fresh, &cfg, "r", false, &clock).unwrap();
    assert_eq!(fresh_report().status, LockStatus::Approved);
    let other = repo.git(&["rev-parse", "main"]);
    let info = format!("160000,{},vendor/sub", other.trim());
    repo.git(&["-C", fresh_arg, "update-index", "--cacheinfo", &info]);
    repo.git(&["-C", fresh_arg, "commit", "-q", "-m", "bump again"]);
    assert_only_hard(&fresh_report(), "vendor/sub", ChangeKind::Modified);
}

// Rework 1 of GATE-089 (counter-review 47).

#[test]
fn gitlink_typechanged_to_a_file_is_hard_locked() {
    let repo = base_with_gitlink();
    fs::remove_dir_all(repo.path("vendor/sub")).unwrap();
    repo.write("vendor/sub", "x");
    repo.commit("typechange");
    assert_only_hard(&report(&repo), "vendor/sub", ChangeKind::Modified);
}

#[test]
fn committed_gitlink_replaced_by_a_file_on_disk_stays_locked() {
    let repo = Repo::new();
    nested(&repo, "crates/evil", &[SUB_CARGO]);
    repo.git(&["add", "crates/evil"]);
    repo.commit("gitlink");
    fs::remove_dir_all(repo.path("crates/evil")).unwrap();
    repo.write("crates/evil", "");
    assert_only_hard(&report(&repo), "crates/evil", ChangeKind::Added);
}

#[cfg(unix)]
#[test]
fn locked_dir_symlink_in_a_gitlink_commit_is_locked_without_the_disk() {
    let repo = base_with_gitlink();
    std::os::unix::fs::symlink("mycargo", repo.path("vendor/sub/.cargo")).unwrap();
    bump(&repo, &[("mycargo/config.toml", "[build]\njobs = 1\n")]);
    fs::remove_file(repo.path("vendor/sub/.cargo")).unwrap();
    assert_only_hard(&report(&repo), "vendor/sub", ChangeKind::Modified);
}

#[cfg(unix)]
#[test]
fn symlink_to_a_directory_in_an_untracked_nested_repo_is_locked() {
    let repo = Repo::new();
    let out = tempfile::TempDir::new().unwrap();
    fs::write(out.path().join("Cargo.toml"), SUB_CARGO.1).unwrap();
    nested(&repo, "crates/evil", &[("readme.txt", "x\n")]);
    std::os::unix::fs::symlink(out.path(), repo.path("crates/evil/link")).unwrap();
    assert_only_hard(&report(&repo), "crates/evil/", ChangeKind::Added);
}

#[test]
fn staged_only_locked_edit_in_a_submodule_index_is_locked() {
    let repo = base_with_gitlink();
    bump(&repo, &[("src/lib.rs", "fn b() {}\n")]);
    assert_eq!(report(&repo).status, LockStatus::Clear);
    repo.write("vendor/sub/Cargo.toml", "[package]\nname = \"evil\"\n");
    repo.git(&["-C", "vendor/sub", "add", "Cargo.toml"]);
    repo.write("vendor/sub/Cargo.toml", SUB_CARGO.1);
    assert_only_hard(&report(&repo), "vendor/sub", ChangeKind::Modified);
}

#[test]
fn skip_worktree_or_assume_unchanged_locked_file_in_a_submodule_is_locked() {
    for flag in ["--skip-worktree", "--assume-unchanged"] {
        let repo = base_with_gitlink();
        bump(&repo, &[("src/lib.rs", "fn b() {}\n")]);
        repo.git(&["-C", "vendor/sub", "update-index", flag, "Cargo.toml"]);
        repo.write("vendor/sub/Cargo.toml", "[package]\nname = \"evil\"\n");
        assert_only_hard(&report(&repo), "vendor/sub", ChangeKind::Modified);
    }
}

#[test]
fn unborn_head_with_a_clean_nested_repo_is_clear() {
    let repo = Repo::empty();
    nested(&repo, "docs/notes", &[("readme.txt", "notes\n")]);
    let empty_tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let cfg = RepoConfig::default();
    let report = evaluate(repo.data_dir(), repo.root(), &cfg, empty_tree);
    assert_eq!(report.status, LockStatus::Clear, "{report:?}");
}

#[test]
fn uninitialized_gitlink_at_a_locked_path_approval_is_bound_to_its_sha() {
    let repo = Repo::new();
    fs::create_dir(repo.path(".cargo")).unwrap();
    let link = |sha: &str| {
        let info = format!("160000,{sha},.cargo");
        repo.git(&["update-index", "--add", "--cacheinfo", &info]);
        repo.git(&["commit", "-q", "-m", "gitlink"]);
    };
    link(&repo.git(&["rev-parse", "main"]));
    assert_only_hard(&report(&repo), ".cargo", ChangeKind::Added);
    let clock = super::support::FixedClock(42);
    let cfg = RepoConfig::default();
    approve(repo.data_dir(), repo.root(), &cfg, "r", false, &clock).unwrap();
    assert_eq!(report(&repo).status, LockStatus::Approved);
    link(&repo.git(&["rev-parse", "HEAD"]));
    assert_only_hard(&report(&repo), ".cargo", ChangeKind::Added);
}
