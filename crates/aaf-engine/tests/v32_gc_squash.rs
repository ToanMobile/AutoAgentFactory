#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "git_support.rs"]
mod support;

use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{EventSink, RunStore, TaskStore};
use aaf_domain::run::{RunRecord, RunStatus};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::{Db, private};
use aaf_engine::factory::{GitOps, RealGit, resolve_base};
use aaf_engine::gc::{GcOptions, run_gc};
use aaf_engine::git::GitError;
use aaf_engine::git::worktree;
use aaf_engine::process::{CommandOutput, CommandRunner};
use serde_json::json;
use support::{Fixture, commit_file, git};

struct StubGh {
    result: Result<CommandOutput, io::ErrorKind>,
}

impl StubGh {
    fn ok_json(value: serde_json::Value) -> Self {
        Self {
            result: Ok(CommandOutput {
                exit_code: Some(0),
                stdout: value.to_string(),
                stderr: String::new(),
            }),
        }
    }

    fn exit_err() -> Self {
        Self {
            result: Ok(CommandOutput {
                exit_code: Some(1),
                stdout: String::new(),
                stderr: "offline".to_string(),
            }),
        }
    }

    fn io_err(kind: io::ErrorKind) -> Self {
        Self { result: Err(kind) }
    }
}

impl CommandRunner for StubGh {
    fn run(&self, program: &str, _args: &[&str], _cwd: &Path) -> io::Result<CommandOutput> {
        assert_eq!(program, "gh");
        match &self.result {
            Ok(out) => Ok(out.clone()),
            Err(kind) => Err(io::Error::new(*kind, "stubbed gh error")),
        }
    }
}

fn spec() -> ImmutableSpec {
    ImmutableSpec {
        description: "Test task".to_string(),
        acceptance: vec!["passes".to_string()],
    }
}

fn insert_task(db: &Db, repo: &Path, id: &str, state: TaskState) {
    let mut task = Task::new_root(
        id,
        repo.to_string_lossy(),
        format!("Task {id}"),
        spec(),
        Priority::P2,
        1_700_000_000,
    );
    task.state = state;
    db.insert_task(&task).unwrap();
}

fn set_mtime_past(path: &Path, age: Duration) {
    let file = File::options().write(true).open(path).unwrap();
    let target = SystemTime::now().checked_sub(age).unwrap();
    file.set_modified(target).unwrap();
}

fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

