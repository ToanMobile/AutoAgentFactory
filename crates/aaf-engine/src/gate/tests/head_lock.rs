use std::fs;

use aaf_domain::config::RepoConfig;
use aaf_domain::gate::{Approval, ChangeKind, HeadLockReport, LockStatus, LockedChange};

use super::super::GateError;
use super::super::head_lock::{approve, evaluate};
use super::support::{FixedClock, Repo};

fn report(repo: &Repo) -> HeadLockReport {
    evaluate(repo.data_dir(), repo.root(), &RepoConfig::default(), "main")
}

fn sign_off(repo: &Repo) -> Result<Approval, GateError> {
    approve(
        repo.data_dir(),
        repo.root(),
        &RepoConfig::default(),
        "reviewer-1",
        false,
        &FixedClock(42),
    )
}

#[test]
fn new_test_file_is_not_locked() {
    let repo = Repo::new();
    repo.write("tests/b_test.rs", "fn more() {}\n");
    repo.write("tests/c_test.rs", "fn committed() {}\n");
    repo.git(&["add", "tests/c_test.rs"]);
    repo.git(&["commit", "-q", "-m", "add test"]);
    repo.write("src/lib.rs", "pub fn one() -> u32 {\n    2\n}\n");
    let report = report(&repo);
    assert_eq!(report.status, LockStatus::Clear);
    assert!(report.lock_fp.is_empty());
    assert!(report.locked_changes.is_empty());
}

#[test]
fn edited_existing_test_file_is_locked() {
    let repo = Repo::new();
    repo.append("tests/a_test.rs", "// weakened\n");
    let report = report(&repo);
    assert_eq!(report.status, LockStatus::Blocked);
    assert_eq!(report.lock_fp.len(), 64);
    assert_eq!(
        report.locked_changes,
        vec![LockedChange {
            path: "tests/a_test.rs".to_string(),
            change: ChangeKind::Modified
        }]
    );
    assert!(report.approval.is_none());
}

#[test]
fn committed_edit_and_deletion_are_locked_too() {
    let repo = Repo::new();
    repo.append("tests/a_test.rs", "// weakened\n");
    repo.commit("edit test");
    assert_eq!(report(&repo).locked_changes[0].change, ChangeKind::Modified);

    let repo = Repo::new();
    fs::remove_file(repo.path("tests/a_test.rs")).unwrap();
    let report = report(&repo);
    assert_eq!(report.status, LockStatus::Blocked);
    assert_eq!(report.locked_changes[0].change, ChangeKind::Deleted);
}

#[test]
fn renamed_locked_file_counts_for_its_old_path() {
    let repo = Repo::new();
    repo.git(&["mv", "tests/a_test.rs", "src/moved.rs"]);
    let report = report(&repo);
    assert_eq!(report.status, LockStatus::Blocked);
    assert_eq!(
        report.locked_changes,
        vec![LockedChange {
            path: "tests/a_test.rs".to_string(),
            change: ChangeKind::Renamed
        }]
    );
}

#[test]
fn approval_unlocks_exactly_that_fingerprint() {
    let repo = Repo::new();
    repo.append("tests/a_test.rs", "// reviewed change\n");
    let before = report(&repo);
    assert_eq!(before.status, LockStatus::Blocked);

    let approval = sign_off(&repo).unwrap();
    assert_eq!(approval.lock_fp, before.lock_fp);
    assert_eq!(approval.verdict, "PASS");
    assert_eq!(approval.reviewer, "reviewer-1");
    assert_eq!(approval.created_at, 42);

    let after = report(&repo);
    assert_eq!(after.status, LockStatus::Approved);
    assert_eq!(after.lock_fp, before.lock_fp);
    assert_eq!(after.approval, Some(approval));

    // One more edit: a new fingerprint that nobody approved.
    repo.append("tests/a_test.rs", "// sneaked in afterwards\n");
    let edited = report(&repo);
    assert_eq!(edited.status, LockStatus::Blocked);
    assert_ne!(edited.lock_fp, before.lock_fp);
    assert!(edited.approval.is_none());
}

#[test]
fn approve_refuses_when_nothing_is_locked() {
    let repo = Repo::new();
    repo.write("src/new.rs", "pub fn two() {}\n");
    assert!(matches!(sign_off(&repo), Err(GateError::NothingLocked)));
}

#[test]
fn forged_or_mismatched_approval_does_not_unlock() {
    let repo = Repo::new();
    repo.append("tests/a_test.rs", "// weakened\n");
    // Create the real key, as a first gate run would.
    let key = aaf_fp::load_or_create_key(repo.data_dir()).unwrap();
    let lock_fp = report(&repo).lock_fp;
    let path = aaf_fp::approval_path(repo.data_dir(), repo.root(), &lock_fp);
    let verdict = |fp: &str, verdict: &str| Approval {
        lock_fp: fp.to_string(),
        reviewer: "mallory".to_string(),
        verdict: verdict.to_string(),
        same_account: false,
        created_at: 1,
    };
    fs::create_dir_all(path.parent().unwrap()).unwrap();

    // Signed with a key that is not the machine's key.
    let payload = serde_json::to_string(&verdict(&lock_fp, "PASS")).unwrap();
    let forged = aaf_fp::sign_tagged(&[7u8; 32], aaf_fp::APPROVAL_DOMAIN, payload.clone()).unwrap();
    fs::write(&path, serde_json::to_vec(&forged).unwrap()).unwrap();
    assert_eq!(report(&repo).status, LockStatus::Blocked);

    // Unsigned JSON.
    fs::write(&path, payload.as_bytes()).unwrap();
    assert_eq!(report(&repo).status, LockStatus::Blocked);

    // Properly signed, but for another fingerprint, copied into place.
    let other = serde_json::to_string(&verdict(&"0".repeat(64), "PASS")).unwrap();
    let moved = aaf_fp::sign_tagged(&key, aaf_fp::APPROVAL_DOMAIN, other).unwrap();
    fs::write(&path, serde_json::to_vec(&moved).unwrap()).unwrap();
    assert_eq!(report(&repo).status, LockStatus::Blocked);

    // Properly signed for this fingerprint, but not a pass.
    let fail = verdict(&lock_fp, "FAIL");
    aaf_fp::write_approval(repo.data_dir(), &key, repo.root(), &fail).unwrap();
    assert_eq!(report(&repo).status, LockStatus::Blocked);

    // Control: the same file with a real pass does unlock.
    let pass = verdict(&lock_fp, "PASS");
    aaf_fp::write_approval(repo.data_dir(), &key, repo.root(), &pass).unwrap();
    assert_eq!(report(&repo).status, LockStatus::Approved);
}

#[test]
fn unusable_glob_or_base_fails_closed() {
    let repo = Repo::new();
    repo.append("tests/a_test.rs", "// weakened\n");
    let mut cfg = RepoConfig::default();
    cfg.gate.head_lock_globs = vec!["tests/[".to_string()];
    let report = evaluate(repo.data_dir(), repo.root(), &cfg, "main");
    assert_eq!(report.status, LockStatus::Blocked);
    let report = evaluate(
        repo.data_dir(),
        repo.root(),
        &RepoConfig::default(),
        "no-such-ref",
    );
    assert_eq!(report.status, LockStatus::Blocked);
}
