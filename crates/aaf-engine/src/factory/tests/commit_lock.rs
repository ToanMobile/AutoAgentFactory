//! `RealGit::commit_all` runs one `git commit` per repository at a time, so
//! `--max-parallel N` never fires N pre-commit hooks at once. Real `git`,
//! real hook scripts, real lock files in a temp dir.

use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::Barrier;
use std::time::{Duration, Instant};

use super::git_real::{Fixture, git};
use crate::factory::ports::GitOps;
use crate::factory::{RealGit, resolve_base};
use crate::git::TaskWorktree;
use crate::throttle::tests::exec_support::write_executable;

/// Long enough that only a hung test reaches it.
const HANG: Duration = Duration::from_secs(100);

fn commit_lock(fx: &Fixture) -> PathBuf {
    let name = format!("{}.commit.lock", aaf_fp::repo_id(&fx.repo));
    fx.tmp.path().join("data/locks").join(name)
}

/// Makes `script` the pre-commit hook of the repository and all its worktrees.
fn install_hook(fx: &Fixture, script: &str) {
    let hooks = fx.tmp.path().join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("pre-commit");
    write_executable(&hook, script);
    git(
        &fx.repo,
        &["config", "core.hooksPath", hooks.to_str().unwrap()],
    );
}

/// A task worktree with one change nobody committed yet.
fn uncommitted(fx: &Fixture, id: &str) -> TaskWorktree {
    let tree = fx.ops.task_worktree(id).unwrap();
    fs::write(tree.path.join("work.txt"), id).unwrap();
    tree
}

/// The ids of the hook log lines that begin with `word`.
fn logged(log: &str, word: &str) -> Vec<String> {
    log.lines()
        .filter_map(|line| line.strip_prefix(word))
        .map(|id| id.trim().to_string())
        .collect()
}

/// True once `commit_all` is past `git add` in `tree` (staged or committed).
fn past_add(tree: &Path) -> bool {
    // Without optional locks: this reading must not take the index lock the
    // commit under test needs.
    !git(tree, &["--no-optional-locks", "status", "--porcelain"]).contains("??")
}

fn held(lock: &Path) -> bool {
    let Ok(file) = File::options().write(true).open(lock) else {
        return false;
    };
    match file.try_lock() {
        Ok(()) => {
            file.unlock().unwrap();
            false
        }
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(error)) => panic!("{error}"),
    }
}

#[test]
fn four_concurrent_commits_run_their_pre_commit_hooks_one_at_a_time() {
    let fx = Fixture::new();
    let trees: Vec<TaskWorktree> = ["c1", "c2", "c3", "c4"]
        .iter()
        .map(|id| uncommitted(&fx, id))
        .collect();
    let dir = fx.tmp.path().join("hook-state");
    fs::create_dir_all(&dir).unwrap();
    // The hook announces itself and then stays inside the commit until the
    // test lets this very hook go: whatever else can start, starts.
    install_hook(
        &fx,
        &format!(
            "#!/bin/sh\ndir='{}'\necho \"start $$\" >> \"$dir/log\"\n\
             while [ ! -f \"$dir/go.$$\" ] && [ ! -f \"$dir/abort\" ]; do sleep 0.02; done\n\
             echo \"end $$\" >> \"$dir/log\"\n",
            dir.display()
        ),
    );
    let lock = commit_lock(&fx);

    // What the test saw each time it found a hook running: how many hooks
    // were inside a commit, and whether the commit lock was held.
    let mut seen: Vec<(usize, bool)> = Vec::new();
    let mut hung = false;
    let start = Barrier::new(trees.len());
    let results = std::thread::scope(|scope| {
        let handles: Vec<_> = trees
            .iter()
            .map(|tree| {
                let (fx, start) = (&fx, &start);
                scope.spawn(move || {
                    start.wait();
                    fx.ops.commit_all(tree, "aaf: concurrent")
                })
            })
            .collect();
        let deadline = Instant::now() + HANG;
        let mut released: Vec<String> = Vec::new();
        while !handles.iter().all(|handle| handle.is_finished()) {
            if Instant::now() > deadline {
                hung = true;
                fs::write(dir.join("abort"), "").unwrap();
                break;
            }
            let log = fs::read_to_string(dir.join("log")).unwrap_or_default();
            let ended = logged(&log, "end");
            let running: Vec<String> = logged(&log, "start")
                .into_iter()
                .filter(|id| !ended.contains(id))
                .collect();
            // A hook is let go only when every other commit had the chance
            // to start beside it: all of them are past `git add`.
            if running.iter().any(|id| !released.contains(id))
                && trees.iter().all(|tree| past_add(&tree.path))
            {
                seen.push((running.len(), held(&lock)));
                for id in running {
                    fs::write(dir.join(format!("go.{id}")), "").unwrap();
                    released.push(id);
                }
            }
            // Polls the hooks' log; nothing is ordered by this pause.
            std::thread::sleep(Duration::from_millis(5));
        }
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });

    let log = fs::read_to_string(dir.join("log")).unwrap_or_default();
    assert!(!hung, "commits never finished: {log}");
    for result in results {
        result.unwrap();
    }
    assert_eq!(
        seen,
        [(1, true); 4],
        "(hooks running, commit lock held): {log}"
    );
    // Strictly `start a, end a, start b, end b, ...`.
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 8, "{log}");
    for pair in lines.chunks(2) {
        let id = pair[0]
            .strip_prefix("start ")
            .unwrap_or_else(|| panic!("{log}"));
        assert_eq!(pair[1], format!("end {id}"), "{log}");
    }
    for tree in &trees {
        assert_eq!(git(&tree.path, &["status", "--porcelain"]), "");
        assert_eq!(git(&tree.path, &["rev-list", "--count", "main..HEAD"]), "1");
    }
}

#[test]
fn a_commit_stays_out_while_another_process_holds_the_commit_lock_of_the_repository() {
    let fx = Fixture::new();
    let tree = uncommitted(&fx, "c1");
    let log = fx.tmp.path().join("hook.log");
    install_hook(
        &fx,
        &format!("#!/bin/sh\necho start >> '{}'\n", log.display()),
    );
    // An `aaf run` of another process would hold exactly this lock.
    let lock = commit_lock(&fx);
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    let other = File::create(&lock).unwrap();
    other.try_lock().unwrap();
    let impatient = RealGit::new(
        fx.repo.clone(),
        fx.tmp.path().join("worktrees"),
        fx.tmp.path().join("data/locks"),
        resolve_base(&fx.repo, "auto").unwrap(),
    )
    .with_commit_lock_timeout(Duration::ZERO);

    let refused = impatient.commit_all(&tree, "aaf: c1").unwrap_err();

    assert!(refused.to_string().contains("commit.lock"), "{refused}");
    assert!(!log.exists(), "the hook ran beside the other process");
    assert_eq!(git(&tree.path, &["rev-list", "--count", "main..HEAD"]), "0");

    other.unlock().unwrap();
    impatient.commit_all(&tree, "aaf: c1").unwrap();
    assert_eq!(fs::read_to_string(&log).unwrap(), "start\n");
    assert_eq!(git(&tree.path, &["rev-list", "--count", "main..HEAD"]), "1");
    // Another repository commits under a lock of its own.
    assert!(!held(&lock), "released with the commit");
}
