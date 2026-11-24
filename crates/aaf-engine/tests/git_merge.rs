#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "git_support.rs"]
mod support;

use std::fs::{self, File};
use std::path::Path;
use std::sync::{Arc, Barrier, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use aaf_engine::git::GitError;
use aaf_engine::git::merge_queue::{self, MergeOutcome, MergeRequest};
use aaf_engine::git::worktree::{self, TaskWorktree};
use support::{Fixture, commit_file, git};

fn pass(_: &Path) -> Result<bool, String> {
    Ok(true)
}

fn request(fx: &Fixture, wt: &TaskWorktree) -> MergeRequest {
    MergeRequest {
        repo_root: fx.repo.clone(),
        worktree: wt.clone(),
        base: "main".to_string(),
        remote: Some("origin".to_string()),
        locks_dir: fx.locks.clone(),
        lock_timeout: Duration::from_secs(30),
    }
}

/// A task worktree with one commit adding `file`.
fn task(fx: &Fixture, id: &str, file: &str, content: &str) -> TaskWorktree {
    let wt = worktree::create(&fx.repo, &fx.worktrees, id, "main").unwrap();
    commit_file(&wt.path, file, content, &format!("task {id}"));
    wt
}

fn assert_lock_is_free(fx: &Fixture) {
    let lock = File::open(merge_queue::lock_path(&fx.locks, &fx.repo)).unwrap();
    lock.try_lock().expect("the merge lock is still held");
    // By hand, not by the close: a child another test thread is spawning
    // holds a copy of the descriptor until its exec, and the lock with it.
    lock.unlock().unwrap();
}

fn assert_no_rebase_in_progress(wt: &TaskWorktree) {
    for state in ["rebase-merge", "rebase-apply"] {
        let dir = git(&wt.path, &["rev-parse", "--git-path", state]);
        assert!(!wt.path.join(dir).exists(), "{state} left behind");
    }
    assert_eq!(
        git(&wt.path, &["symbolic-ref", "--short", "HEAD"]),
        wt.branch
    );
    assert_eq!(git(&wt.path, &["status", "--porcelain"]), "");
}

#[test]
fn fast_forwards_the_checked_out_local_base() {
    let fx = Fixture::new();
    let wt = task(&fx, "t1", "a.txt", "a\n");
    let tip = fx.head(&wt.path, "HEAD");

    let outcome = merge_queue::merge(&request(&fx, &wt), &pass).unwrap();
    assert_eq!(
        outcome,
        MergeOutcome::Merged {
            new_base_sha: tip.clone()
        }
    );
    assert_eq!(fx.head(&fx.repo, "main"), tip);
    // The main work tree follows its branch instead of showing a revert.
    assert!(fx.repo.join("a.txt").is_file());
    assert_eq!(git(&fx.repo, &["status", "--porcelain"]), "");
    assert_lock_is_free(&fx);
}

#[test]
fn fast_forwards_a_local_base_that_is_not_checked_out() {
    let fx = Fixture::new();
    let wt = task(&fx, "t1", "a.txt", "a\n");
    git(&fx.repo, &["switch", "-q", "-c", "elsewhere"]);
    let tip = fx.head(&wt.path, "HEAD");

    let outcome = merge_queue::merge(&request(&fx, &wt), &pass).unwrap();
    assert_eq!(
        outcome,
        MergeOutcome::Merged {
            new_base_sha: tip.clone()
        }
    );
    assert_eq!(fx.head(&fx.repo, "main"), tip);
    assert!(!fx.repo.join("a.txt").exists());
}

#[test]
fn refuses_when_the_checked_out_base_tree_is_dirty() {
    let fx = Fixture::new();
    let wt = task(&fx, "t1", "a.txt", "a\n");
    let before = fx.head(&fx.repo, "main");
    fs::write(fx.repo.join("README.md"), "local edit\n").unwrap();

    let outcome = merge_queue::merge(&request(&fx, &wt), &pass).unwrap();
    assert_eq!(
        outcome,
        MergeOutcome::BaseCheckoutDirty {
            path: fx.repo.clone()
        }
    );
    assert_eq!(fx.head(&fx.repo, "main"), before);
    assert_eq!(
        fs::read_to_string(fx.repo.join("README.md")).unwrap(),
        "local edit\n"
    );
    assert_lock_is_free(&fx);
}

#[test]
fn refuses_a_task_worktree_with_uncommitted_work() {
    let fx = Fixture::new();
    let wt = task(&fx, "t1", "a.txt", "a\n");
    fs::write(wt.path.join("wip.txt"), "wip").unwrap();
    let before = fx.head(&fx.repo, "main");

    let err = merge_queue::merge(&request(&fx, &wt), &pass).unwrap_err();
    assert!(
        matches!(err, GitError::Dirty(ref p) if p == &wt.path),
        "{err}"
    );
    assert_eq!(fx.head(&fx.repo, "main"), before);
}

#[test]
fn fast_forwards_the_remote_base_with_a_plain_push() {
    let fx = Fixture::with_remote();
    let remote = fx.remote.clone().unwrap();
    let wt = task(&fx, "t1", "a.txt", "a\n");
    let tip = fx.head(&wt.path, "HEAD");

    let outcome = merge_queue::merge(&request(&fx, &wt), &pass).unwrap();
    assert_eq!(
        outcome,
        MergeOutcome::Merged {
            new_base_sha: tip.clone()
        }
    );
    assert_eq!(fx.head(&remote, "main"), tip);
    assert_lock_is_free(&fx);
}

#[test]
fn rebases_onto_a_remote_base_that_moved() {
    let fx = Fixture::with_remote();
    let remote = fx.remote.clone().unwrap();
    let wt = task(&fx, "t1", "a.txt", "a\n");
    // Someone else lands on the remote after the task branched.
    // It goes through a second clone, so this repository's remote-tracking
    // ref is stale and only a fetch can reveal the new base.
    let clone = fx.repo.parent().unwrap().join("clone");
    git(
        &fx.repo,
        &[
            "clone",
            "-q",
            remote.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    for (key, value) in [("user.name", "other"), ("user.email", "o@example.invalid")] {
        git(&clone, &["config", key, value]);
    }
    git(&clone, &["config", "core.hooksPath", "/dev/null"]);
    git(&clone, &["config", "commit.gpgsign", "false"]);
    let other = commit_file(&clone, "other.txt", "o\n", "other work");
    git(&clone, &["push", "-q", "origin", "main"]);

    let outcome = merge_queue::merge(&request(&fx, &wt), &pass).unwrap();
    let MergeOutcome::Merged { new_base_sha } = outcome else {
        panic!("unexpected outcome: {outcome:?}");
    };
    assert_eq!(fx.head(&remote, "main"), new_base_sha);
    assert_eq!(fx.head(&remote, "main^"), other);
    assert_eq!(
        git(&remote, &["ls-tree", "--name-only", "main"]),
        "README.md\na.txt\nother.txt"
    );
}

#[test]
fn a_local_base_that_moved_gives_a_clean_rebase_then_a_merge() {
    let fx = Fixture::new();
    let wt = task(&fx, "t1", "a.txt", "a\n");
    let other = commit_file(&fx.repo, "other.txt", "o\n", "other work");

    let outcome = merge_queue::merge(&request(&fx, &wt), &pass).unwrap();
    let MergeOutcome::Merged { new_base_sha } = outcome else {
        panic!("unexpected outcome: {outcome:?}");
    };
    assert_eq!(fx.head(&fx.repo, "main"), new_base_sha);
    assert_eq!(fx.head(&fx.repo, "main^"), other);
    assert!(fx.repo.join("a.txt").is_file() && fx.repo.join("other.txt").is_file());
}

#[test]
fn a_rebase_conflict_is_aborted_and_reported_with_its_files() {
    let fx = Fixture::new();
    let wt = task(&fx, "t1", "README.md", "task version\n");
    let task_tip = fx.head(&wt.path, "HEAD");
    let base_tip = commit_file(&fx.repo, "README.md", "base version\n", "base edit");

    let outcome = merge_queue::merge(&request(&fx, &wt), &|_| {
        panic!("the gate must not run on a conflicted branch")
    })
    .unwrap();
    assert_eq!(
        outcome,
        MergeOutcome::Conflict {
            files: vec!["README.md".to_string()]
        }
    );
    assert_no_rebase_in_progress(&wt);
    assert_eq!(fx.head(&wt.path, "HEAD"), task_tip);
    assert_eq!(fx.head(&fx.repo, "main"), base_tip);
    assert_lock_is_free(&fx);
}

#[test]
fn a_failing_gate_leaves_the_base_where_it_was() {
    for with_remote in [false, true] {
        let fx = if with_remote {
            Fixture::with_remote()
        } else {
            Fixture::new()
        };
        let wt = task(&fx, "t1", "a.txt", "a\n");
        let before = fx.head(&fx.repo, "main");

        let gated = Mutex::new(Vec::new());
        let outcome = merge_queue::merge(&request(&fx, &wt), &|path| {
            gated.lock().unwrap().push(path.to_path_buf());
            Ok(false)
        })
        .unwrap();
        assert_eq!(outcome, MergeOutcome::GateFailed);
        assert_eq!(*gated.lock().unwrap(), std::slice::from_ref(&wt.path));
        assert_eq!(fx.head(&fx.repo, "main"), before);
        if let Some(remote) = &fx.remote {
            assert_eq!(fx.head(remote, "main"), before);
        }
        assert_lock_is_free(&fx);
    }
}

#[test]
fn a_gate_that_cannot_run_is_an_error_and_nothing_merges() {
    let fx = Fixture::new();
    let wt = task(&fx, "t1", "a.txt", "a\n");
    let before = fx.head(&fx.repo, "main");

    let err =
        merge_queue::merge(&request(&fx, &wt), &|_| Err("no config".to_string())).unwrap_err();
    assert!(
        matches!(err, GitError::Gate(ref m) if m == "no config"),
        "{err}"
    );
    assert_eq!(fx.head(&fx.repo, "main"), before);
    assert_lock_is_free(&fx);
}

#[test]
fn a_remote_base_that_moves_during_the_gate_is_not_forced() {
    let fx = Fixture::with_remote();
    let remote = fx.remote.clone().unwrap();
    let wt = task(&fx, "t1", "a.txt", "a\n");

    let outcome = merge_queue::merge(&request(&fx, &wt), &|_| {
        commit_file(&fx.repo, "late.txt", "l\n", "lands during the gate");
        git(&fx.repo, &["push", "-q", "origin", "main"]);
        Ok(true)
    })
    .unwrap();
    assert_eq!(outcome, MergeOutcome::NotFastForward);
    assert_eq!(fx.head(&remote, "main"), fx.head(&fx.repo, "main"));
    assert_lock_is_free(&fx);
}

#[test]
fn a_local_base_that_moves_during_the_gate_is_not_forced() {
    let fx = Fixture::new();
    let wt = task(&fx, "t1", "a.txt", "a\n");

    let late = Mutex::new(String::new());
    let outcome = merge_queue::merge(&request(&fx, &wt), &|_| {
        *late.lock().unwrap() = commit_file(&fx.repo, "late.txt", "l\n", "lands during the gate");
        Ok(true)
    })
    .unwrap();
    assert_eq!(outcome, MergeOutcome::NotFastForward);
    assert_eq!(fx.head(&fx.repo, "main"), *late.lock().unwrap());
}

#[test]
fn a_held_lock_times_out_and_a_released_one_lets_the_merge_through() {
    let fx = Fixture::new();
    let wt = task(&fx, "t1", "a.txt", "a\n");
    let before = fx.head(&fx.repo, "main");
    fs::create_dir_all(&fx.locks).unwrap();
    let path = merge_queue::lock_path(&fx.locks, &fx.repo);
    assert_eq!(
        path,
        fx.locks
            .join(format!("{}.merge.lock", aaf_fp::repo_id(&fx.repo)))
    );
    let holder = File::create(&path).unwrap();
    holder.lock().unwrap();

    let mut req = request(&fx, &wt);
    req.lock_timeout = Duration::from_millis(50);
    let err = merge_queue::merge(&req, &|_| panic!("ran without the lock")).unwrap_err();
    assert!(
        matches!(err, GitError::LockTimeout(ref p) if p == &path),
        "{err}"
    );
    assert_eq!(fx.head(&fx.repo, "main"), before);

    holder.unlock().unwrap();
    let outcome = merge_queue::merge(&req, &pass).unwrap();
    assert!(matches!(outcome, MergeOutcome::Merged { .. }));
}

#[test]
fn rejects_a_base_name_git_could_read_as_an_option() {
    let fx = Fixture::new();
    let wt = task(&fx, "t1", "a.txt", "a\n");
    for base in ["--force", "", "a..b", "with space"] {
        let mut req = request(&fx, &wt);
        req.base = base.to_string();
        let err = merge_queue::merge(&req, &pass).unwrap_err();
        assert!(matches!(err, GitError::Invalid(_)), "{base:?}: {err}");
    }
}

/// Counts how many gates run at once. The first gate waits a bounded time
/// for a second one to show up: under a working lock none can.
#[derive(Default)]
struct Overlap {
    state: Mutex<(u32, u32)>,
    entered: Condvar,
}

impl Overlap {
    fn gate(&self) -> Result<bool, String> {
        let mut state = self.state.lock().unwrap();
        state.0 += 1;
        state.1 = state.1.max(state.0);
        self.entered.notify_all();
        let (mut state, _) = self
            .entered
            .wait_timeout_while(state, Duration::from_millis(200), |s| s.1 < 2)
            .unwrap();
        state.0 -= 1;
        Ok(true)
    }
}

#[test]
fn two_threads_merging_different_branches_are_serialised_by_the_lock() {
    let fx = Fixture::new();
    let first = task(&fx, "t1", "a.txt", "a\n");
    let second = task(&fx, "t2", "b.txt", "b\n");
    let overlap = Arc::new(Overlap::default());
    let start = Arc::new(Barrier::new(2));

    let handles: Vec<_> = [first, second]
        .into_iter()
        .map(|wt| {
            let req = request(&fx, &wt);
            let (overlap, start) = (Arc::clone(&overlap), Arc::clone(&start));
            thread::spawn(move || {
                start.wait();
                merge_queue::merge(&req, &|_| overlap.gate())
            })
        })
        .collect();
    for handle in handles {
        let outcome = handle.join().unwrap().unwrap();
        assert!(
            matches!(outcome, MergeOutcome::Merged { .. }),
            "{outcome:?}"
        );
    }

    assert_eq!(overlap.state.lock().unwrap().1, 1, "two merges ran at once");
    assert!(fx.repo.join("a.txt").is_file() && fx.repo.join("b.txt").is_file());
    // Linear history: init, then the two task commits.
    assert_eq!(git(&fx.repo, &["rev-list", "--count", "main"]), "3");
    assert_eq!(git(&fx.repo, &["rev-list", "--merges", "main"]), "");
}
