use std::fs;

use aaf_domain::config::RepoConfig;
use aaf_domain::gate::{Approval, ChangeKind, HeadLockReport, LockStatus, LockedChange};

use super::super::GateError;
use super::super::base;
use super::super::head_lock::{approve, approve_expecting, approve_inner, content_hash, evaluate};
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
            change: ChangeKind::Modified,
            tier: aaf_domain::gate::LockTier::Soft,
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
            change: ChangeKind::Renamed,
            tier: aaf_domain::gate::LockTier::Soft,
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

/// The cockpit, web UI and Telegram sign only the locked diff the reviewer
/// was shown: an edit after the preview is refused before any key or
/// approval is touched (audit a13 L8-R3-1).
#[test]
fn approve_expecting_refuses_a_stale_fingerprint_and_writes_nothing() {
    let repo = Repo::new();
    repo.append("tests/a_test.rs", "// previewed\n");
    let shown = report(&repo).lock_fp;
    repo.append("tests/a_test.rs", "// sneaked in after the preview\n");
    let cfg = RepoConfig::default();
    let base = base::resolve(repo.root(), "main", super::super::GIT_TIMEOUT).unwrap();
    let clock = FixedClock(42);
    let sign = |expected: &str| {
        let who = ("reviewer-1", false);
        approve_expecting(
            repo.data_dir(),
            repo.root(),
            &cfg,
            &base,
            who,
            &clock,
            expected,
        )
    };
    let refused = sign(&shown);
    assert!(
        matches!(refused, Err(GateError::LockFpChanged)),
        "{refused:?}"
    );
    assert!(
        !repo.data_dir().join("approvals").exists(),
        "nothing signed"
    );
    assert_eq!(report(&repo).status, LockStatus::Blocked);

    let now = report(&repo).lock_fp;
    assert_eq!(sign(&now).unwrap().lock_fp, now);
    assert_eq!(report(&repo).status, LockStatus::Approved);

    let clean = Repo::new();
    let base = base::resolve(clean.root(), "main", super::super::GIT_TIMEOUT).unwrap();
    let who = ("reviewer-1", false);
    let none = approve_expecting(
        clean.data_dir(),
        clean.root(),
        &cfg,
        &base,
        who,
        &clock,
        &now,
    );
    assert!(matches!(none, Err(GateError::NothingLocked)), "{none:?}");
}

