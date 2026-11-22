#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::process::Command;

use aaf_domain::gate::{
    Approval, GateMode, GateStatus, HeadLockReport, LockStatus, Receipt, StoreComplianceReport,
    StoreStatus,
};
use aaf_fp::{GIT_TIMEOUT, Signed};

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(status.status.success(), "git {args:?} failed");
}

fn repo_with_commit() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    fs::write(dir.path().join("a.txt"), "a\n").unwrap();
    fs::write(dir.path().join(".gitignore"), "ignored/\n").unwrap();
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    dir
}

fn fp(repo: &Path) -> String {
    aaf_fp::tree_fp(repo, GIT_TIMEOUT).unwrap()
}

#[test]
fn untracked_file_changes_the_fingerprint() {
    let repo = repo_with_commit();
    let clean = fp(repo.path());
    fs::write(repo.path().join("NewUntracked.kt"), "evil\n").unwrap();
    assert_ne!(clean, fp(repo.path()));
}

#[test]
fn edit_and_delete_change_the_fingerprint() {
    let repo = repo_with_commit();
    let clean = fp(repo.path());
    fs::write(repo.path().join("a.txt"), "b\n").unwrap();
    let edited = fp(repo.path());
    assert_ne!(clean, edited);
    fs::remove_file(repo.path().join("a.txt")).unwrap();
    assert_ne!(edited, fp(repo.path()));
}

#[test]
fn committing_the_same_content_keeps_the_fingerprint() {
    let repo = repo_with_commit();
    fs::write(repo.path().join("b.txt"), "b\n").unwrap();
    let dirty = fp(repo.path());
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-q", "-m", "second"]);
    assert_eq!(dirty, fp(repo.path()));
}

#[test]
fn ignored_files_and_the_real_index_are_left_alone() {
    let repo = repo_with_commit();
    let clean = fp(repo.path());
    fs::create_dir(repo.path().join("ignored")).unwrap();
    fs::write(repo.path().join("ignored/out.bin"), "x").unwrap();
    fs::write(repo.path().join("staged-not.txt"), "x").unwrap();
    let _ = fp(repo.path());
    fs::remove_file(repo.path().join("staged-not.txt")).unwrap();
    assert_eq!(clean, fp(repo.path()));
    // Fingerprinting must not stage anything in the real index.
    let staged = Command::new("git")
        .arg("-C")
        .arg(repo.path())
        .args(["diff", "--cached", "--name-only"])
        .output()
        .unwrap();
    assert!(staged.stdout.is_empty());
    let leftovers: Vec<_> = fs::read_dir(repo.path().join(".git"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("aaf-index-"))
        .collect();
    assert!(leftovers.is_empty());
}

#[test]
fn works_in_a_repo_without_commits() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    let empty = fp(dir.path());
    fs::write(dir.path().join("a.txt"), "a\n").unwrap();
    assert_ne!(empty, fp(dir.path()));
    assert_eq!(aaf_fp::head_sha(dir.path()), None);
}

#[test]
fn signature_rejects_a_changed_payload_and_a_wrong_key() {
    let signed = aaf_fp::sign(&[7u8; 32], "{\"a\":1}".to_string()).unwrap();
    assert!(aaf_fp::verify(&[7u8; 32], &signed));
    assert!(!aaf_fp::verify(&[8u8; 32], &signed));
    let forged = Signed {
        payload: "{\"a\":2}".to_string(),
        hmac: signed.hmac.clone(),
    };
    assert!(!aaf_fp::verify(&[7u8; 32], &forged));
    let garbage = Signed {
        payload: signed.payload,
        hmac: "zz".to_string(),
    };
    assert!(!aaf_fp::verify(&[7u8; 32], &garbage));
}

fn receipt(repo_root: &Path, tree_fp: &str) -> Receipt {
    Receipt {
        schema_version: 1,
        repo_root: repo_root.to_string_lossy().into_owned(),
        tree_fp: tree_fp.to_string(),
        head_sha: None,
        base_sha: None,
        mode: GateMode::Release,
        status: GateStatus::Clear,
        reasons: Vec::new(),
        config_hash: String::new(),
        commands: Vec::new(),
        static_findings: Vec::new(),
        suppressions: 0,
        head_lock: HeadLockReport {
            status: LockStatus::Clear,
            lock_fp: String::new(),
            locked_changes: Vec::new(),
            approval: None,
        },
        store_compliance: StoreComplianceReport::from_findings(Vec::new(), Vec::new()),
        created_at: 1,
    }
}

