#![allow(clippy::unwrap_used, clippy::expect_used)]
//! What needs the inside of the crate: a stand-in for the `git` program and
//! the sweep of a dead run's temporary files. Real git otherwise.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{GIT_TIMEOUT, GitCall, stale, tree_fp, tree_fp_using};

fn real_git(repo: &Path, args: &[&str]) {
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
}

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    real_git(dir.path(), &["init", "-q", "-b", "main"]);
    fs::write(dir.path().join("a.txt"), "a\n").unwrap();
    real_git(dir.path(), &["add", "-A"]);
    real_git(dir.path(), &["commit", "-q", "-m", "init"]);
    dir
}

/// A `git` from before 2.34: `add` does not know `--sparse` and answers the
/// way git answers any unknown option. Everything else is the real git.
fn git_without_add_sparse(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = "#!/bin/sh\n\
        for arg in \"$@\"; do\n\
        \x20 if [ \"$arg\" = \"--sparse\" ]; then\n\
        \x20   echo \"error: unknown option \\`sparse'\" >&2\n\
        \x20   echo \"usage: git add [<options>] [--] <pathspec>...\" >&2\n\
        \x20   exit 129\n\
        \x20 fi\n\
        done\n\
        exec git \"$@\"\n";
    let path = dir.join("old-git");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn failed_git_command_says_what_git_said() {
    let repo = repo();
    let error = crate::git(repo.path(), &["add", "--no-such-option"], &[], GIT_TIMEOUT)
        .unwrap_err()
        .to_string();
    assert!(error.contains("add: exit Some(129)"), "{error}");
    assert!(error.contains("unknown option"), "{error}");
}

#[test]
fn git_without_add_sparse_is_told_apart_and_the_needed_version_is_named() {
    let repo = repo();
    let tools = tempfile::tempdir().unwrap();
    let old_git = git_without_add_sparse(tools.path());
    let call = GitCall {
        program: Some(OsStr::new(&old_git)),
        repo: repo.path(),
        timeout: GIT_TIMEOUT,
    };

    let error = tree_fp_using(&call).unwrap_err().to_string();

    assert!(error.contains("unknown option"), "{error}");
    assert!(error.contains("git 2.34 or newer"), "{error}");
    // What is installed, so the reader need not go and look.
    assert!(error.contains("git version "), "{error}");
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("aaf-"))
        .collect();
    names.sort();
    names
}

/// The pid of a process that has ended and been reaped.
fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

#[test]
fn leftovers_of_a_killed_run_are_removed_and_a_run_in_progress_is_not() {
    let repo = repo();
    let git_dir = repo.path().join(".git");
    let dead = dead_pid();
    // The test runner's parent (cargo) is alive for as long as this runs.
    let alive = std::os::unix::process::parent_id();
    let own = std::process::id();
    let blob = |dir: String| {
        let dir = git_dir.join(dir).join("ab");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("cdef"), "blob of a file nobody committed").unwrap();
    };
    blob(format!("aaf-objects-{dead}-7"));
    fs::write(git_dir.join(format!("aaf-index-{dead}-7")), "index").unwrap();
    fs::write(git_dir.join(format!("aaf-index-{dead}-7.lock")), "").unwrap();
    blob(format!("aaf-objects-{alive}-3"));
    // Another thread of this process, in the middle of its own run.
    blob(format!("aaf-objects-{own}-999999"));

    tree_fp(repo.path(), GIT_TIMEOUT).unwrap();

    let mut kept = vec![
        format!("aaf-objects-{alive}-3"),
        format!("aaf-objects-{own}-999999"),
    ];
    kept.sort();
    assert_eq!(names(&git_dir), kept);
}

#[test]
fn sweep_touches_only_names_of_its_own_making() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["aaf-objects-12-0", "aaf-objects-x-0", "aaf-notes-12-0"] {
        fs::create_dir(dir.path().join(name)).unwrap();
    }
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("kept"), "kept").unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("aaf-objects-13-0")).unwrap();

    stale::sweep(dir.path(), 1, &|_| false);

    assert_eq!(names(dir.path()), vec!["aaf-notes-12-0", "aaf-objects-x-0"]);
    // The link went, not what it pointed at.
    assert!(outside.path().join("kept").exists());
}

