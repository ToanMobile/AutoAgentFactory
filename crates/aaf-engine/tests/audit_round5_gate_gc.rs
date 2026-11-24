#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for audit round 5 gate, docs_fast, head_lock, and gc fixes.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use aaf_domain::config::{GateConfig, MachineConfig, RepoConfig};
use aaf_domain::gate::{ChangeKind, GateMode, GateStatus, LockStatus};
use aaf_domain::ports::Clock;
use aaf_engine::db::private;
use aaf_engine::gate::changes::Change;
use aaf_engine::gate::{GateError, HiddenPolicy, docs_fast, head_lock, run_gate_policy};
use aaf_engine::gc::{GcOptions, run_gc};
use aaf_engine::throttle::{SystemProbe, Throttle};
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(30);

struct FixedClock(i64);
impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn repo_with(files: &[(&str, String)]) -> TempDir {
    let dir = TempDir::new().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    for (path, body) in files {
        let full = dir.path().join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, body).unwrap();
    }
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    dir
}

fn in_repo(dir: &Path, path: &str) -> bool {
    let globs = GateConfig::default().head_lock_globs;
    let change = Change {
        kind: ChangeKind::Modified,
        path: path.to_string(),
        old_path: None,
    };
    docs_fast::is_docs_only_in_repo(dir, &[change], &globs, &[], TIMEOUT).unwrap()
}

#[test]
fn docs_fast_go_embed_wildcards_escapes_and_benign_cases() {
    let marker = concat!("//go:", "embed");
    let rows = [
        // Wildcard / glob patterns that MUST trigger full gate (`false`).
        (format!("{marker} README.*\n"), "README.md", false),
        (format!("{marker} doc*\n"), "docs/g.md", false),
        (format!("{marker} all:doc*\n"), "docs/g.md", false),
        (format!("{marker} docs?\n"), "docs1/g.md", false),
        (format!("{marker} doc?\n"), "docs/g.md", false),
        (format!("{marker} [d]ocs\n"), "docs/g.md", false),
        (format!("{marker} ?*\n"), "docs/g.md", false),
        (format!("{marker} *.m?\n"), "docs/g.md", false),
        // Double-quoted backslash escape and malformed glob MUST fail closed (`false`).
        (format!("{marker} \"docs\\x2fg.md\"\n"), "docs/g.md", false),
        (format!("{marker} [unclosed\n"), "docs/g.md", false),
        // Unrelated embed targets MUST remain fast (`true`) for `docs/g.md`.
        (format!("{marker} web\n"), "docs/g.md", true),
        (format!("{marker} mydocs\n"), "docs/g.md", true),
        (format!("{marker} web/*\n"), "docs/g.md", true),
    ];
    for (directive, doc, expected_fast) in rows {
        let repo = repo_with(&[("embed.go", format!("package p\n{directive}var f string\n"))]);
        assert_eq!(
            in_repo(repo.path(), doc),
            expected_fast,
            "directive={directive:?}, doc={doc}"
        );
    }
}

#[test]
fn docs_fast_include_dir_parent_traversal_blocks_while_sibling_stays_fast() {
    let inc_dir = concat!("include_", "dir!");
    let rows = [
        (format!("static D: Dir = {inc_dir}(\"..\");\n"), false),
        (format!("static D: Dir = {inc_dir}(\"../\");\n"), false),
        (format!("static D: Dir = {inc_dir}(\"../..\");\n"), false),
        (format!("static D: Dir = {inc_dir}(\"../../\");\n"), false),
        (format!("static D: Dir = {inc_dir}(\"../web\");\n"), true),
    ];
    for (body, expected_fast) in rows {
        let repo = repo_with(&[("src/lib.rs", body.clone())]);
        assert_eq!(
            in_repo(repo.path(), "docs/g.md"),
            expected_fast,
            "body={body:?}"
        );
    }
}

#[test]
fn head_lock_approve_expecting_rejects_stale_fp_and_approves_exact_match() {
    let dir = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("CLAUDE.md"), "# initial\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "init"]);

    git(root, &["checkout", "-q", "-b", "feat"]);
    fs::write(root.join("CLAUDE.md"), "# preview version\n").unwrap();

    let mut cfg = RepoConfig::default();
    cfg.gate.base_ref = "main".to_string();

    let preview = head_lock::evaluate(data.path(), root, &cfg, "main");
    assert_eq!(preview.status, LockStatus::Blocked);
    assert!(!preview.lock_fp.is_empty());

    // Mutate the locked file after preview to simulate a TOCTOU race.
    fs::write(root.join("CLAUDE.md"), "# tampered after preview\n").unwrap();

    let base = aaf_engine::gate::base::resolve(root, &cfg.gate.base_ref, TIMEOUT).unwrap();
    let approve = |expected: &str, clock: &FixedClock| {
        head_lock::approve_expecting(
            data.path(),
            root,
            &cfg,
            &base,
            ("alice", false),
            clock,
            expected,
        )
    };
    let err = approve(&preview.lock_fp, &FixedClock(100)).unwrap_err();
    assert!(
        matches!(err, GateError::LockFpChanged),
        "expected LockFpChanged, got {err:?}"
    );

    // No approval may have been written on mismatch, and the current
    // fingerprint is a different one than the stale preview.
    let still_blocked = head_lock::evaluate(data.path(), root, &cfg, "main");
    assert_eq!(still_blocked.status, LockStatus::Blocked);
    let actual_fp = still_blocked.lock_fp;
    assert_ne!(actual_fp, preview.lock_fp);

    // Approving with the current fingerprint succeeds and unlocks head_lock.
    let approval = approve(&actual_fp, &FixedClock(101)).unwrap();
    assert_eq!(approval.lock_fp, actual_fp);
    assert_eq!(approval.reviewer, "alice");
    assert!(!approval.same_account);

    let after = head_lock::evaluate(data.path(), root, &cfg, "main");
    assert_eq!(after.status, LockStatus::Approved);
}