#[test]
fn receipt_round_trips_and_a_hand_written_one_is_refused() {
    let data = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let key = aaf_fp::load_or_create_key(data.path()).unwrap();
    assert_eq!(key, aaf_fp::load_key(data.path()).unwrap());

    let original = receipt(repo.path(), "git-tree:abc");
    let path = aaf_fp::write_receipt(data.path(), &key, &original).unwrap();
    assert_eq!(
        aaf_fp::read_receipt(data.path(), &key, repo.path()).unwrap(),
        original
    );

    // An agent writing its own "CLEAR" receipt has no key to sign it with.
    let forged = serde_json::json!({
        "payload": serde_json::to_string(&original).unwrap().replace("abc", "xyz"),
        "hmac": "00".repeat(32),
    });
    fs::write(&path, forged.to_string()).unwrap();
    assert!(aaf_fp::read_receipt(data.path(), &key, repo.path()).is_err());
}

#[test]
fn key_file_is_private_to_the_user() {
    use std::os::unix::fs::PermissionsExt;
    let data = tempfile::tempdir().unwrap();
    aaf_fp::load_or_create_key(data.path()).unwrap();
    let mode = fs::metadata(data.path().join("receipt.key"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn approval_round_trips_and_missing_is_none() {
    let data = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let key = aaf_fp::load_or_create_key(data.path()).unwrap();
    assert_eq!(
        aaf_fp::read_approval(data.path(), &key, repo.path(), "fp1").unwrap(),
        None
    );
    let approval = Approval {
        lock_fp: "fp1".to_string(),
        reviewer: "human".to_string(),
        verdict: "PASS".to_string(),
        same_account: false,
        created_at: 5,
    };
    aaf_fp::write_approval(data.path(), &key, repo.path(), &approval).unwrap();
    let read = aaf_fp::read_approval(data.path(), &key, repo.path(), "fp1").unwrap();
    assert_eq!(read, Some(approval));
}

#[test]
fn store_receipt_rule_refuses_every_non_clear_case() {
    let repo = tempfile::tempdir().unwrap();
    let good = receipt(repo.path(), "git-tree:abc");
    assert!(aaf_fp::check_store_receipt(&good, "git-tree:abc").is_ok());
    assert!(aaf_fp::check_store_receipt(&good, "git-tree:other").is_err());

    let mut scoped = good.clone();
    scoped.mode = GateMode::Scoped;
    assert!(aaf_fp::check_store_receipt(&scoped, "git-tree:abc").is_err());

    let mut blocked = good.clone();
    blocked.status = GateStatus::Blocked;
    assert!(aaf_fp::check_store_receipt(&blocked, "git-tree:abc").is_err());

    for status in [
        StoreStatus::Skipped,
        StoreStatus::NotApplicable,
        StoreStatus::Blocked,
    ] {
        let mut receipt = good.clone();
        receipt.store_compliance.status = status;
        assert!(aaf_fp::check_store_receipt(&receipt, "git-tree:abc").is_err());
    }
}

#[test]
fn git_returns_output_larger_than_a_pipe_buffer() {
    let repo = repo_with_commit();
    // Well above any pipe buffer (64 KiB on macOS and Linux).
    let big = "0123456789abcdef\n".repeat(80_000);
    assert!(big.len() > 1_048_576);
    fs::write(repo.path().join("big.txt"), &big).unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-q", "-m", "big"]);
    let out = aaf_fp::git(
        repo.path(),
        &["cat-file", "-p", "HEAD:big.txt"],
        &[],
        std::time::Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(out, big.trim());
}

#[test]
fn git_still_times_out_and_reports_failures() {
    let repo = repo_with_commit();
    let err = aaf_fp::git(
        repo.path(),
        &["cat-file", "--batch"],
        &[],
        std::time::Duration::ZERO,
    )
    .unwrap_err();
    assert!(err.to_string().contains("timed out"), "{err}");
    let err = aaf_fp::git(
        repo.path(),
        &["rev-parse", "--verify", "nope"],
        &[],
        GIT_TIMEOUT,
    )
    .unwrap_err();
    assert!(err.to_string().contains("exit"), "{err}");
}
