#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Store submission against a real git repository and receipts written
//! through `aaf_fp::write_receipt`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use aaf_domain::gate::{
    GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION, Receipt,
    StoreComplianceReport, StoreStatus,
};
use aaf_hook::{Decision, HookEnv, decide};
use serde_json::json;

struct Project {
    _dir: tempfile::TempDir,
    repo: PathBuf,
    data: PathBuf,
    home: PathBuf,
}

fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?} failed: {output:?}");
}

impl Project {
    fn new() -> Project {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let project = Project {
            repo: base.join("app"),
            data: base.join("data"),
            home: base.join("home"),
            _dir: dir,
        };
        for path in [&project.repo, &project.data, &project.home] {
            fs::create_dir_all(path).unwrap();
        }
        git(&project.repo, &["init", "-q", "-b", "main"]);
        fs::write(project.repo.join("app.txt"), "v1\n").unwrap();
        fs::write(project.repo.join(".gitignore"), "build/\n").unwrap();
        git(&project.repo, &["add", "-A"]);
        git(&project.repo, &["commit", "-q", "-m", "init"]);
        project
    }

    fn receipt(&self) -> Receipt {
        Receipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            repo_root: self.repo.to_string_lossy().into_owned(),
            tree_fp: aaf_fp::tree_fp(&self.repo, aaf_fp::GIT_TIMEOUT).unwrap(),
            head_sha: aaf_fp::head_sha(&self.repo),
            base_sha: None,
            mode: GateMode::Release,
            status: GateStatus::Clear,
            reasons: Vec::new(),
            config_hash: "test".to_string(),
            commands: Vec::new(),
            static_findings: Vec::new(),
            suppressions: 0,
            head_lock: HeadLockReport {
                status: LockStatus::Clear,
                lock_fp: String::new(),
                locked_changes: Vec::new(),
                approval: None,
            },
            store_compliance: StoreComplianceReport {
                status: StoreStatus::Clear,
                critical: 0,
                warnings: 0,
                unresolved: Vec::new(),
                findings: Vec::new(),
            },
            created_at: 1_700_000_000,
        }
    }

    /// Signs `receipt` with the data dir's key, as `aaf gate --release` does.
    fn gate(&self, receipt: &Receipt) -> PathBuf {
        let key = aaf_fp::load_or_create_key(&self.data).unwrap();
        aaf_fp::write_receipt(&self.data, &key, receipt).unwrap()
    }

    fn payload(&self, command: &str, cwd: &Path) -> String {
        json!({"tool_name": "Bash", "tool_input": {"command": command}, "cwd": cwd}).to_string()
    }

    /// In-process decision with the real git-backed repo lookup.
    fn submit_from(&self, command: &str, cwd: &Path) -> Result<(), String> {
        let lookup = |dir: &Path| aaf_fp::repo_root(dir).ok();
        let env = HookEnv {
            repo_root: &lookup,
            git_config: &|_| Some(String::new()),
            data_dir: Some(self.data.clone()),
            config_dir: None,
            home: Some(self.home.clone()),
            cwd: None,
            in_factory_run: false,
            trusted_aaf_bins: Vec::new(),
        };
        match decide(self.payload(command, cwd).as_bytes(), &env) {
            Decision::Allow => Ok(()),
            Decision::Deny(denial) => {
                assert_eq!(denial.rule, "store_submit_guard");
                assert_eq!(denial.fix, "aaf gate --release");
                Err(denial.reason)
            }
        }
    }

    fn submit(&self, command: &str) -> Result<(), String> {
        self.submit_from(command, &self.repo)
    }
}

#[test]
fn a_matching_release_receipt_allows_the_submission() {
    let project = Project::new();
    project.gate(&project.receipt());
    for command in [
        "fastlane deliver",
        "eas submit -p ios",
        "xcrun altool --upload-app -f app.ipa",
        // A Gradle run is a heavy build: it goes through the machine queue,
        // and the receipt is still checked for the command inside the slot.
        "aaf slot -- ./gradlew publishReleaseBundle",
    ] {
        assert_eq!(project.submit(command), Ok(()), "{command}");
    }
    // Committing the same content does not invalidate the receipt.
    fs::write(project.repo.join("later.txt"), "x\n").unwrap();
    project.gate(&project.receipt());
    git(&project.repo, &["add", "-A"]);
    git(&project.repo, &["commit", "-q", "-m", "later"]);
    assert_eq!(project.submit("fastlane deliver"), Ok(()));
}

