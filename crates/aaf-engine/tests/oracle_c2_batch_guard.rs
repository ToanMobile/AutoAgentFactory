#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Oracle tests for Gói C2: RAII `IntegrationGuard`, `batch-<pid>-<8hex>`
//! naming, `WorktreeEntry.locked`, `active_in_progress_branches`, fail-closed
//! `process_alive`, and safe dirty-batch sweeping.

#[path = "git_support.rs"]
mod support;

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::process::Command;

use aaf_domain::gate::{HeadLockReport, LockStatus, Receipt};
use aaf_domain::ports::PortResult;
use aaf_engine::factory::batch::{Member, gate_batch};
use aaf_engine::factory::{GateRunner, GitOps, IntegrationGuard, RealGit, resolve_base};
use aaf_engine::gc::{GcOptions, run_gc};
use aaf_engine::git::worktree;
use aaf_engine::process::process_alive;
use support::{Fixture, commit_file, git};

struct PanickingGate;

impl GateRunner for PanickingGate {
    fn run(&self, _tree: &Path) -> PortResult<Receipt> {
        panic!("simulated gate panic inside batch");
    }

    fn head_lock(&self, _tree: &Path) -> PortResult<HeadLockReport> {
        Ok(HeadLockReport {
            status: LockStatus::Clear,
            lock_fp: String::new(),
            locked_changes: Vec::new(),
            approval: None,
        })
    }
}

fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

fn batch_dirs(fx: &Fixture) -> Vec<String> {
    let dir = fx.worktrees.join(aaf_fp::repo_id(&fx.repo));
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| name.starts_with("batch-"))
        .collect()
}

fn batch_branches(repo: &Path) -> Vec<String> {
    git(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads/aaf/batch-*",
        ],
    )
    .lines()
    .filter(|l| !l.is_empty())
    .map(str::to_string)
    .collect()
}

#[test]
fn integration_guard_cleans_up_on_gate_panic_and_integrate_error_and_tags_pid() {
    let fx = Fixture::new();
    let base = resolve_base(&fx.repo, "main").unwrap();
    let real = RealGit::new(
        fx.repo.clone(),
        fx.worktrees.clone(),
        fx.locks.clone(),
        base,
    );

    let wt1 = real.task_worktree("t1").unwrap();
    commit_file(&wt1.path, "a.txt", "a\n", "commit t1");
    let wt2 = real.task_worktree("t2").unwrap();
    commit_file(&wt2.path, "b.txt", "b\n", "commit t2");

    // 1. Panic inside `GateRunner::run` during `factory::batch::gate_batch`:
    //    `IntegrationGuard` must unwind-discard the batch worktree and branch.
    let members = vec![
        Member {
            task_id: "t1".to_string(),
            worktree: wt1,
        },
        Member {
            task_id: "t2".to_string(),
            worktree: wt2,
        },
    ];
    let panicked = catch_unwind(AssertUnwindSafe(|| {
        let _ = gate_batch(&real, &PanickingGate, &members);
    }));
    assert!(panicked.is_err(), "PanickingGate must trigger unwind");
    assert!(
        batch_dirs(&fx).is_empty(),
        "batch dir leaked after gate panic: {:?}",
        batch_dirs(&fx)
    );
    assert!(
        batch_branches(&fx.repo).is_empty(),
        "batch branch leaked after gate panic: {:?}",
        batch_branches(&fx.repo)
    );

    // 2. Error partway through `RealGit::integrate` (invalid task id "../bad"):
    //    guard constructed after releasing `_admin` must clean up without deadlock.
    let err = real
        .integrate(&["t1".to_string(), "../bad".to_string()])
        .unwrap_err();
    assert!(err.to_string().contains("invalid task id"), "{err}");
    assert!(
        batch_dirs(&fx).is_empty(),
        "batch dir leaked after integrate error: {:?}",
        batch_dirs(&fx)
    );
    assert!(
        batch_branches(&fx.repo).is_empty(),
        "batch branch leaked after integrate error: {:?}",
        batch_branches(&fx.repo)
    );

    // 3. `RealGit::integrate(&[])` creates `aaf/batch-<own_pid>-<8hex>` and
    //    dropping `IntegrationGuard::from_existing` cleans it up automatically.
    let own_pid = std::process::id();
    let expected_prefix = format!("aaf/batch-{own_pid}-");
    let integration = real.integrate(&[]).unwrap();
    let branch = integration.tree.branch.clone();
    assert!(
        branch.starts_with(&expected_prefix),
        "expected prefix {expected_prefix:?}, got {branch:?}"
    );
    let hex_suffix = branch.strip_prefix(&expected_prefix).unwrap();
    assert_eq!(
        hex_suffix.len(),
        8,
        "expected 8 hex chars in {branch:?}, got {hex_suffix:?}"
    );
    assert!(
        hex_suffix.bytes().all(|b| b.is_ascii_hexdigit()),
        "non-hex suffix in {branch:?}"
    );
    let batch_id = branch.strip_prefix("aaf/").unwrap();
    assert_eq!(worktree::parse_batch_pid(batch_id), Some(own_pid));

    let guard = IntegrationGuard::from_existing(&real, integration);
    drop(guard);
    assert!(batch_dirs(&fx).is_empty(), "{:?}", batch_dirs(&fx));
    assert!(
        batch_branches(&fx.repo).is_empty(),
        "{:?}",
        batch_branches(&fx.repo)
    );
}

