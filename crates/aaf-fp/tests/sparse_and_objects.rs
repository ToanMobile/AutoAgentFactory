#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Wave 3, seen on real repositories: `tree_fp` failed in a sparse checkout
//! ("git add: exit Some(1)") and left one loose object in the repository for
//! every untracked file it hashed. Real git, temp repositories.

use std::fs;
use std::path::Path;
use std::process::Command;

use aaf_fp::GIT_TIMEOUT;

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(["-c", "gc.auto=0", "-c", "maintenance.auto=false"])
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
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn fp(repo: &Path) -> String {
    aaf_fp::tree_fp(repo, GIT_TIMEOUT).unwrap()
}

/// A repository with `kept/a.txt`, `dropped/b.txt` and `root.txt` committed.
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    for (name, body) in [
        ("kept/a.txt", "a\n"),
        ("dropped/b.txt", "b\n"),
        ("root.txt", "r\n"),
    ] {
        let path = dir.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    dir
}

fn object_files(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in fs::read_dir(&next).unwrap().filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                found.push(path.to_string_lossy().into_owned());
            }
        }
    }
    found.sort();
    found
}

/// The tree of the three committed files: fixed content, so the hash is a
/// constant. Receipts signed before this change carry values computed the
/// old way; the constant was recorded with the old code.
const CLEAN_TREE: &str = "git-tree:325d3f4643213a66f8f141f9fa835340b8d597f7";

#[test]
fn a_normal_checkout_keeps_the_value_it_always_had() {
    let repo = repo();
    assert_eq!(fp(repo.path()), CLEAN_TREE);
    assert_eq!(
        fp(repo.path()),
        format!(
            "git-tree:{}",
            git(repo.path(), &["rev-parse", "HEAD^{tree}"])
        )
    );

    // Dirty: the value is what staging everything in the real index gives.
    fs::write(repo.path().join("kept/new.txt"), "n\n").unwrap();
    fs::write(repo.path().join("root.txt"), "changed\n").unwrap();
    let dirty = fp(repo.path());
    git(repo.path(), &["add", "-A"]);
    assert_eq!(
        dirty,
        format!("git-tree:{}", git(repo.path(), &["write-tree"]))
    );
}

#[test]
fn a_sparse_checkout_is_fingerprinted() {
    let repo = repo();
    git(repo.path(), &["sparse-checkout", "set", "kept"]);
    assert!(!repo.path().join("dropped").exists());

    // What the sparse checkout leaves out is still part of the tree.
    assert_eq!(fp(repo.path()), CLEAN_TREE);

    fs::write(repo.path().join("kept/new.txt"), "n\n").unwrap();
    let inside = fp(repo.path());
    assert_ne!(inside, CLEAN_TREE);

    // A file outside the sparse definition is on disk all the same, so a
    // build sees it: it counts. This is where `git add -A` used to stop.
    fs::create_dir_all(repo.path().join("dropped")).unwrap();
    fs::write(repo.path().join("dropped/extra.txt"), "x\n").unwrap();
    let outside = fp(repo.path());
    assert_ne!(outside, inside);
}

#[test]
fn fingerprinting_writes_no_object_into_the_repository() {
    let repo = repo();
    fs::write(repo.path().join("untracked.bin"), "never committed\n").unwrap();
    fs::write(repo.path().join("root.txt"), "edited\n").unwrap();
    let objects = repo.path().join(".git/objects");
    let before = object_files(&objects);

    let first = fp(repo.path());

    assert_eq!(object_files(&objects), before);
    assert_eq!(fp(repo.path()), first);
    let leftovers: Vec<_> = fs::read_dir(repo.path().join(".git"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("aaf-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn a_linked_worktree_is_fingerprinted_without_writing_objects() {
    let repo = repo();
    let other = tempfile::tempdir().unwrap();
    let tree = other.path().join("wt");
    git(
        repo.path(),
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "side",
            tree.to_str().unwrap(),
        ],
    );
    fs::write(tree.join("untracked.txt"), "u\n").unwrap();
    let objects = repo.path().join(".git/objects");
    let before = object_files(&objects);

    assert_ne!(fp(&tree), CLEAN_TREE);

    assert_eq!(object_files(&objects), before);
}

#[cfg(unix)]
#[test]
fn the_key_is_created_in_an_owner_only_data_directory() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("share/autofactory");

    aaf_fp::load_or_create_key(&data).unwrap();

    let mode = fs::metadata(&data).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700);
}
