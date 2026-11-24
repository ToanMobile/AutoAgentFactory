//! Merge-queue rebase hardening (audit a13 round 2, F4-A / F4-B): the gate
//! judges exactly the commit that lands, whatever the task's in-tree hooks do
//! and whatever moves the base ref while the rebase starts.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "git_support.rs"]
mod support;

use std::cell::RefCell;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use aaf_engine::git::GitError;
use aaf_engine::git::merge_queue::{self, MergeOutcome, MergeRequest};
use aaf_engine::git::worktree::{self, TaskWorktree};
use support::{Fixture, commit_file, git};

#[path = "../../aaf-fp/tests/exec_support.rs"]
mod exec_support;
use exec_support::write_executable;

const BASE_TEST: &str = "#[test]\nfn guarded() { assert!(true); }\n";
const LOCKED_EDIT: &str = "#[test]\nfn guarded() {}\n";

fn request(fx: &Fixture, wt: &TaskWorktree) -> MergeRequest {
    MergeRequest {
        repo_root: fx.repo.clone(),
        worktree: wt.clone(),
        base: "main".to_string(),
        remote: "origin".to_string(),
        locks_dir: fx.locks.clone(),
        lock_timeout: Duration::from_secs(30),
    }
}

fn write_exec(path: &Path, text: &str) {
    write_executable(path, text);
}

/// F4-A: a `post-rewrite` hook committed by the task (in-tree
/// `core.hooksPath`) puts the locked test file back to its base content in the
/// working tree after the rebase. The gate must not judge that tree while the
/// rebased HEAD, which still carries the locked edit, lands.
#[test]
fn rebase_hooks_cannot_show_the_gate_a_tree_other_than_head() {
    let fx = Fixture::new();
    fs::create_dir_all(fx.repo.join("tests")).unwrap();
    commit_file(&fx.repo, "tests/app_test.rs", BASE_TEST, "base test");
    // Husky-style in-tree hooks directory (fixtures normally use /dev/null).
    git(&fx.repo, &["config", "core.hooksPath", ".githooks"]);

    let wt = worktree::create(&fx.repo, &fx.worktrees, "t1", "main").unwrap();
    fs::create_dir_all(wt.path.join(".githooks")).unwrap();
    write_exec(
        &wt.path.join(".githooks/post-rewrite"),
        &format!("#!/bin/sh\nprintf '%s' '{BASE_TEST}' > tests/app_test.rs\n"),
    );
    fs::write(wt.path.join("tests/app_test.rs"), LOCKED_EDIT).unwrap();
    git(&wt.path, &["add", "-A"]);
    git(&wt.path, &["commit", "-q", "-m", "task t1"]);
    // The base moves on, so the merge queue's rebase really rewrites.
    commit_file(&fx.repo, "other.txt", "other\n", "base moves");

    let seen = RefCell::new(None);
    let regate = |tree: &Path| -> Result<bool, String> {
        *seen.borrow_mut() = Some(fs::read_to_string(tree.join("tests/app_test.rs")).unwrap());
        Ok(true)
    };
    let outcome = merge_queue::merge(&request(&fx, &wt), &regate);
    match outcome {
        Err(GitError::Dirty(_)) => {}
        Ok(MergeOutcome::Merged { new_base_sha }) => {
            let landed = git(
                &fx.repo,
                &["show", &format!("{new_base_sha}:tests/app_test.rs")],
            );
            let gated = seen.borrow().clone().expect("the gate ran");
            assert_eq!(
                gated.trim_end(),
                landed.trim_end(),
                "the gate judged a tree other than the commit that landed"
            );
        }
        other => panic!("unexpected outcome {other:?}"),
    }
}

const CHILD_ENV: &str = "AAF_A13_FX4_MOVE_DIR";

/// F4-B: the rebase must use the exact `onto_sha` the gate is told about, not
/// the ref name, which can move in between. A `git` wrapper early on `PATH`
/// commits on `main` when it sees the `rebase` subcommand; the scenario runs
/// in a child test process because aaf resolves `git` once per process from
/// the `PATH` it started with.
#[test]
fn rebase_uses_the_resolved_onto_sha() {
    let dir = tempfile::tempdir().unwrap();
    let dir = fs::canonicalize(dir.path()).unwrap();
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let real = aaf_fp::find_program(&path_var, "git", &[]).expect("git on PATH");
    write_exec(
        &dir.join("git"),
        &format!(
            "#!/bin/sh\nreal='{real}'\ndir='{dir}'\ncase \" $* \" in\n  *\" rebase --abort \"*) ;;\n  *\" rebase \"*)\n    if [ -f \"$dir/repo\" ] && [ ! -e \"$dir/moved\" ]; then\n      : > \"$dir/moved\"\n      \"$real\" -C \"$(cat \"$dir/repo\")\" commit -q --allow-empty -m moved || exit 97\n    fi ;;\nesac\nexec \"$real\" \"$@\"\n",
            real = real.display(),
            dir = dir.display()
        ),
    );
    let mut paths = vec![dir.clone()];
    paths.extend(std::env::split_paths(&path_var));
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "moved_base_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env(CHILD_ENV, &dir)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "child failed:\n{text}");
    assert!(text.contains("1 passed"), "child did not run:\n{text}");
    assert!(dir.join("moved").exists(), "the wrapper never moved main");
}

#[test]
#[ignore = "run by rebase_uses_the_resolved_onto_sha in a child process"]
fn moved_base_child() {
    let Some(dir) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    let dir = Path::new(&dir);
    let fx = Fixture::new();
    let wt = worktree::create(&fx.repo, &fx.worktrees, "t1", "main").unwrap();
    commit_file(&wt.path, "a.txt", "a\n", "task t1");
    fs::write(dir.join("repo"), fx.repo.to_str().unwrap()).unwrap();

    let req = request(&fx, &wt);
    let lock = merge_queue::acquire_lock(&req).unwrap();
    let seen = RefCell::new(None);
    let regate = |tree: &Path, onto: &str| -> Result<bool, String> {
        let parent = git(tree, &["rev-parse", "HEAD~1"]);
        *seen.borrow_mut() = Some((onto.to_string(), parent));
        Ok(true)
    };
    let outcome = merge_queue::merge_with_lock_onto(&req, &lock, &regate).unwrap();
    assert!(dir.join("moved").exists(), "the wrapper never moved main");
    let (onto, parent) = seen.borrow().clone().expect("the gate ran");
    assert_eq!(
        parent, onto,
        "the task was rebased onto a base other than the onto_sha the gate was told"
    );
    // The base moved past the gated one, so nothing lands without a new gate.
    assert_eq!(outcome, MergeOutcome::NotFastForward);
}