fn reaped_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    child.wait().unwrap();
    child.id()
}

#[test]
fn gc_preserves_live_locked_merge_scratch_and_cleans_crashed_unlocked_scratch() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    let worktrees = tmp.path().join("worktrees");
    let scratch = data.join("merge-scratch");
    fs::create_dir_all(&scratch).unwrap();

    // 1. Live scratch worktree holding an exclusive `<dir>.lock` (even with max_age = 0).
    let live_dir = scratch.join("repoLive-pr1-11111-0");
    let live_lock = scratch.join("repoLive-pr1-11111-0.lock");
    fs::create_dir_all(&live_dir).unwrap();
    let live_guard = private::open_file(&live_lock).unwrap();
    live_guard.try_lock().unwrap();

    // 2. Crashed scratch worktree with an unlocked `<dir>.lock` (fresh mtime).
    // Owner pids are reaped children: a pid that names a live process keeps
    // its scratch (gc owner-pid guard).
    let dead = reaped_pid();
    let crashed_dir = scratch.join(format!("repoDead-pr2-{dead}-0"));
    let crashed_lock = scratch.join(format!("repoDead-pr2-{dead}-0.lock"));
    fs::create_dir_all(&crashed_dir).unwrap();
    let _ = private::open_file(&crashed_lock).unwrap();

    // 3. Legacy scratch dir with NO `.lock` file and fresh mtime.
    let legacy_dir = scratch.join(format!("repoOld-pr3-{}-0", reaped_pid()));
    fs::create_dir_all(&legacy_dir).unwrap();

    // Dry run must leave everything on disk while reporting the crashed dir.
    let dry = run_gc(
        &data,
        &worktrees,
        &GcOptions {
            dry_run: true,
            ..GcOptions::default()
        },
    )
    .unwrap();
    assert!(live_dir.is_dir());
    assert!(crashed_dir.is_dir());
    assert!(crashed_lock.is_file());
    assert!(legacy_dir.is_dir());
    assert!(
        dry.removed_files
            .iter()
            .any(|p| p == &crashed_dir.display().to_string()),
        "{:?}",
        dry.removed_files
    );

    // Default retention (14 days): keeps live dir & fresh legacy dir, removes crashed dir + lock.
    let report = run_gc(&data, &worktrees, &GcOptions::default()).unwrap();
    assert!(live_dir.is_dir(), "live locked scratch must NOT be deleted");
    assert!(live_lock.is_file(), "live lock must NOT be deleted");
    assert!(
        !crashed_dir.exists(),
        "crashed unlocked scratch must be removed"
    );
    assert!(
        !crashed_lock.exists(),
        "crashed companion lock must be removed"
    );
    assert!(
        legacy_dir.is_dir(),
        "fresh legacy dir without .lock must wait for max_age"
    );
    assert_eq!(
        report.removed_files,
        vec![crashed_dir.display().to_string()]
    );

    // Even with `older_than_days = 0` (`max_age = 0`), the live locked scratch stays intact,
    // while the expired legacy dir is cleaned up.
    let zero_age = run_gc(
        &data,
        &worktrees,
        &GcOptions {
            older_than_days: 0,
            ..GcOptions::default()
        },
    )
    .unwrap();
    assert!(
        live_dir.is_dir(),
        "live locked scratch must NOT be deleted even when max_age == 0"
    );
    assert!(live_lock.is_file());
    assert!(!legacy_dir.exists(), "expired legacy dir must be removed");
    assert_eq!(
        zero_age.removed_files,
        vec![legacy_dir.display().to_string()]
    );

    drop(live_guard);
}

#[test]
fn gate_parallel_preflight_works_for_warn_and_remove_policies() {
    let dir = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("README.md"), "# hi\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "init"]);

    fs::write(root.join(".git/info/exclude"), "planted.tmp\n").unwrap();
    fs::write(root.join("planted.tmp"), "hidden\n").unwrap();

    let mut cfg = RepoConfig::default();
    cfg.commands.lint = "true".to_string();
    cfg.commands.test_scoped = "true".to_string();
    let probe = SystemProbe::detect();
    let noop = |_: &aaf_engine::throttle::Progress<'_>| {};
    let machine = MachineConfig::default();
    let throttle = Throttle::system(&machine, &probe, &noop);

    let warned = run_gate_policy(
        data.path(),
        root,
        &cfg,
        GateMode::Scoped,
        &FixedClock(200),
        &throttle,
        HiddenPolicy::Warn,
    )
    .unwrap();
    assert_eq!(warned.receipt.status, GateStatus::Clear);
    assert!(root.join("planted.tmp").is_file());

    let removed = run_gate_policy(
        data.path(),
        root,
        &cfg,
        GateMode::Scoped,
        &FixedClock(201),
        &throttle,
        HiddenPolicy::Remove,
    )
    .unwrap();
    assert_eq!(removed.receipt.status, GateStatus::Clear);
    assert!(!root.join("planted.tmp").exists());
}
