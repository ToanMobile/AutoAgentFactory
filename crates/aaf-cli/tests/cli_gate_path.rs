//! Which `git` and which shell `aaf gate` runs: never a `git` or `sh` inside
//! the repository, even when a directory of it comes first on `PATH`. The
//! gate's own git calls (the change list) use the `git` looked up once at
//! start outside the repository, and the repo's commands run under `/bin/sh`,
//! both the light ones (lint) and those that go through the throttle (test).

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "cli_support.rs"]
mod support;

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use support::{Sandbox, make_executable, write};

/// The test process's own git, to which every stand-in hands over.
fn real_git() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("git"))
        .find(|candidate| candidate.is_file())
        .expect("git on the test's PATH")
}

/// A program `name` in `dir` that records each call in `log` and then runs
/// `real` with the same arguments.
fn recording(dir: &Path, name: &str, log: &Path, real: &Path) {
    let path = dir.join(name);
    write(
        &path,
        &format!(
            "#!/bin/sh\necho call >> '{}'\nexec '{}' \"$@\"\n",
            log.display(),
            real.display()
        ),
    );
    make_executable(&path);
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

/// A repo with `commands` as its `[commands]` table and a machine config
/// that does not wait for the CPU load, plus a `git` and a `sh` committed in
/// `tools/` that record their calls in `planted-git.log` / `planted-sh.log`.
fn repo_with_planted_tools(commands: &str) -> (Sandbox, PathBuf) {
    let sb = Sandbox::new();
    write(
        &sb.repo.join(".autofactory/config.toml"),
        &format!("[commands]\n{commands}"),
    );
    support::calm_machine(&sb, 1);
    let planted_dir = sb.repo.join("tools");
    recording(
        &planted_dir,
        "git",
        &root(&sb).join("planted-git.log"),
        &real_git(),
    );
    recording(
        &planted_dir,
        "sh",
        &root(&sb).join("planted-sh.log"),
        Path::new("/bin/sh"),
    );
    // Committed, so the stand-ins are ordinary tracked files of the task.
    sb.commit_all("tools");
    (sb, planted_dir)
}

fn gate(sb: &Sandbox, path: OsString) -> (Value, String) {
    let out = sb
        .command(&["gate", "--json"])
        .env("PATH", path)
        .output()
        .unwrap();
    let text = format!(
        "{} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (support::one_object(&out), text)
}

/// Light path only (no test command, so no throttle and no wait for memory
/// pressure): the change list and the lint command.
#[test]
fn a_git_and_a_sh_inside_the_repository_early_on_path_are_not_run_by_the_gate() {
    let marker = tempfile::TempDir::new().unwrap();
    let ran = marker.path().join("lint.ran");
    let (sb, planted_dir) =
        repo_with_planted_tools(&format!("lint = \"echo ran > '{}'\"\n", ran.display()));

    let (out, text) = gate(&sb, path_of(&[&planted_dir, &sb.bin]));

    assert_eq!(
        calls(&root(&sb).join("planted-git.log")),
        0,
        "the gate ran the repo's git: {text}"
    );
    assert_eq!(
        calls(&root(&sb).join("planted-sh.log")),
        0,
        "the gate ran the repo's sh: {text}"
    );
    assert!(ran.is_file(), "the lint command did not run: {text}");
    // No test command: the gate cannot verify, and nothing else blocks.
    assert_eq!(out["status"], json!("UNVERIFIED"), "{text}");
}

/// The same for a command that goes through the throttle, which builds its
/// own argument vector (and priority wrappers that look the shell up again).
#[test]
fn a_sh_inside_the_repository_early_on_path_is_not_run_for_a_throttled_command() {
    if !support::memory_is_calm("a_sh_inside_the_repository_throttled") {
        return;
    }
    let (sb, planted_dir) = repo_with_planted_tools("test_scoped = \"true\"\n");

    let (out, text) = gate(&sb, path_of(&[&planted_dir, &sb.bin]));

    assert_eq!(
        calls(&root(&sb).join("planted-sh.log")),
        0,
        "the throttled command ran under the repo's sh: {text}"
    );
    assert_eq!(out["status"], json!("CLEAR"), "{text}");
}

/// The neighbour: the same recording `git` outside the repository is a git
/// like any other and the gate's change list goes through it, so the first
/// test would see a call if the gate made one through `PATH`.
#[test]
fn a_git_outside_the_repository_early_on_path_is_the_one_the_gate_runs() {
    let (sb, _) = repo_with_planted_tools("lint = \"true\"\n");
    let early_dir = root(&sb).join("early");
    let early_log = root(&sb).join("early-git.log");
    recording(&early_dir, "git", &early_log, &real_git());

    let (out, text) = gate(&sb, path_of(&[&early_dir, &sb.bin]));

    assert!(
        calls(&early_log) > 0,
        "no git call went through PATH: {text}"
    );
    assert_eq!(out["status"], json!("UNVERIFIED"), "{text}");
}