#[test]
fn squash_merge_verification_rejects_untrusted_rows_and_cleans_verified_merge() {
    let fx = Fixture::with_remote();
    let base = resolve_base(&fx.repo, "auto").unwrap();
    let real = RealGit::new(
        fx.repo.clone(),
        fx.worktrees.clone(),
        fx.locks.clone(),
        base,
    );

    let wt = real.task_worktree("t1").unwrap();
    let local_head = commit_file(&wt.path, "feat.txt", "hello\n", "feat commit");
    git(
        &fx.repo,
        &["update-ref", "refs/aaf/snapshots/t1/step-1", &local_head],
    );
    git(
        &fx.repo,
        &["update-ref", "refs/aaf/snapshots/t1/step-2", &local_head],
    );

    // Create an unrelated commit not reachable from origin/main.
    let wt_other = real.task_worktree("t_other").unwrap();
    let non_ancestor_oid = commit_file(&wt_other.path, "other.txt", "x\n", "unrelated");

    // Simulate a GitHub squash-merge on origin/main via a second clone so
    // `aaf/t1` is NOT an ancestor of origin/main and `fetch_base` must run.
    let remote_clone = fx.repo.parent().unwrap().join("remote_clone");
    let bare = fx.remote.as_ref().unwrap();
    let clone_status = Command::new("git")
        .args([
            "clone",
            "-q",
            bare.to_str().unwrap(),
            remote_clone.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(clone_status.success());
    for (k, v) in [
        ("user.name", "squash"),
        ("user.email", "squash@example.invalid"),
        ("commit.gpgsign", "false"),
        ("core.hooksPath", "/dev/null"),
    ] {
        git(&remote_clone, &["config", k, v]);
    }
    let squash_oid = commit_file(&remote_clone, "feat.txt", "hello\n", "squash PR (#1)");
    git(&remote_clone, &["push", "-q", "origin", "main"]);

    assert!(!real.is_merged("t1").unwrap());
    let candidates = vec![("t1".to_string(), local_head.clone())];

    // 1. Cross-repo PR rejected.
    let cross_repo = StubGh::ok_json(json!([{
        "headRefName": "aaf/t1",
        "headRefOid": local_head,
        "baseRefName": "main",
        "mergeCommit": { "oid": squash_oid },
        "isCrossRepository": true
    }]));
    assert!(
        real.verify_squash_merges_with(&candidates, &cross_repo)
            .unwrap()
            .is_empty()
    );

    // 2. Wrong baseRefName rejected.
    let wrong_base = StubGh::ok_json(json!([{
        "headRefName": "aaf/t1",
        "headRefOid": local_head,
        "baseRefName": "develop",
        "mergeCommit": { "oid": squash_oid },
        "isCrossRepository": false
    }]));
    assert!(
        real.verify_squash_merges_with(&candidates, &wrong_base)
            .unwrap()
            .is_empty()
    );

    // 3. Stale headRefOid rejected.
    let stale_oid = "0".repeat(40);
    let stale_head = StubGh::ok_json(json!([{
        "headRefName": "aaf/t1",
        "headRefOid": stale_oid,
        "baseRefName": "main",
        "mergeCommit": { "oid": squash_oid },
        "isCrossRepository": false
    }]));
    assert!(
        real.verify_squash_merges_with(&candidates, &stale_head)
            .unwrap()
            .is_empty()
    );

    // 4. Non-ancestor mergeCommit.oid rejected.
    let non_ancestor = StubGh::ok_json(json!([{
        "headRefName": "aaf/t1",
        "headRefOid": local_head,
        "baseRefName": "main",
        "mergeCommit": { "oid": non_ancestor_oid },
        "isCrossRepository": false
    }]));
    assert!(
        real.verify_squash_merges_with(&candidates, &non_ancestor)
            .unwrap()
            .is_empty()
    );

    // 5. Offline / failing gh returns empty map without crashing.
    assert!(
        real.verify_squash_merges_with(&candidates, &StubGh::exit_err())
            .unwrap()
            .is_empty()
    );
    assert!(
        real.verify_squash_merges_with(&candidates, &StubGh::io_err(io::ErrorKind::TimedOut))
            .unwrap()
            .is_empty()
    );

    // 6. Valid squash-merge passes all 5 checks.
    let valid = StubGh::ok_json(json!([{
        "headRefName": "aaf/t1",
        "headRefOid": local_head,
        "baseRefName": "main",
        "mergeCommit": { "oid": squash_oid },
        "isCrossRepository": false
    }]));
    let verified = real.verify_squash_merges_with(&candidates, &valid).unwrap();
    assert_eq!(verified.get("t1"), Some(&local_head));

    // 7. Dirty worktree fails closed with GitError::Dirty without losing files.
    fs::write(wt.path.join("uncommitted.txt"), "keep me\n").unwrap();
    let dirty_err = real.remove_verified_merged(&wt, &local_head).unwrap_err();
    assert!(matches!(dirty_err, GitError::Dirty(_)), "{dirty_err}");
    assert_eq!(
        fs::read_to_string(wt.path.join("uncommitted.txt")).unwrap(),
        "keep me\n"
    );
    assert_eq!(
        git(
            &fx.repo,
            &["branch", "--list", "--format=%(refname:short)", "aaf/t1"]
        ),
        "aaf/t1"
    );

    // 8. Clean worktree is removed along with branch and snapshot refs.
    fs::remove_file(wt.path.join("uncommitted.txt")).unwrap();
    real.remove_verified_merged(&wt, &local_head).unwrap();
    assert!(!wt.path.exists());
    assert_eq!(git(&fx.repo, &["branch", "--list", "aaf/t1"]), "");
    assert_eq!(
        git(
            &fx.repo,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/aaf/snapshots/t1/"
            ]
        ),
        ""
    );

    // 9. HEAD mismatch fails closed without deleting worktree or branch.
    let wt_moved = real.task_worktree("t_moved").unwrap();
    let old_oid = commit_file(&wt_moved.path, "m1.txt", "1\n", "c1");
    let _new_oid = commit_file(&wt_moved.path, "m2.txt", "2\n", "c2");
    let err = real
        .remove_verified_merged(&wt_moved, &old_oid)
        .unwrap_err();
    assert!(matches!(err, GitError::Command { .. }), "{err}");
    assert!(wt_moved.path.is_dir());
    assert_eq!(
        git(
            &fx.repo,
            &[
                "branch",
                "--list",
                "--format=%(refname:short)",
                "aaf/t_moved"
            ]
        ),
        "aaf/t_moved"
    );
}

#[test]
fn sweep_stale_removes_dead_temp_entries_and_keeps_live_ones() {
    let fx = Fixture::new();
    let git_dir = fx.repo.join(".git");
    let dead = dead_pid();
    let own = std::process::id();

    let dead_idx = git_dir.join(format!("aaf-index-{dead}-1"));
    let dead_obj = git_dir.join(format!("aaf-objects-{dead}-1"));
    let old_fp = git_dir.join("aaf-fp-stale");
    let fresh_fp = git_dir.join("aaf-fp-fresh");
    let own_idx = git_dir.join(format!("aaf-index-{own}-1"));

    fs::write(&dead_idx, "idx").unwrap();
    fs::create_dir_all(&dead_obj).unwrap();
    fs::write(&old_fp, "old").unwrap();
    set_mtime_past(&old_fp, Duration::from_secs(7200));
    fs::write(&fresh_fp, "fresh").unwrap();
    fs::write(&own_idx, "live").unwrap();

    let removed = aaf_fp::sweep_stale(&fx.repo).unwrap();
    assert_eq!(removed, 3);
    assert!(!dead_idx.exists());
    assert!(!dead_obj.exists());
    assert!(!old_fp.exists());
    assert!(fresh_fp.exists());
    assert!(own_idx.exists());
}

#[test]
fn gc_skips_busy_repo_when_run_lock_is_held() {
    let fx = Fixture::new();
    let data_dir = fx.repo.parent().unwrap().join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let db = Db::open(&data_dir.join("aaf.db")).unwrap();
    insert_task(&db, &fx.repo, "t-done", TaskState::Done);
    let wt = worktree::create(&fx.repo, &fx.worktrees, "t-done", "main").unwrap();

    let locks_dir = data_dir.join("locks");
    private::create_dir(&locks_dir).unwrap();
    let lock_path = locks_dir.join(format!("{}.run.lock", aaf_fp::repo_id(&fx.repo)));
    let held = private::open_file(&lock_path).unwrap();
    held.try_lock().unwrap();

    let opts = GcOptions {
        repo_root: Some(fx.repo.clone()),
        ..GcOptions::default()
    };
    let report = run_gc(&data_dir, &fx.worktrees, &opts).unwrap();
    drop(held);

    assert_eq!(report.skipped_busy.len(), 1);
    assert!(report.repos_scanned.is_empty());
    assert!(wt.path.is_dir(), "busy repo worktree must not be touched");
}

#[test]
fn gc_dry_run_and_full_cleanup_respect_cancelled_dirty_symlinks_running_and_protected_files() {
    let fx = Fixture::new();
    let root = fx.repo.parent().unwrap();
    let data_dir = root.join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let db = Db::open(&data_dir.join("aaf.db")).unwrap();

    insert_task(&db, &fx.repo, "t-done", TaskState::Done);
    insert_task(&db, &fx.repo, "t-ready", TaskState::Ready);
    insert_task(&db, &fx.repo, "t-canc-clean", TaskState::Cancelled);
    insert_task(&db, &fx.repo, "t-canc-dirty", TaskState::Cancelled);

    let wt_done = worktree::create(&fx.repo, &fx.worktrees, "t-done", "main").unwrap();
    let done_head = commit_file(&wt_done.path, "d.txt", "d\n", "done");
    db.emit(
        &FactoryEvent::new(
            EventKind::MergeResult,
            json!({ "merged": true, "kind": "squash_pr", "head_sha": done_head }).to_string(),
            1_700_000_001,
        )
        .for_task("t-done"),
    )
    .unwrap();
    git(
        &fx.repo,
        &["update-ref", "refs/aaf/snapshots/t-done/s1", &done_head],
    );
    let wt_ready = worktree::create(&fx.repo, &fx.worktrees, "t-ready", "main").unwrap();
    let wt_canc_clean = worktree::create(&fx.repo, &fx.worktrees, "t-canc-clean", "main").unwrap();
    let wt_canc_dirty = worktree::create(&fx.repo, &fx.worktrees, "t-canc-dirty", "main").unwrap();
    fs::write(wt_canc_dirty.path.join("wip.txt"), "unsaved\n").unwrap();
    let wt_batch = worktree::create(&fx.repo, &fx.worktrees, "batch-orphan1", "main").unwrap();

    // Active running run whose stream file must never be deleted even if old.
    db.insert_run(&RunRecord {
        id: "run-active".to_string(),
        task_id: "t-ready".to_string(),
        account_id: None,
        executor: "claude".to_string(),
        worktree_path: wt_ready.path.to_string_lossy().into_owned(),
        branch: wt_ready.branch.clone(),
        status: RunStatus::Running,
        exit_code: None,
        tokens_in: 0,
        tokens_out: 0,
        started_at: 1_700_000_000,
        ended_at: None,
    })
    .unwrap();

    // Streams, reports, symlink, and protected files.
    let streams = data_dir.join("streams");
    let reports = data_dir.join("reports");
    fs::create_dir_all(&streams).unwrap();
    fs::create_dir_all(&reports).unwrap();

    let expired_stream = streams.join("run-old.jsonl");
    let active_stream = streams.join("run-active.jsonl");
    let fresh_stream = streams.join("run-fresh.jsonl");
    let expired_report = reports.join("old.html");
    let target_file = root.join("symlink-target.jsonl");
    let symlink_stream = streams.join("link.jsonl");

    let twenty_days = Duration::from_secs(20 * 86_400);
    for p in [
        &expired_stream,
        &active_stream,
        &expired_report,
        &target_file,
    ] {
        fs::write(p, "data\n").unwrap();
        set_mtime_past(p, twenty_days);
    }
    fs::write(&fresh_stream, "fresh\n").unwrap();
    std::os::unix::fs::symlink(&target_file, &symlink_stream).unwrap();

    // Protected files/directories that gc must NEVER touch.
    let key = data_dir.join("receipt.key");
    let receipt_f = data_dir.join("receipts/r1.json");
    let approval_f = data_dir.join("approvals/a1.json");
    let proof_f = data_dir.join("proofs/p1.json");
    for (p, body) in [
        (&key, "secret-key"),
        (&receipt_f, "{\"r\":1}"),
        (&approval_f, "{\"a\":1}"),
        (&proof_f, "{\"p\":1}"),
    ] {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
        set_mtime_past(p, twenty_days);
    }

    // 1. Dry-run removes nothing.
    let dry = run_gc(
        &data_dir,
        &fx.worktrees,
        &GcOptions {
            repo_root: Some(fx.repo.clone()),
            dry_run: true,
            ..GcOptions::default()
        },
    )
    .unwrap();
    assert!(dry.dry_run);
    assert_eq!(dry.removed_orphan_batches, vec!["batch-orphan1"]);
    assert_eq!(dry.removed_worktrees.len(), 1);
    assert_eq!(dry.removed_snapshot_refs, 1);
    assert_eq!(dry.removed_files.len(), 2);
    assert!(wt_done.path.is_dir());
    assert!(wt_batch.path.is_dir());
    assert!(expired_stream.is_file());
    assert!(expired_report.is_file());

    // 2. Real run without --include-cancelled cleans Done, orphan batch, expired regular files.
    let pass1 = run_gc(
        &data_dir,
        &fx.worktrees,
        &GcOptions {
            repo_root: Some(fx.repo.clone()),
            ..GcOptions::default()
        },
    )
    .unwrap();
    assert_eq!(pass1.removed_orphan_batches, vec!["batch-orphan1"]);
    assert_eq!(pass1.removed_snapshot_refs, 1);
    assert!(!wt_done.path.exists());
    assert!(!wt_batch.path.exists());
    assert_eq!(git(&fx.repo, &["branch", "--list", "aaf/t-done"]), "");
    assert_eq!(
        git(&fx.repo, &["branch", "--list", "aaf/batch-orphan1"]),
        ""
    );
    assert_eq!(
        git(
            &fx.repo,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/aaf/snapshots/t-done/"
            ]
        ),
        ""
    );
    assert!(wt_ready.path.is_dir(), "Ready task must remain untouched");
    assert!(
        wt_canc_clean.path.is_dir(),
        "Cancelled needs --include-cancelled"
    );
    assert!(wt_canc_dirty.path.is_dir());
    assert!(!expired_stream.exists());
    assert!(!expired_report.exists());
    assert!(active_stream.is_file(), "Running run stream must stay");
    assert!(fresh_stream.is_file(), "Fresh stream must stay");
    assert!(
        symlink_stream.symlink_metadata().is_ok(),
        "Symlink must be skipped"
    );
    assert!(target_file.is_file(), "Symlink target must remain intact");

    // 3. --include-cancelled cleans clean Cancelled, skips dirty Cancelled.
    let pass2 = run_gc(
        &data_dir,
        &fx.worktrees,
        &GcOptions {
            repo_root: Some(fx.repo.clone()),
            include_cancelled: true,
            ..GcOptions::default()
        },
    )
    .unwrap();
    assert!(!wt_canc_clean.path.exists());
    assert_eq!(pass2.skipped_dirty, vec!["t-canc-dirty"]);
    assert!(wt_canc_dirty.path.join("wip.txt").is_file());

    // 4. --include-cancelled --force cleans dirty Cancelled too.
    let pass3 = run_gc(
        &data_dir,
        &fx.worktrees,
        &GcOptions {
            repo_root: Some(fx.repo.clone()),
            include_cancelled: true,
            force: true,
            ..GcOptions::default()
        },
    )
    .unwrap();
    assert!(pass3.skipped_dirty.is_empty());
    assert!(!wt_canc_dirty.path.exists());

    // 5. Protected files/dirs untouched.
    assert_eq!(fs::read_to_string(&key).unwrap(), "secret-key");
    assert!(data_dir.join("aaf.db").is_file());
    assert_eq!(fs::read_to_string(&receipt_f).unwrap(), "{\"r\":1}");
    assert_eq!(fs::read_to_string(&approval_f).unwrap(), "{\"a\":1}");
    assert_eq!(fs::read_to_string(&proof_f).unwrap(), "{\"p\":1}");
}
