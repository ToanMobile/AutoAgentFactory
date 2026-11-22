#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Domain separation of receipt and approval signatures, and the binding of
//! a store receipt to the repository it was issued for.

use std::fs;
use std::path::Path;

use aaf_domain::gate::{
    Approval, GateMode, GateStatus, HeadLockReport, LockStatus, Receipt, StoreComplianceReport,
};
use aaf_fp::{FpError, Signed};

const TREE: &str = "git-tree:abc";

fn receipt(repo_root: &Path) -> Receipt {
    Receipt {
        schema_version: 1,
        repo_root: repo_root.to_string_lossy().into_owned(),
        tree_fp: TREE.to_string(),
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

fn approval() -> Approval {
    Approval {
        lock_fp: "fp1".to_string(),
        reviewer: "human".to_string(),
        verdict: "PASS".to_string(),
        same_account: false,
        created_at: 5,
    }
}

struct Env {
    data: tempfile::TempDir,
    repo: tempfile::TempDir,
    key: Vec<u8>,
}

fn env() -> Env {
    let data = tempfile::tempdir().unwrap();
    let key = aaf_fp::load_or_create_key(data.path()).unwrap();
    Env {
        data,
        repo: tempfile::tempdir().unwrap(),
        key,
    }
}

fn store(path: &Path, signed: &Signed) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec(signed).unwrap()).unwrap();
}