#[test]
fn the_receipt_also_covers_subdirectories_and_cd() {
    let project = Project::new();
    fs::create_dir(project.repo.join("ios")).unwrap();
    fs::write(project.repo.join("ios/Fastfile"), "lane\n").unwrap();
    project.gate(&project.receipt());
    assert_eq!(project.submit("cd ios && fastlane deliver"), Ok(()));
    assert_eq!(
        project.submit_from("fastlane deliver", &project.repo.join("ios")),
        Ok(())
    );
}

#[test]
fn any_file_change_after_the_gate_denies() {
    let project = Project::new();
    project.gate(&project.receipt());
    assert_eq!(project.submit("fastlane deliver"), Ok(()));

    fs::write(project.repo.join("app.txt"), "v2\n").unwrap();
    let reason = project.submit("fastlane deliver").unwrap_err();
    assert!(reason.contains("work tree changed"), "{reason}");

    fs::write(project.repo.join("app.txt"), "v1\n").unwrap();
    assert_eq!(project.submit("fastlane deliver"), Ok(()));

    fs::write(project.repo.join("Untracked.kt"), "new\n").unwrap();
    assert!(project.submit("fastlane deliver").is_err());
    fs::remove_file(project.repo.join("Untracked.kt")).unwrap();
    assert_eq!(project.submit("fastlane deliver"), Ok(()));

    fs::remove_file(project.repo.join("app.txt")).unwrap();
    assert!(project.submit("fastlane deliver").is_err());
}

#[test]
fn a_tracked_file_git_was_told_not_to_look_at_denies_the_submission() {
    let project = Project::new();
    project.gate(&project.receipt());
    assert_eq!(project.submit("fastlane deliver"), Ok(()));

    for (set, clear) in [
        ("--skip-worktree", "--no-skip-worktree"),
        ("--assume-unchanged", "--no-assume-unchanged"),
    ] {
        git(&project.repo, &["update-index", set, "app.txt"]);
        // On disk it is no longer what was gated, and git does not say so:
        // the fingerprint of the tree is still the receipt's.
        fs::write(project.repo.join("app.txt"), "not what was gated\n").unwrap();
        let reason = project.submit("fastlane deliver").unwrap_err();
        assert!(reason.contains("app.txt"), "{set}: {reason}");
        assert!(reason.contains("skip-worktree"), "{set}: {reason}");

        // The same tree without the flag is the gated one again.
        fs::write(project.repo.join("app.txt"), "v1\n").unwrap();
        git(&project.repo, &["update-index", clear, "app.txt"]);
        assert_eq!(project.submit("fastlane deliver"), Ok(()), "{set}");
    }
}

#[test]
fn a_file_ignored_outside_the_tree_is_left_to_the_release_gate() {
    let project = Project::new();
    project.gate(&project.receipt());
    // Every developer machine has such files (`.DS_Store` under a global
    // excludes file): the hook does not refuse them, `aaf gate --release`
    // does before it signs.
    fs::write(project.repo.join(".git/info/exclude"), "local.cache\n").unwrap();
    fs::write(project.repo.join("local.cache"), "x\n").unwrap();
    let hidden = aaf_fp::hidden_files(&project.repo, aaf_fp::GIT_TIMEOUT).unwrap();
    assert_eq!(hidden.ignored.len(), 1, "{hidden:?}");
    assert_eq!(project.submit("fastlane deliver"), Ok(()));
}

#[test]
fn a_forged_receipt_denies() {
    let project = Project::new();
    let path = project.gate(&project.receipt());
    assert_eq!(project.submit("fastlane deliver"), Ok(()));

    // Signed with a key that is not the data dir's key.
    aaf_fp::write_receipt(&project.data, &[7u8; 32], &project.receipt()).unwrap();
    let reason = project.submit("fastlane deliver").unwrap_err();
    assert!(reason.contains("signature"), "{reason}");

    // A valid blocked receipt whose payload is edited to say CLEAR.
    let mut blocked = project.receipt();
    blocked.status = GateStatus::Blocked;
    project.gate(&blocked);
    assert!(project.submit("fastlane deliver").is_err());
    let edited = fs::read_to_string(&path)
        .unwrap()
        .replace("BLOCKED", "CLEAR");
    assert!(edited.contains("CLEAR"));
    fs::write(&path, edited).unwrap();
    let reason = project.submit("fastlane deliver").unwrap_err();
    assert!(reason.contains("signature"), "{reason}");

    // Not a receipt at all.
    fs::write(&path, "{\"payload\":\"{}\",\"hmac\":\"00\"}").unwrap();
    assert!(project.submit("fastlane deliver").is_err());
}

