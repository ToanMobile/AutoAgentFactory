#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The lookup of `git` and `gh` aaf does once per process: which `PATH`
//! entries count, and which files are never picked. Real git for the
//! repositories, stand-in executables for the programs.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use aaf_fp::{find_program, repository_dirs};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A canonical scratch root (macOS temp dirs sit behind a symlink).
fn scratch() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    (tmp, root)
}

fn repo_at(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    fs::write(dir.join("a.txt"), "a\n").unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

fn program(dir: &Path, mode: u32) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join("git");
    fs::write(&path, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    path
}

fn path_var(dirs: &[&Path]) -> OsString {
    std::env::join_paths(dirs).unwrap()
}

#[test]
fn a_git_inside_the_repository_is_passed_over_for_the_next_on_path() {
    let (_tmp, root) = scratch();
    let repo = root.join("repo");
    repo_at(&repo);
    program(&repo.join("bin"), 0o755);
    let outside = program(&root.join("outside"), 0o755);
    // Started in a subdirectory: the whole work tree is left out.
    let cwd = repo.join("bin");

    let found = find_program(
        &path_var(&[&repo.join("bin"), &root.join("outside")]),
        "git",
        &repository_dirs(&cwd),
    );

    assert_eq!(found, Some(outside));
}

#[test]
fn a_git_outside_the_repository_is_found_where_path_puts_it_first() {
    let (_tmp, root) = scratch();
    let repo = root.join("repo");
    repo_at(&repo);
    let first = program(&root.join("first"), 0o755);
    program(&root.join("second"), 0o755);

    let found = find_program(
        &path_var(&[&root.join("first"), &root.join("second")]),
        "git",
        &repository_dirs(&repo),
    );

    assert_eq!(found, Some(first));
}

#[test]
fn relative_and_empty_path_entries_are_not_searched() {
    let (_tmp, root) = scratch();
    // They depend on the current directory, wherever that is. This one
    // leads from the test's current directory to `root/rel`.
    program(&root.join("rel"), 0o755);
    let cwd = fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
    let mut relative = PathBuf::new();
    for _ in cwd.components().skip(1) {
        relative.push("..");
    }
    relative.push(root.join("rel").strip_prefix("/").unwrap());
    assert!(relative.join("git").is_file(), "{relative:?}");
    let absolute = program(&root.join("abs"), 0o755);

    let found = find_program(
        &path_var(&[&relative, Path::new(""), &root.join("abs")]),
        "git",
        &[],
    );

    assert_eq!(found, Some(absolute));
}

#[test]
fn a_link_from_outside_to_a_file_inside_the_repository_is_passed_over() {
    let (_tmp, root) = scratch();
    let repo = root.join("repo");
    repo_at(&repo);
    let inside = program(&repo.join("tools"), 0o755);
    fs::create_dir_all(root.join("links")).unwrap();
    std::os::unix::fs::symlink(&inside, root.join("links/git")).unwrap();
    // A directory link into the repository, too.
    std::os::unix::fs::symlink(repo.join("tools"), root.join("dirlink")).unwrap();

    let found = find_program(
        &path_var(&[&root.join("links"), &root.join("dirlink")]),
        "git",
        &repository_dirs(&repo),
    );

    assert_eq!(found, None);
}

#[test]
fn a_file_that_is_not_executable_is_passed_over() {
    let (_tmp, root) = scratch();
    program(&root.join("plain"), 0o644);
    let runnable = program(&root.join("runnable"), 0o755);

    let found = find_program(
        &path_var(&[&root.join("plain"), &root.join("runnable")]),
        "git",
        &[],
    );

    assert_eq!(found, Some(runnable));
}

#[test]
fn from_a_linked_worktree_the_main_work_tree_is_left_out_as_well() {
    let (_tmp, root) = scratch();
    let main = root.join("main");
    repo_at(&main);
    let linked = root.join("linked");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "task",
            linked.to_str().unwrap(),
        ],
    );
    program(&main.join("bin"), 0o755);

    let dirs = repository_dirs(&linked);

    assert_eq!(dirs, vec![linked.clone(), main.clone()]);
    assert_eq!(
        find_program(&path_var(&[&main.join("bin")]), "git", &dirs),
        None
    );
}

#[test]
fn outside_any_repository_nothing_is_left_out() {
    let (_tmp, root) = scratch();
    assert_eq!(repository_dirs(&root), Vec::<PathBuf>::new());
}