fn load(path: &Path) -> Signed {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn a_signed_approval_does_not_verify_as_a_receipt_and_vice_versa() {
    let e = env();
    let (data, repo) = (e.data.path(), e.repo.path());
    let receipt_file = aaf_fp::write_receipt(data, &e.key, &receipt(repo)).unwrap();
    let approval_file = aaf_fp::write_approval(data, &e.key, repo, &approval()).unwrap();
    let (signed_receipt, signed_approval) = (load(&receipt_file), load(&approval_file));

    // Replayed across types, the signature itself must fail; a parse error
    // would mean the HMAC was accepted for the wrong kind of document.
    store(&receipt_file, &signed_approval);
    let err = aaf_fp::read_receipt(data, &e.key, repo).unwrap_err();
    assert!(matches!(err, FpError::BadSignature), "{err}");

    store(&approval_file, &signed_receipt);
    let err = aaf_fp::read_approval(data, &e.key, repo, "fp1").unwrap_err();
    assert!(matches!(err, FpError::BadSignature), "{err}");

    // Allowed side: each file back in its own place reads fine.
    store(&receipt_file, &signed_receipt);
    store(&approval_file, &signed_approval);
    assert_eq!(
        aaf_fp::read_receipt(data, &e.key, repo).unwrap(),
        receipt(repo)
    );
    assert_eq!(
        aaf_fp::read_approval(data, &e.key, repo, "fp1").unwrap(),
        Some(approval())
    );
}

/// Files written before the domain tags existed carry a plain HMAC of the
/// payload. They are refused (fail closed): the gate has to run again.
#[test]
fn untagged_legacy_signatures_are_rejected() {
    let e = env();
    let (data, repo) = (e.data.path(), e.repo.path());

    let payload = serde_json::to_string(&receipt(repo)).unwrap();
    let legacy = aaf_fp::sign(&e.key, payload).unwrap();
    assert!(aaf_fp::verify(&e.key, &legacy));
    store(&aaf_fp::receipt_path(data, repo), &legacy);
    let err = aaf_fp::read_receipt(data, &e.key, repo).unwrap_err();
    assert!(matches!(err, FpError::BadSignature), "{err}");

    let payload = serde_json::to_string(&approval()).unwrap();
    let legacy = aaf_fp::sign(&e.key, payload).unwrap();
    store(&aaf_fp::approval_path(data, repo, "fp1"), &legacy);
    let err = aaf_fp::read_approval(data, &e.key, repo, "fp1").unwrap_err();
    assert!(matches!(err, FpError::BadSignature), "{err}");
}

#[test]
fn tagged_signatures_are_bound_to_their_domain_and_key() {
    let key = [7u8; 32];
    let payload = "{\"a\":1}".to_string();
    let signed = aaf_fp::sign_tagged(&key, aaf_fp::RECEIPT_DOMAIN, payload.clone()).unwrap();
    assert!(aaf_fp::verify_tagged(&key, aaf_fp::RECEIPT_DOMAIN, &signed));
    assert!(!aaf_fp::verify_tagged(
        &key,
        aaf_fp::APPROVAL_DOMAIN,
        &signed
    ));
    assert!(!aaf_fp::verify_tagged(
        &[8u8; 32],
        aaf_fp::RECEIPT_DOMAIN,
        &signed
    ));
    assert!(!aaf_fp::verify(&key, &signed));
    assert_ne!(signed.hmac, aaf_fp::sign(&key, payload).unwrap().hmac);
}

#[test]
fn store_receipt_copied_from_another_repo_is_rejected() {
    let e = env();
    let data = e.data.path();
    let repo_a = e.repo.path().canonicalize().unwrap();
    let other = tempfile::tempdir().unwrap();
    let repo_b = other.path().canonicalize().unwrap();

    let file_a = aaf_fp::write_receipt(data, &e.key, &receipt(&repo_a)).unwrap();
    // Repo A passed its release gate; its receipt is copied into B's slot.
    let file_b = aaf_fp::receipt_path(data, &repo_b);
    fs::create_dir_all(file_b.parent().unwrap()).unwrap();
    fs::copy(&file_a, &file_b).unwrap();
    let copied = aaf_fp::read_receipt(data, &e.key, &repo_b).unwrap();

    let refused = aaf_fp::check_store_receipt_in(&repo_b, &copied, TREE).unwrap_err();
    assert!(refused.contains("another repository"), "{refused}");

    // Allowed side: the same receipt for the repo it was issued for, also
    // when that repo is reached through a symlink.
    let own = aaf_fp::read_receipt(data, &e.key, &repo_a).unwrap();
    assert!(aaf_fp::check_store_receipt_in(&repo_a, &own, TREE).is_ok());
    let link = other.path().join("link-to-a");
    std::os::unix::fs::symlink(&repo_a, &link).unwrap();
    assert!(aaf_fp::check_store_receipt_in(&link, &own, TREE).is_ok());

    // The other rules still apply through the new entry point.
    assert!(aaf_fp::check_store_receipt_in(&repo_a, &own, "git-tree:other").is_err());
    // A repo root that cannot be resolved is refused, not compared as text.
    let gone = repo_a.join("does-not-exist");
    assert!(aaf_fp::check_store_receipt_in(&gone, &receipt(&gone), TREE).is_err());
}

/// Receipts and approvals decide what may merge; nothing in them is for
/// another account. The umask is not relied on, and what an earlier version
/// left open is closed by the next write.
#[cfg(unix)]
#[test]
fn receipts_and_approvals_are_written_owner_only_and_old_ones_are_tightened() {
    use std::os::unix::fs::PermissionsExt;

    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let open = |path: &Path, bits: u32| {
        fs::set_permissions(path, fs::Permissions::from_mode(bits)).unwrap();
    };
    let e = env();
    let (data, repo) = (e.data.path(), e.repo.path());

    // As an earlier version left them: 0755 directories, 0644 files.
    let receipt_file = aaf_fp::receipt_path(data, repo);
    let approval_file = aaf_fp::approval_path(data, repo, "fp1");
    for file in [&receipt_file, &approval_file] {
        let dir = file.parent().unwrap();
        fs::create_dir_all(dir).unwrap();
        fs::write(file, "old").unwrap();
        open(file, 0o644);
        open(dir, 0o755);
        open(dir.parent().unwrap(), 0o755);
    }
    open(data, 0o755);

    let written = aaf_fp::write_receipt(data, &e.key, &receipt(repo)).unwrap();
    assert_eq!(written, receipt_file);
    let written = aaf_fp::write_approval(data, &e.key, repo, &approval()).unwrap();
    assert_eq!(written, approval_file);

    for file in [&receipt_file, &approval_file] {
        assert_eq!(mode(file), 0o600, "{}", file.display());
        // `<kind>/<repo id>/` and `<kind>/`, up to the data dir itself.
        let dir = file.parent().unwrap();
        assert_eq!(mode(dir), 0o700, "{}", dir.display());
        assert_eq!(mode(dir.parent().unwrap()), 0o700);
        // No temp file is left beside it, open or not.
        let names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }
    assert_eq!(mode(data), 0o700);
    // Still what was signed.
    aaf_fp::read_receipt(data, &e.key, repo).unwrap();
    assert!(
        aaf_fp::read_approval(data, &e.key, repo, "fp1")
            .unwrap()
            .is_some()
    );
}

/// A fresh data dir: created private, not created open and closed later.
#[cfg(unix)]
#[test]
fn a_first_receipt_creates_private_directories() {
    use std::os::unix::fs::PermissionsExt;

    let e = env();
    let data = e.data.path().join("not-there-yet");
    let file = aaf_fp::write_receipt(&data, &e.key, &receipt(e.repo.path())).unwrap();
    for path in [
        file.parent().unwrap(),
        file.parent().unwrap().parent().unwrap(),
        &data,
    ] {
        let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{}", path.display());
    }
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
}