#[test]
fn a_receipt_issued_for_another_repository_denies() {
    let project = Project::new();
    // A genuine, validly signed Release/Clear receipt of another repository
    // whose tree happens to be identical, copied into this repo's slot.
    let other = project.repo.with_file_name("other");
    fs::create_dir_all(&other).unwrap();
    let mut foreign = project.receipt();
    foreign.repo_root = other.to_string_lossy().into_owned();
    let issued = project.gate(&foreign);
    let slot = aaf_fp::receipt_path(&project.data, &project.repo);
    assert_ne!(issued, slot);
    fs::create_dir_all(slot.parent().unwrap()).unwrap();
    fs::copy(&issued, &slot).unwrap();
    let reason = project.submit("fastlane deliver").unwrap_err();
    assert!(reason.contains("another repository"), "{reason}");

    // The repo's own receipt in the same slot is accepted.
    assert_eq!(project.gate(&project.receipt()), slot);
    assert_eq!(project.submit("fastlane deliver"), Ok(()));
}

#[test]
fn receipts_that_are_not_a_clear_release_gate_deny() {
    let project = Project::new();
    let mut scoped = project.receipt();
    scoped.mode = GateMode::Scoped;
    project.gate(&scoped);
    assert!(project.submit("fastlane deliver").is_err());

    let mut unverified = project.receipt();
    unverified.status = GateStatus::Unverified;
    project.gate(&unverified);
    assert!(project.submit("fastlane deliver").is_err());

    let mut skipped = project.receipt();
    skipped.store_compliance = StoreComplianceReport::skipped();
    project.gate(&skipped);
    assert!(project.submit("fastlane deliver").is_err());

    project.gate(&project.receipt());
    assert_eq!(project.submit("fastlane deliver"), Ok(()));
}

#[test]
fn missing_key_missing_receipt_and_no_repository_deny() {
    let project = Project::new();
    // No key yet.
    let reason = project.submit("fastlane deliver").unwrap_err();
    assert!(reason.contains("key"), "{reason}");
    // Key, but no receipt for this repo.
    aaf_fp::load_or_create_key(&project.data).unwrap();
    let reason = project.submit("fastlane deliver").unwrap_err();
    assert!(reason.contains("receipt"), "{reason}");
    // Outside any repository.
    project.gate(&project.receipt());
    assert!(
        project
            .submit_from("fastlane deliver", &project.home)
            .is_err()
    );
    // Commands that are not submissions never need a receipt.
    assert_eq!(project.submit_from("fastlane test", &project.home), Ok(()));
}

#[test]
fn the_binary_enforces_the_receipt_end_to_end() {
    let project = Project::new();
    project.gate(&project.receipt());
    let run = || {
        let mut child = Command::new(env!("CARGO_BIN_EXE_aaf-hook"))
            .env("AAF_DATA_DIR", &project.data)
            .env("AAF_CONFIG_DIR", project.home.join("config"))
            .env("HOME", &project.home)
            .env_remove("GIT_DIR")
            .env_remove("GIT_INDEX_FILE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let payload = project.payload("bundle exec fastlane deliver", &project.repo);
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    let allowed = run();
    assert_eq!(allowed.status.code(), Some(0), "{allowed:?}");
    assert_eq!(allowed.stdout, b"{}");

    fs::write(project.repo.join("app.txt"), "tampered\n").unwrap();
    let denied = run();
    assert_eq!(denied.status.code(), Some(2), "{denied:?}");
    let stderr = String::from_utf8_lossy(&denied.stderr);
    assert!(stderr.starts_with("store_submit_guard: "), "{stderr}");
    assert!(
        stderr.trim_end().ends_with("fix: aaf gate --release"),
        "{stderr}"
    );
}