#[test]
fn only_a_missing_process_counts_as_dead() {
    assert!(stale::process_alive(std::process::id()));
    assert!(!stale::process_alive(dead_pid()));
}

/// Ends the stand-in's helper whatever the test's outcome.
struct KillOnDrop(PathBuf);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Ok(pid) = fs::read_to_string(&self.0) {
            let _ = Command::new("/bin/kill")
                .args(["-KILL", pid.trim()])
                .status();
        }
    }
}

/// A `git` that answers, then exits with `code`, but leaves a process that
/// holds its stdout and stderr open: what a copy of the pipe that leaked
/// into another child does (see `output.rs`), and a detached helper too.
fn git_leaving_a_holder(dir: &Path, code: u8) -> (PathBuf, KillOnDrop) {
    use std::os::unix::fs::PermissionsExt;
    let pid_file = dir.join(format!("holder-{code}.pid"));
    let script = format!(
        "#!/bin/sh\n/bin/sleep 30 &\necho $! > '{}'\necho answer\necho complaint >&2\nexit {code}\n",
        pid_file.display()
    );
    let path = dir.join(format!("git-{code}"));
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    (path, KillOnDrop(pid_file))
}

#[test]
fn a_finished_git_call_does_not_wait_for_another_holder_of_its_output() {
    let repo = repo();
    let tools = tempfile::tempdir().unwrap();
    for code in [0, 3] {
        let (fake, _holder) = git_leaving_a_holder(tools.path(), code);
        let call = GitCall {
            program: Some(OsStr::new(&fake)),
            repo: repo.path(),
            timeout: GIT_TIMEOUT,
        };
        let result = crate::git_raw(&call, &["rev-parse"], &[], None)
            .map_err(|failure| failure.into_error("rev-parse").to_string());
        if code == 0 {
            assert_eq!(result, Ok("answer\n".to_string()));
        } else {
            // What the command said is in the error, not lost to the wait.
            let error = result.unwrap_err();
            assert!(error.contains("exit Some(3): complaint"), "{error}");
        }
    }
}

/// The input's end reaches the child while another copy of the writing end
/// is still open, as one that leaked into another child is.
#[test]
fn the_end_of_the_input_reaches_the_child_past_a_leaked_copy() {
    let (stdin, input) = crate::output::input().unwrap();
    let exited = crate::output::Exited::new();
    let (stdout, mut output) = crate::output::output(&exited).unwrap();
    let mut cat = Command::new("/bin/cat");
    cat.stdin(stdin).stdout(stdout);
    let mut child = cat.spawn().unwrap();
    drop(cat);
    let leaked = input.0.try_clone().unwrap();
    input.finish(b"all of it").unwrap();
    let status = child.wait().unwrap();
    exited.mark();
    let mut seen = Vec::new();
    std::io::Read::read_to_end(&mut output, &mut seen).unwrap();
    drop(leaked);
    assert!(status.success());
    assert_eq!(seen, b"all of it");
}

/// A name that resolves to no commit is `None`; a git that cannot answer
/// is an error, not `None`.
#[test]
fn git_or_none_tells_none_from_no_answer() {
    let commit = |dir: &Path, name: &str| {
        let spec = format!("{name}^{{commit}}");
        crate::git_or_none(
            dir,
            &["rev-parse", "--verify", "--quiet", &spec],
            GIT_TIMEOUT,
        )
    };
    let repo = repo();
    let head = commit(repo.path(), "HEAD").unwrap();
    assert_eq!(head.map(|sha| sha.len()), Some(40));
    assert_eq!(commit(repo.path(), "no-such-branch").unwrap(), None);
    let outside = tempfile::tempdir().unwrap();
    let error = commit(outside.path(), "HEAD").unwrap_err().to_string();
    assert!(error.contains("rev-parse: exit Some(128)"), "{error}");
}
