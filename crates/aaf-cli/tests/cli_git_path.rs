//! Which `git` the built `aaf` runs: the one `PATH` named when it started,
//! never one inside the repository it works in, and never one that appears
//! in a `PATH` directory while it runs.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "cli_support.rs"]
mod support;

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use support::{Sandbox, make_executable, write};

/// The test process's own git, to which every stand-in hands over.
fn real_git() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("git"))
        .find(|candidate| candidate.is_file())
        .expect("git on the test's PATH")
}

/// A `git` that records each call in `log` and then runs the real git.
fn recording_git(dir: &Path, log: &Path) -> PathBuf {
    let path = dir.join("git");
    write(
        &path,
        &format!(
            "#!/bin/sh\necho call >> '{}'\nexec '{}' \"$@\"\n",
            log.display(),
            real_git().display()
        ),
    );
    make_executable(&path);
    path
}

fn calls(log: &Path) -> usize {
    fs::read_to_string(log).map_or(0, |text| text.lines().count())
}

fn path_of(dirs: &[&Path]) -> OsString {
    let mut all: Vec<PathBuf> = dirs.iter().map(|dir| dir.to_path_buf()).collect();
    all.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::env::join_paths(all).unwrap()
}

fn root(sb: &Sandbox) -> PathBuf {
    sb.home.parent().unwrap().to_path_buf()
}

#[test]
fn a_git_inside_the_repository_early_on_path_is_not_run() {
    let sb = Sandbox::new();
    let root = root(&sb);
    let planted_dir = sb.repo.join("tools");
    let planted_log = root.join("planted.log");
    recording_git(&planted_dir, &planted_log);
    let outside_dir = root.join("outside");
    let outside_log = root.join("outside.log");
    recording_git(&outside_dir, &outside_log);

    let out = sb
        .command(&["doctor", "--json"])
        .env("PATH", path_of(&[&planted_dir, &outside_dir, &sb.bin]))
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        calls(&planted_log),
        0,
        "the git inside the repository ran: {stdout}"
    );
    assert!(calls(&outside_log) > 0, "no git ran at all: {stdout}");
    assert!(
        stdout.contains(&outside_dir.join("git").display().to_string()),
        "doctor does not name the git it runs: {stdout}"
    );
}

/// The neighbour of the test above: the same stand-in outside the
/// repository is a git like any other and is the one run.
#[test]
fn a_git_outside_the_repository_early_on_path_is_the_one_run() {
    let sb = Sandbox::new();
    let root = root(&sb);
    let early_dir = root.join("early");
    let early_log = root.join("early.log");
    recording_git(&early_dir, &early_log);

    let out = sb
        .command(&["doctor", "--json"])
        .env("PATH", path_of(&[&early_dir, &sb.bin]))
        .output()
        .unwrap();

    assert!(
        calls(&early_log) > 0,
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn a_git_planted_on_path_after_aaf_started_is_not_picked_up() {
    let sb = Sandbox::new();
    let root = root(&sb);
    // Empty when aaf starts; the first git call fills it.
    let early_dir = root.join("early");
    fs::create_dir_all(&early_dir).unwrap();
    let planted_log = root.join("planted.log");
    let template = recording_git(&root.join("template"), &planted_log);
    let first_dir = root.join("first");
    let first_log = root.join("first.log");
    let first = first_dir.join("git");
    write(
        &first,
        &format!(
            "#!/bin/sh\necho call >> '{log}'\n[ -e '{early}/git' ] || cp '{template}' \
             '{early}/git'\nexec '{real}' \"$@\"\n",
            log = first_log.display(),
            early = early_dir.display(),
            template = template.display(),
            real = real_git().display(),
        ),
    );
    make_executable(&first);

    let out = sb
        .command(&["doctor", "--json"])
        .env("PATH", path_of(&[&early_dir, &first_dir, &sb.bin]))
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        early_dir.join("git").is_file(),
        "the first git call did not plant: {stdout}"
    );
    assert_eq!(
        calls(&planted_log),
        0,
        "a git planted after start ran: {stdout}"
    );
    assert!(
        calls(&first_log) >= 2,
        "doctor made fewer than two git calls, so nothing was proven: {stdout}"
    );
}