#[test]
fn gc_force_removes_dirty_dead_pid_batch_and_protects_live_and_locked_batches() {
    let fx = Fixture::new();
    let data_dir = fx.repo.parent().unwrap().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let dead = dead_pid();
    let own = std::process::id();
    assert!(!process_alive(dead), "reaped child pid must be dead");
    assert!(process_alive(own), "own pid must be alive");
    assert!(process_alive(0), "pid 0 must fail-closed alive");
    assert!(process_alive(1), "pid 1 must fail-closed alive");

    let dead_dirty_id = format!("batch-{dead}-abcd1234");
    let live_clean_id = format!("batch-{own}-abcd1234");
    let dead_locked_id = format!("batch-{dead}-deadbeef");
    let dead_detached_locked_id = format!("batch-{dead}-cafe0123");

    // Dirty dead-PID batch (including a read-only file inside a read-only dir).
    let wt_dirty = worktree::create(&fx.repo, &fx.worktrees, &dead_dirty_id, "main").unwrap();
    let ro_dir = wt_dirty.path.join("ro_dir");
    fs::create_dir_all(&ro_dir).unwrap();
    let ro_file = ro_dir.join("dirty.txt");
    fs::write(&ro_file, "uncommitted dirty batch\n").unwrap();
    fs::set_permissions(&ro_file, fs::Permissions::from_mode(0o400)).unwrap();
    fs::set_permissions(&ro_dir, fs::Permissions::from_mode(0o500)).unwrap();

    // Clean live-PID batch (must survive even `force: true`).
    let wt_live = worktree::create(&fx.repo, &fx.worktrees, &live_clean_id, "main").unwrap();

    // Locked dead-PID batch (must survive even `force: true`).
    let wt_locked = worktree::create(&fx.repo, &fx.worktrees, &dead_locked_id, "main").unwrap();
    git(
        &fx.repo,
        &["worktree", "lock", wt_locked.path.to_str().unwrap()],
    );

    // Detached locked dead-PID batch with a reason string (tests both
    // `locked <reason>` parsing and canonical-path matching for detached trees).
    let wt_detached =
        worktree::create(&fx.repo, &fx.worktrees, &dead_detached_locked_id, "main").unwrap();
    git(&wt_detached.path, &["checkout", "--detach", "-q"]);
    git(
        &fx.repo,
        &[
            "worktree",
            "lock",
            "--reason",
            "in use",
            wt_detached.path.to_str().unwrap(),
        ],
    );

    // Verify `worktree::list` parses both bare `locked` and `locked <reason>`.
    let listed = worktree::list(&fx.repo).unwrap();
    let canon_locked = fs::canonicalize(&wt_locked.path).unwrap();
    let canon_detached = fs::canonicalize(&wt_detached.path).unwrap();
    assert!(
        listed
            .iter()
            .any(|e| { fs::canonicalize(&e.path).ok() == Some(canon_locked.clone()) && e.locked })
    );
    assert!(listed.iter().any(|e| {
        fs::canonicalize(&e.path).ok() == Some(canon_detached.clone())
            && e.branch.is_none()
            && e.locked
    }));

    let report = run_gc(
        &data_dir,
        &fx.worktrees,
        &GcOptions {
            repo_root: Some(fx.repo.clone()),
            force: true,
            ..GcOptions::default()
        },
    )
    .unwrap();

    // Dirty dead-PID batch is completely removed (worktree + branch).
    assert!(
        report.removed_orphan_batches.contains(&dead_dirty_id),
        "{report:?}"
    );
    assert!(!wt_dirty.path.exists(), "dirty dead-PID batch must be gone");
    assert_eq!(
        git(&fx.repo, &["branch", "--list", &wt_dirty.branch]),
        "",
        "dirty dead-PID batch branch must be deleted"
    );

    // Live-PID clean batch is untouched even under `force: true`.
    assert!(
        !report.removed_orphan_batches.contains(&live_clean_id),
        "{report:?}"
    );
    assert!(
        wt_live.path.is_dir(),
        "live-PID batch worktree must be kept"
    );
    assert_eq!(
        git(
            &fx.repo,
            &[
                "branch",
                "--list",
                "--format=%(refname:short)",
                &wt_live.branch
            ]
        ),
        wt_live.branch
    );

    // Both locked dead-PID batches are untouched even under `force: true`.
    assert!(
        !report.removed_orphan_batches.contains(&dead_locked_id),
        "{report:?}"
    );
    assert!(
        wt_locked.path.is_dir(),
        "locked dead-PID batch must be kept"
    );
    assert_eq!(
        git(
            &fx.repo,
            &[
                "branch",
                "--list",
                "--format=%(refname:short)",
                &wt_locked.branch
            ]
        ),
        wt_locked.branch
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.starts_with(&format!("{dead_locked_id}: worktree remove:"))),
        "{:?}",
        report.warnings
    );

    assert!(
        !report
            .removed_orphan_batches
            .contains(&dead_detached_locked_id),
        "{report:?}"
    );
    assert!(
        wt_detached.path.is_dir(),
        "detached locked dead-PID batch must be kept"
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.starts_with(&format!("{dead_detached_locked_id}: worktree remove:"))),
        "{:?}",
        report.warnings
    );
}

