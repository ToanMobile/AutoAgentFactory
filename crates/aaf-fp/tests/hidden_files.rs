#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Security review of 43bbd10..e4645ed: files that are on disk, and that a
//! command run in the work tree reads, without being in the fingerprint and
//! without `git status` showing anything. Real git, temp repositories.

use std::fs;
use std::path::Path;
use std::process::Command;

use aaf_fp::{GIT_TIMEOUT, HiddenFiles, IgnoredFile};

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
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
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn write(repo: &Path, name: &str, body: &str) {
    let path = repo.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

/// `test_x.py`, `src/app.py` and a `.gitignore` for build output, committed.
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    write(
        dir.path(),
        "test_x.py",
        "def test_x():\n    assert 1 == 2\n",
    );
    write(dir.path(), "src/app.py", "x = 1\n");
    write(dir.path(), ".gitignore", "build/\n*.o\n");
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    dir
}

fn exclude(repo: &Path, pattern: &str) {
    let path = repo.join(".git/info/exclude");
    let mut text = fs::read_to_string(&path).unwrap_or_default();
    text.push_str(pattern);
    text.push('\n');
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn hidden(repo: &Path) -> HiddenFiles {
    aaf_fp::hidden_files(repo, GIT_TIMEOUT).unwrap()
}

fn ignored(path: &str, source: &str) -> IgnoredFile {
    IgnoredFile {
        path: path.to_string(),
        source: source.to_string(),
    }
}

/// The reviewer's four commands. The fingerprint and `git status` do not
/// move, which is exactly why something else has to notice.
#[test]
fn excluded_conftest_and_skip_worktree_test_are_found() {
    let repo = repo();
    let root = repo.path();
    let before = aaf_fp::tree_fp(root, GIT_TIMEOUT).unwrap();
    assert!(hidden(root).is_empty());

    exclude(root, "conftest.py");
    write(root, "conftest.py", "collect_ignore = ['test_x.py']\n");
    git(root, &["update-index", "--skip-worktree", "test_x.py"]);
    write(root, "test_x.py", "def test_x():\n    assert True\n");

    assert_eq!(aaf_fp::tree_fp(root, GIT_TIMEOUT).unwrap(), before);
    assert_eq!(git(root, &["status", "--porcelain"]), "");
    assert_eq!(
        git(root, &["ls-files", "--others", "--exclude-standard"]),
        ""
    );
    let found = hidden(root);
    assert_eq!(found.flagged, vec!["test_x.py"]);
    assert_eq!(
        found.ignored,
        vec![ignored("conftest.py", ".git/info/exclude")]
    );
    let reasons = found.reasons();
    assert_eq!(reasons.len(), 2);
    assert!(reasons[0].contains("test_x.py") && reasons[0].contains("skip-worktree"));
    assert!(reasons[1].contains("conftest.py") && reasons[1].contains(".git/info/exclude"));
}

#[test]
fn assume_unchanged_file_on_disk_is_found() {
    let repo = repo();
    git(
        repo.path(),
        &["update-index", "--assume-unchanged", "src/app.py"],
    );
    assert_eq!(hidden(repo.path()).flagged, vec!["src/app.py"]);
}

#[test]
fn build_output_ignored_by_a_tracked_gitignore_is_not_reported() {
    let repo = repo();
    let root = repo.path();
    write(root, "build/out.bin", "built\n");
    write(root, "src/app.o", "object\n");
    // New and not committed: `git add -A` takes it, so its rules are signed.
    write(root, "docs/.gitignore", "*.tmp\n");
    write(root, "docs/draft.tmp", "draft\n");
    // A pattern that hides nothing on disk is nobody's problem.
    exclude(root, "scratch/");

    assert_eq!(hidden(root), HiddenFiles::default());
}

#[test]
fn only_the_file_with_an_outside_rule_is_reported_in_a_directory_of_ignored_files() {
    let repo = repo();
    let root = repo.path();
    exclude(root, "mixed/planted.py");
    write(root, "mixed/a.o", "object\n");
    write(root, "mixed/planted.py", "import os\n");

    assert_eq!(
        hidden(root).ignored,
        vec![ignored("mixed/planted.py", ".git/info/exclude")]
    );
}

#[test]
fn untracked_gitignore_that_ignores_itself_does_not_vouch_for_anything() {
    let repo = repo();
    let root = repo.path();
    write(root, "tools/.gitignore", "*\n");
    write(
        root,
        "tools/conftest.py",
        "collect_ignore = ['test_x.py']\n",
    );
    assert_eq!(git(root, &["status", "--porcelain"]), "");

    let paths: Vec<String> = hidden(root).ignored.into_iter().map(|f| f.path).collect();
    assert!(
        paths.contains(&"tools/conftest.py".to_string()),
        "{paths:?}"
    );
}

#[test]
fn sparse_checkout_is_not_reported() {
    let repo = repo();
    let root = repo.path();
    git(root, &["sparse-checkout", "set", "docs"]);
    assert!(!root.join("src").exists());
    assert!(git(root, &["ls-files", "-v"]).contains("S src/app.py"));

    assert_eq!(hidden(root), HiddenFiles::default());
}

#[test]
fn rule_in_the_shared_exclude_file_reaches_a_linked_worktree() {
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
    exclude(repo.path(), "conftest.py");
    write(&tree, "conftest.py", "collect_ignore = ['test_x.py']\n");

    let found = hidden(&tree).ignored;
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].path, "conftest.py");
    assert!(found[0].source.ends_with("info/exclude"), "{found:?}");
}

#[test]
fn long_list_is_cut_and_counted() {
    let found = HiddenFiles {
        flagged: (0..8).map(|n| format!("t{n}.py")).collect(),
        ignored: Vec::new(),
    };
    let reasons = found.reasons();
    assert!(reasons[0].starts_with("8 tracked file(s)"), "{reasons:?}");
    assert!(reasons[0].contains("t4.py (+3 more)"), "{reasons:?}");
    assert!(!reasons[0].contains("t5.py"), "{reasons:?}");
}