/// One condition, one error: the signing core itself refuses a stale
/// preview with the production `LockFpChanged` (no second error variant or
/// approval API that callers must remember to translate).
#[test]
fn the_signing_core_refuses_a_stale_preview_with_lock_fp_changed() {
    let repo = Repo::new();
    repo.append("tests/a_test.rs", "// previewed\n");
    let shown = report(&repo).lock_fp;
    repo.append("tests/a_test.rs", "// sneaked in after the preview\n");
    let cfg = RepoConfig::default();
    let base = base::resolve(repo.root(), "main", super::super::GIT_TIMEOUT).unwrap();
    let mut read_hash = |path: &str| content_hash(repo.root(), path);
    let refused = approve_inner(
        repo.data_dir(),
        repo.root(),
        &cfg,
        &base,
        ("reviewer-1", false),
        &FixedClock(42),
        Some(&shown),
        &mut read_hash,
    );
    assert!(
        matches!(refused, Err(GateError::LockFpChanged)),
        "stale preview must be refused with LockFpChanged, got {refused:?}"
    );
    assert!(
        !repo.data_dir().join("approvals").exists(),
        "nothing signed"
    );
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

/// One approval reads each locked file once: the base-config and
/// tracked-only fingerprints are signed for the same content as the one
/// the reviewer was shown, not for an edit made while signing (audit a13
/// R4-L2-1).
#[test]
fn approve_signs_every_fingerprint_from_one_read_of_each_file() {
    let repo = Repo::new();
    repo.write(".autofactory/config.toml", "");
    repo.commit("base config");
    repo.git(&["branch", "-f", "main"]);
    repo.append("tests/a_test.rs", "// reviewed\n");
    repo.append("src/lib.rs", "// reviewed\n");
    repo.write("tests/new_test.rs", "fn added() {}\n");
    // The worktree config also locks `src/`: its fingerprint and the one
    // under the base config differ.
    let mut cfg = RepoConfig::default();
    cfg.gate.head_lock_globs.push("src/**".to_string());
    let base = base::resolve(repo.root(), "main", super::super::GIT_TIMEOUT).unwrap();
    let mut reads = std::collections::HashMap::<String, u32>::new();
    // The real content the first time, an edit every time after.
    let mut read_hash = |path: &str| -> Result<String, GateError> {
        let count = reads.entry(path.to_string()).or_default();
        *count += 1;
        if *count == 1 {
            let bytes = fs::read(repo.path(path)).unwrap();
            Ok(aaf_fp::sha256_hex(&bytes))
        } else {
            Ok(format!("edited-{count}"))
        }
    };
    let who = ("reviewer-1", false);
    let clock = FixedClock(42);
    let root = repo.root();
    let signed = approve_inner(
        repo.data_dir(),
        root,
        &cfg,
        &base,
        who,
        &clock,
        None,
        &mut read_hash,
    );
    signed.unwrap();
    for (path, count) in &reads {
        assert_eq!(*count, 1, "{path} read {count} times: {reads:?}");
    }
    let under = |cfg: &RepoConfig| evaluate(repo.data_dir(), root, cfg, "main").status;
    assert_eq!(under(&cfg), LockStatus::Approved);
    assert_eq!(under(&RepoConfig::default()), LockStatus::Approved);
}

/// A locked path whose name git reported in bytes that are not UTF-8 was
/// decoded lossily: reading the decoded name finds nothing, which must not
/// pass for a deletion an approval could cover (audit a13 L2-R5-1).
#[test]
fn missing_lossily_decoded_path_fails_closed() {
    let repo = Repo::new();
    let err = content_hash(repo.root(), "tests/bad_\u{FFFD}_test.rs").unwrap_err();
    assert!(
        matches!(&err, GateError::Config(msg) if msg.contains("not valid UTF-8")),
        "{err:?}"
    );
    let gone = content_hash(repo.root(), "tests/gone_test.rs").unwrap();
    assert_eq!(gone, "deleted");
}

#[test]
fn directory_content_hash_is_deterministic_sensitive_and_ignores_git_dirs() {
    let repo = Repo::new();
    repo.write(".cargo/config.toml", "[build]\nrustc = \"/bin/true\"\n");
    repo.write(".cargo/sub/a.txt", "hello\n");
    repo.write(".cargo/.git/HEAD", "ref: refs/heads/main\n");
    repo.write(".cargo/sub/.git/HEAD", "ref: refs/heads/nested\n");

    let first = content_hash(repo.root(), ".cargo").unwrap();
    let second = content_hash(repo.root(), ".cargo").unwrap();
    let trailing = content_hash(repo.root(), ".cargo/").unwrap();
    assert!(
        first.starts_with("dir:"),
        "expected dir: prefix, got {first}"
    );
    assert_eq!(first, second);
    assert_eq!(first, trailing);

    // Mutating `.git` subdirectories at any depth must not change the hash.
    repo.write(".cargo/.git/HEAD", "0123456789abcdef\n");
    repo.write(".cargo/.git/objects/info/packs", "pack\n");
    repo.write(".cargo/sub/.git/index", "changed\n");
    assert_eq!(content_hash(repo.root(), ".cargo").unwrap(), first);

    // Changing a child file must change the hash.
    repo.write(".cargo/config.toml", "[build]\nrustc = \"/bin/false\"\n");
    let modified = content_hash(repo.root(), ".cargo").unwrap();
    assert_ne!(modified, first);
    assert!(modified.starts_with("dir:"));
}

#[test]
fn nested_git_repo_at_locked_dir_has_approvable_lock_fp_that_tracks_edits() {
    let repo = Repo::new();
    let cargo_dir = repo.path(".cargo");
    fs::create_dir_all(&cargo_dir).unwrap();
    repo.git(&["-C", ".cargo", "init", "-q", "-b", "main"]);
    repo.git(&["-C", ".cargo", "config", "user.name", "test"]);
    repo.git(&[
        "-C",
        ".cargo",
        "config",
        "user.email",
        "test@example.invalid",
    ]);
    repo.git(&["-C", ".cargo", "config", "commit.gpgsign", "false"]);
    repo.write(".cargo/config.toml", "[build]\nrustc = \"/bin/true\"\n");
    repo.git(&["-C", ".cargo", "add", "-A"]);
    repo.git(&["-C", ".cargo", "commit", "-q", "-m", "nested"]);

    // Untracked nested repositories are listed by git with a trailing `/`.
    let locked = vec![LockedChange {
        path: ".cargo/".to_string(),
        change: ChangeKind::Added,
        tier: aaf_domain::gate::LockTier::Hard,
    }];
    let before = report(&repo);
    assert_eq!(before.status, LockStatus::Blocked);
    assert_eq!(before.lock_fp.len(), 64);
    assert_eq!(before.locked_changes.len(), 1, "{before:?}");
    assert_eq!(before.locked_changes, locked);

    let approval = sign_off(&repo).unwrap();
    assert_eq!(approval.lock_fp, before.lock_fp);
    assert_eq!(report(&repo).status, LockStatus::Approved);

    repo.write(".cargo/config.toml", "[build]\nrustc = \"/bin/false\"\n");
    let after = report(&repo);
    assert_eq!(after.status, LockStatus::Blocked);
    assert_eq!(after.lock_fp.len(), 64);
    assert_ne!(after.lock_fp, before.lock_fp);
    assert_eq!(after.locked_changes, locked);
}

#[test]
fn the_effective_changes_come_back_with_the_report() {
    use super::super::changes;
    use super::super::head_lock::evaluate_changes_effective;

    let repo = Repo::new();
    repo.append("tests/a_test.rs", "// weakened\n");
    repo.write("src/new.rs", "pub fn n() {}\n");
    let set = changes::collect(repo.root(), "main", super::super::GIT_TIMEOUT).unwrap();
    let (report, effective) = evaluate_changes_effective(
        repo.data_dir(),
        repo.root(),
        None,
        &RepoConfig::default(),
        &[],
        ("main", &set.changes),
        &[],
        false,
    )
    .unwrap();
    assert_eq!(report.status, LockStatus::Blocked);
    assert_eq!(effective.changes, set.changes);
    // Same report as the function every other caller uses.
    let plain = super::super::head_lock::evaluate_changes_with_common(
        repo.data_dir(),
        repo.root(),
        None,
        &RepoConfig::default(),
        &[],
        ("main", &set.changes),
        &[],
        false,
    )
    .unwrap();
    assert_eq!(plain, report);
}