#[test]
fn cwd_guard_active_in_progress_branches_and_legacy_dirty_batches_fail_closed() {
    let fx = Fixture::new();
    let repo_id = aaf_fp::repo_id(&fx.repo);
    let dead = dead_pid();

    // 1. Legacy dirty batch: skipped when force=false, removed when force=true.
    let wt_legacy = worktree::create(&fx.repo, &fx.worktrees, "batch-legacy01", "main").unwrap();
    fs::write(wt_legacy.path.join("wip.txt"), "dirty\n").unwrap();

    // 2. Batch containing caller `cwd`: protected even when force=true.
    let cwd_batch_id = format!("batch-{dead}-11223344");
    let wt_cwd = worktree::create(&fx.repo, &fx.worktrees, &cwd_batch_id, "main").unwrap();
    let cwd_sub = wt_cwd.path.join("nested");
    fs::create_dir_all(&cwd_sub).unwrap();

    // 3. Worktree with an active `git rebase` (detached HEAD, rebase-merge/head-name).
    let rebase_id = format!("batch-{dead}-55667788");
    let wt_rebase = worktree::create(&fx.repo, &fx.worktrees, &rebase_id, "main").unwrap();
    commit_file(
        &wt_rebase.path,
        "conflict.txt",
        "branch-side\n",
        "branch edit",
    );
    commit_file(&fx.repo, "conflict.txt", "main-side\n", "main edit");
    let rebase_status = Command::new("git")
        .arg("-C")
        .arg(&wt_rebase.path)
        .args(["rebase", "main"])
        .output()
        .unwrap();
    assert!(
        !rebase_status.status.success(),
        "rebase should pause on conflict"
    );

    let active_ops = worktree::active_in_progress_branches(&fx.repo).unwrap();
    assert!(
        active_ops.contains(&wt_rebase.branch),
        "expected {} in active_in_progress_branches: {active_ops:?}",
        wt_rebase.branch
    );
    assert!(worktree::is_operation_in_progress(&wt_rebase.path).unwrap());

    // 4. Unregistered directory with broken `.git` fails closed when force=false.
    let broken_dir = fx.worktrees.join(&repo_id).join("batch-broken01");
    fs::create_dir_all(&broken_dir).unwrap();
    fs::write(broken_dir.join(".git"), "gitdir: /nonexistent/broken\n").unwrap();

    // Pass A: force=false, cwd=Some(cwd_sub) -> nothing removed (all 4 protected/dirty).
    let sweep_safe = worktree::remove_orphan_batch_worktrees_with(
        &fx.repo,
        &fx.worktrees,
        &repo_id,
        &HashSet::new(),
        false,
        false,
        Some(&cwd_sub),
    )
    .unwrap();
    assert!(sweep_safe.removed.is_empty(), "{sweep_safe:?}");
    assert!(wt_legacy.path.is_dir());
    assert!(wt_cwd.path.is_dir());
    assert!(wt_rebase.path.is_dir());
    assert!(broken_dir.is_dir());

    // Pass B: unresolvable cwd fails closed even when force=true.
    let missing_cwd = fx.repo.parent().unwrap().join("does-not-exist-cwd");
    let sweep_missing_cwd = worktree::remove_orphan_batch_worktrees_with(
        &fx.repo,
        &fx.worktrees,
        &repo_id,
        &HashSet::new(),
        false,
        true,
        Some(&missing_cwd),
    )
    .unwrap();
    assert!(
        sweep_missing_cwd.removed.is_empty(),
        "{sweep_missing_cwd:?}"
    );
    assert!(wt_legacy.path.is_dir());

    // Pass C: force=true with valid cwd_sub -> cleans legacy dirty + broken,
    // keeps cwd worktree and active-rebase worktree.
    let sweep_force = worktree::remove_orphan_batch_worktrees_with(
        &fx.repo,
        &fx.worktrees,
        &repo_id,
        &HashSet::new(),
        false,
        true,
        Some(&cwd_sub),
    )
    .unwrap();
    assert!(
        sweep_force.removed.contains(&"batch-legacy01".to_string()),
        "{sweep_force:?}"
    );
    assert!(
        sweep_force.removed.contains(&"batch-broken01".to_string()),
        "{sweep_force:?}"
    );
    assert!(!wt_legacy.path.exists());
    assert!(!broken_dir.exists());
    assert!(wt_cwd.path.is_dir(), "cwd worktree must remain");
    assert!(wt_rebase.path.is_dir(), "rebasing worktree must remain");
}
