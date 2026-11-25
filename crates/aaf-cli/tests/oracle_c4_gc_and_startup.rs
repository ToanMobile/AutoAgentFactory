#![allow(clippy::unwrap_used, clippy::expect_used)]
//! C4 oracle tests: `aaf gc` worktree/cwd/error-isolation/scratch hardening and
//! offline `startup_sweep` cleanup under `.run.lock` without self-deadlock.

#[path = "cli_support.rs"]
mod support;

use std::fs::{self, File};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use aaf_domain::ports::TaskStore;
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::executors::DEFAULT_WORKTREES_REL;
use aaf_engine::gate::stats_repo_id;
use aaf_engine::git::worktree;
use serde_json::json;
use support::{Sandbox, one_object};

fn spec() -> ImmutableSpec {
    ImmutableSpec {
        description: "C4 oracle fixture task".to_string(),
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

fn git_out(dir: &Path, home: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=f@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn has_branch(repo: &Path, home: &Path, short: &str) -> bool {
    !git_out(repo, home, &["branch", "--list", short]).is_empty()
}

fn has_ref(repo: &Path, home: &Path, full_ref: &str) -> bool {
    !git_out(
        repo,
        home,
        &["for-each-ref", "--format=%(refname)", full_ref],
    )
    .is_empty()
}

fn reaped_child_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

fn set_mtime_past(path: &Path, age: Duration) {
    let file = File::options().write(true).open(path).unwrap();
    let target = SystemTime::now().checked_sub(age).unwrap();
    file.set_modified(target).unwrap();
}

#[test]
fn c4_gc_from_linked_worktree_subdir_cleans_other_done_and_protects_cwd() {
    let sb = Sandbox::new();
    let worktrees_dir = sb.home.join(DEFAULT_WORKTREES_REL);
    fs::create_dir_all(&sb.data).unwrap();
    let db = Db::open(&sb.data.join("aaf.db")).unwrap();
    insert_task(&db, &sb.repo, "t-cwd-active", TaskState::Done);
    insert_task(&db, &sb.repo, "t-done-other", TaskState::Done);
    drop(db);

    let wt_active = worktree::create(&sb.repo, &worktrees_dir, "t-cwd-active", "main").unwrap();
    let subdir = wt_active.path.join("subdir");
    fs::create_dir_all(&subdir).unwrap();
    let wt_other = worktree::create(&sb.repo, &worktrees_dir, "t-done-other", "main").unwrap();
    fs::write(wt_other.path.join("other.txt"), "done\n").unwrap();
    git_out(&wt_other.path, &sb.home, &["add", "-A"]);
    git_out(&wt_other.path, &sb.home, &["commit", "-q", "-m", "other"]);
    let other_head = git_out(&wt_other.path, &sb.home, &["rev-parse", "HEAD"]);
    git_out(
        &sb.repo,
        &sb.home,
        &[
            "update-ref",
            "refs/aaf/snapshots/t-done-other/s1",
            &other_head,
        ],
    );

    let out = sb
        .command(&["gc", "--force", "--json"])
        .current_dir(&subdir)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let body = one_object(&out);
    assert_eq!(body["ok"], json!(true), "{body}");
    assert_eq!(body["status"], json!("ok"), "{body}");

    assert!(
        !wt_other.path.exists(),
        "other Done worktree must be removed when running gc from a linked worktree"
    );
    assert!(
        !has_branch(&sb.repo, &sb.home, "aaf/t-done-other"),
        "other Done branch must be removed"
    );
    assert!(
        !has_ref(&sb.repo, &sb.home, "refs/aaf/snapshots/t-done-other/s1"),
        "other Done snapshots must be removed"
    );

    assert!(
        subdir.is_dir(),
        "worktree containing caller cwd must be kept even with --force"
    );
    assert!(
        has_branch(&sb.repo, &sb.home, "aaf/t-cwd-active"),
        "branch of worktree containing caller cwd must be kept"
    );
    let warnings = body["warnings"].as_array().unwrap();
    assert!(
        warnings.iter().any(|w| {
            let s = w.as_str().unwrap_or("");
            s.contains("t-cwd-active") && s.contains("cwd")
        }),
        "expected cwd warning for t-cwd-active, got {warnings:?}"
    );
}

#[test]
fn c4_gc_warns_on_main_repo_done_task_and_still_cleans_other_done_task() {
    let sb = Sandbox::new();
    let worktrees_dir = sb.home.join(DEFAULT_WORKTREES_REL);
    fs::create_dir_all(&sb.data).unwrap();
    let db = Db::open(&sb.data.join("aaf.db")).unwrap();
    insert_task(&db, &sb.repo, "t-main-checked", TaskState::Done);
    insert_task(&db, &sb.repo, "t-done-clean", TaskState::Done);
    drop(db);

    let wt_clean = worktree::create(&sb.repo, &worktrees_dir, "t-done-clean", "main").unwrap();
    let head = git_out(&wt_clean.path, &sb.home, &["rev-parse", "HEAD"]);
    git_out(
        &sb.repo,
        &sb.home,
        &["update-ref", "refs/aaf/snapshots/t-done-clean/s1", &head],
    );
    // Check out task #1's branch directly in the main worktree.
    git_out(
        &sb.repo,
        &sb.home,
        &["checkout", "-q", "-b", "aaf/t-main-checked"],
    );

    let (code, body) = sb.json(&["gc"]);
    assert_eq!(code, 0, "{body}");
    assert_eq!(body["ok"], json!(true), "{body}");
    assert_eq!(body["status"], json!("ok"), "{body}");

    let warnings = body["warnings"].as_array().unwrap();
    assert!(
        warnings.iter().any(|w| {
            let s = w.as_str().unwrap_or("");
            s.starts_with("t-main-checked:") && s.contains("main worktree")
        }),
        "expected main worktree warning, got {warnings:?}"
    );
    assert!(
        has_branch(&sb.repo, &sb.home, "aaf/t-main-checked"),
        "branch checked out in main repo must stay"
    );
    assert!(
        !wt_clean.path.exists(),
        "clean Done task #2 worktree must still be cleaned"
    );
    assert!(
        !has_branch(&sb.repo, &sb.home, "aaf/t-done-clean"),
        "clean Done task #2 branch must still be cleaned"
    );
    assert!(
        !has_ref(&sb.repo, &sb.home, "refs/aaf/snapshots/t-done-clean/s1"),
        "clean Done task #2 snapshots must still be cleaned"
    );
}

#[test]
fn c4_gc_cleans_dead_pid_scratch_and_refs_while_preserving_live_pid() {
    let sb = Sandbox::new();
    let dead = reaped_child_pid();
    let live = std::process::id();
    let common_id = stats_repo_id(&sb.repo);

    let merge_scratch = sb.data.join("merge-scratch");
    fs::create_dir_all(&merge_scratch).unwrap();
    let dead_ms = merge_scratch.join(format!("{common_id}-pr7-{dead}-0"));
    let live_ms = merge_scratch.join(format!("{common_id}-pr8-{live}-0"));
    let oracle_parent = sb.repo.parent().unwrap().join("tmp-oracle");
    fs::create_dir_all(&oracle_parent).unwrap();
    let dead_oracle = oracle_parent.join(format!("aaf-oracle-wt-{dead}-1-123456"));
    let live_oracle = oracle_parent.join(format!("aaf-oracle-wt-{live}-2-123456"));

    for wt in [&dead_ms, &live_ms, &dead_oracle, &live_oracle] {
        git_out(
            &sb.repo,
            &sb.home,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                wt.to_str().unwrap(),
                "HEAD",
            ],
        );
    }

    let head = git_out(&sb.repo, &sb.home, &["rev-parse", "HEAD"]);
    let dead_ref = format!("refs/aaf-merge/pr-7-{dead}-0");
    let live_ref = format!("refs/aaf-merge/pr-8-{live}-0");
    let legacy_old_ref = "refs/aaf-merge/pr-9";
    for r in [&dead_ref, &live_ref, legacy_old_ref] {
        git_out(&sb.repo, &sb.home, &["update-ref", r, &head]);
    }
    let loose_legacy = sb.repo.join(".git").join(legacy_old_ref);
    set_mtime_past(&loose_legacy, Duration::from_secs(7200));

    let merge_lock_path = sb
        .data
        .join("locks")
        .join(format!("{common_id}.merge.lock"));
    let (dry_code, dry_body) = sb.json(&["gc", "--dry-run"]);
    assert_eq!(dry_code, 0, "{dry_body}");
    assert!(
        !merge_lock_path.exists(),
        "--dry-run must NEVER create .merge.lock"
    );
    assert!(dead_ms.is_dir() && dead_oracle.is_dir());
    assert!(has_ref(&sb.repo, &sb.home, &dead_ref));
    assert!(has_ref(&sb.repo, &sb.home, legacy_old_ref));

    let (code, body) = sb.json(&["gc"]);
    assert_eq!(code, 0, "{body}");
    assert_eq!(body["ok"], json!(true), "{body}");
    assert!(!dead_ms.exists(), "dead-PID merge-scratch must be removed");
    assert!(
        !dead_oracle.exists(),
        "dead-PID aaf-oracle-wt must be removed"
    );
    assert!(
        !has_ref(&sb.repo, &sb.home, &dead_ref),
        "dead-PID refs/aaf-merge must be removed"
    );
    assert!(
        !has_ref(&sb.repo, &sb.home, legacy_old_ref),
        "old unlocked legacy refs/aaf-merge must be removed"
    );

    assert!(live_ms.is_dir(), "live-PID merge-scratch must be kept");
    assert!(live_oracle.is_dir(), "live-PID aaf-oracle-wt must be kept");
    assert!(
        has_ref(&sb.repo, &sb.home, &live_ref),
        "live-PID refs/aaf-merge must be kept"
    );

    for wt in [&live_ms, &live_oracle] {
        git_out(
            &sb.repo,
            &sb.home,
            &["worktree", "remove", "--force", wt.to_str().unwrap()],
        );
    }
}

#[test]
fn c4_startup_sweep_under_run_lock_cleans_done_squash_task_without_deadlock() {
    let sb = Sandbox::new();
    let worktrees_dir = sb.home.join(DEFAULT_WORKTREES_REL);
    fs::create_dir_all(&sb.data).unwrap();
    let db = Db::open(&sb.data.join("aaf.db")).unwrap();
    insert_task(&db, &sb.repo, "t-done-squash", TaskState::Done);
    drop(db);

    // Create Done task worktree with a commit NOT an ancestor of main (squash-merged).
    let wt_done = worktree::create(&sb.repo, &worktrees_dir, "t-done-squash", "main").unwrap();
    fs::write(wt_done.path.join("feature.txt"), "squash feature\n").unwrap();
    git_out(&wt_done.path, &sb.home, &["add", "-A"]);
    git_out(
        &wt_done.path,
        &sb.home,
        &["commit", "-q", "-m", "task commit"],
    );
    let task_head = git_out(&wt_done.path, &sb.home, &["rev-parse", "HEAD"]);
    git_out(
        &sb.repo,
        &sb.home,
        &[
            "update-ref",
            "refs/aaf/snapshots/t-done-squash/s1",
            &task_head,
        ],
    );

    // Simulate squash-merge on main with a distinct commit SHA.
    fs::write(sb.repo.join("feature.txt"), "squash feature\n").unwrap();
    git_out(&sb.repo, &sb.home, &["add", "-A"]);
    git_out(
        &sb.repo,
        &sb.home,
        &["commit", "-q", "-m", "squash PR (#10)"],
    );

    // Also stage a dirty dead-PID batch and a clean live-PID batch.
    let dead_batch_id = format!("batch-{}-abcd1234", reaped_child_pid());
    let live_batch_id = format!("batch-{}-ef567890", std::process::id());
    let wt_dead_batch = worktree::create(&sb.repo, &worktrees_dir, &dead_batch_id, "main").unwrap();
    fs::write(wt_dead_batch.path.join("dirty.tmp"), "junk\n").unwrap();
    let wt_live_batch = worktree::create(&sb.repo, &worktrees_dir, &live_batch_id, "main").unwrap();

    // Run `aaf run --unsandboxed --json` (holds `.run.lock`, 0 Ready tasks).
    let (code, body) = sb.json(&["run", "--unsandboxed"]);
    assert_eq!(code, 0, "{body}");
    assert_eq!(body["ok"], json!(true), "{body}");
    assert_eq!(body["status"], json!("ok"), "{body}");

    assert!(
        !wt_done.path.exists(),
        "startup_sweep must remove clean Done task worktree under .run.lock"
    );
    assert!(
        !has_branch(&sb.repo, &sb.home, "aaf/t-done-squash"),
        "startup_sweep must remove squash-merged Done task branch"
    );
    assert!(
        !has_ref(&sb.repo, &sb.home, "refs/aaf/snapshots/t-done-squash/s1"),
        "startup_sweep must remove Done task snapshot refs"
    );
    assert!(
        !wt_dead_batch.path.exists()
            && !has_branch(&sb.repo, &sb.home, &format!("aaf/{dead_batch_id}")),
        "startup_sweep must force-remove dirty dead-PID batch"
    );
    assert!(
        wt_live_batch.path.is_dir()
            && has_branch(&sb.repo, &sb.home, &format!("aaf/{live_batch_id}")),
        "startup_sweep must keep live-PID batch"
    );

    git_out(
        &sb.repo,
        &sb.home,
        &[
            "worktree",
            "remove",
            "--force",
            wt_live_batch.path.to_str().unwrap(),
        ],
    );
}

#[test]
fn c4_gc_review_and_orphan_sweep_cas_done_guards_zero_commit_and_never_forces_dirty() {
    let sb = Sandbox::new();
    let worktrees_dir = sb.home.join(DEFAULT_WORKTREES_REL);
    fs::create_dir_all(&sb.data).unwrap();
    let db = Db::open(&sb.data.join("aaf.db")).unwrap();
    insert_task(&db, &sb.repo, "t-rev-merged", TaskState::Review);
    drop(db);

    // 1. Review task with a real commit merged into main.
    let wt_rev = worktree::create(&sb.repo, &worktrees_dir, "t-rev-merged", "main").unwrap();
    fs::write(wt_rev.path.join("rev.txt"), "reviewed\n").unwrap();
    git_out(&wt_rev.path, &sb.home, &["add", "-A"]);
    git_out(
        &wt_rev.path,
        &sb.home,
        &["commit", "-q", "-m", "rev commit"],
    );
    let rev_head = git_out(&wt_rev.path, &sb.home, &["rev-parse", "HEAD"]);
    git_out(
        &sb.repo,
        &sb.home,
        &[
            "update-ref",
            "refs/aaf/snapshots/t-rev-merged/s1",
            &rev_head,
        ],
    );
    git_out(
        &sb.repo,
        &sb.home,
        &[
            "merge",
            "-q",
            "--no-ff",
            "aaf/t-rev-merged",
            "-m",
            "merge rev",
        ],
    );

    // 2. Orphan aaf/* with a real merged commit + clean worktree -> should be cleaned.
    let wt_orphan_clean =
        worktree::create(&sb.repo, &worktrees_dir, "orphan-merged", "main").unwrap();
    fs::write(wt_orphan_clean.path.join("o1.txt"), "o1\n").unwrap();
    git_out(&wt_orphan_clean.path, &sb.home, &["add", "-A"]);
    git_out(
        &wt_orphan_clean.path,
        &sb.home,
        &["commit", "-q", "-m", "orphan merged"],
    );
    git_out(
        &sb.repo,
        &sb.home,
        &[
            "merge",
            "-q",
            "--no-ff",
            "aaf/orphan-merged",
            "-m",
            "merge orphan",
        ],
    );

    // 3. Orphan aaf/* with a real merged commit + DIRTY worktree -> must be kept even with --force.
    let wt_orphan_dirty =
        worktree::create(&sb.repo, &worktrees_dir, "orphan-dirty", "main").unwrap();
    fs::write(wt_orphan_dirty.path.join("o2.txt"), "o2\n").unwrap();
    git_out(&wt_orphan_dirty.path, &sb.home, &["add", "-A"]);
    git_out(
        &wt_orphan_dirty.path,
        &sb.home,
        &["commit", "-q", "-m", "orphan dirty"],
    );
    git_out(
        &sb.repo,
        &sb.home,
        &[
            "merge",
            "-q",
            "--no-ff",
            "aaf/orphan-dirty",
            "-m",
            "merge orphan dirty",
        ],
    );
    fs::write(wt_orphan_dirty.path.join("unsaved.txt"), "keep\n").unwrap();

    // 4. Orphan aaf/* sitting at main (0 commits ahead) -> must be kept by 0-commit guard.
    let wt_orphan_zero = worktree::create(&sb.repo, &worktrees_dir, "orphan-zero", "main").unwrap();

    // Dry-run must NOT mutate DB or disk.
    let (dry_code, dry_body) = sb.json(&["gc", "--dry-run"]);
    assert_eq!(dry_code, 0, "{dry_body}");
    let db_check = Db::open_read_only(&sb.data.join("aaf.db")).unwrap();
    assert_eq!(
        db_check.get_task("t-rev-merged").unwrap().state,
        TaskState::Review
    );
    drop(db_check);
    assert!(wt_rev.path.is_dir() && wt_orphan_clean.path.is_dir());

    // Real gc with --force: completes Review -> Done, removes clean orphan, keeps dirty & 0-commit.
    let (code, body) = sb.json(&["gc", "--force"]);
    assert_eq!(code, 0, "{body}");
    assert_eq!(body["ok"], json!(true), "{body}");
    assert_eq!(body["completed_review_tasks"], json!(["t-rev-merged"]));

    let db_after = Db::open_read_only(&sb.data.join("aaf.db")).unwrap();
    assert_eq!(
        db_after.get_task("t-rev-merged").unwrap().state,
        TaskState::Done
    );
    drop(db_after);
    assert!(!wt_rev.path.exists() && !has_branch(&sb.repo, &sb.home, "aaf/t-rev-merged"));
    assert!(!has_ref(
        &sb.repo,
        &sb.home,
        "refs/aaf/snapshots/t-rev-merged/s1"
    ));
    assert!(
        !wt_orphan_clean.path.exists() && !has_branch(&sb.repo, &sb.home, "aaf/orphan-merged"),
        "clean merged orphan must be removed"
    );
    assert!(
        wt_orphan_dirty.path.join("unsaved.txt").is_file()
            && has_branch(&sb.repo, &sb.home, "aaf/orphan-dirty"),
        "dirty orphan worktree and branch must stay even under --force"
    );
    assert!(
        wt_orphan_zero.path.is_dir() && has_branch(&sb.repo, &sb.home, "aaf/orphan-zero"),
        "0-commit orphan sitting at base_oid must stay"
    );
}
