#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Hardening integration tests for pure-`base_sha` warm-slot snapshots,
//! merge/oracle/hydration seeding, blank-PNG detection, host proof retry, and
//! stack-default proof trigger globs.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use aaf_domain::config::{MachineConfig, OracleMode, ProofConfig, RepoConfig};
use aaf_domain::gate::{ChangeKind, GateMode};
use aaf_domain::ports::Clock;
use aaf_engine::config::default_proof_trigger_globs;
use aaf_engine::gate::changes::Change;
use aaf_engine::gate::oracle::{self, OracleVerdict};
use aaf_engine::gate::runner::HeavyContext;
use aaf_engine::gate::stats_repo_id;
use aaf_engine::gc::{GcOptions, run_gc};
use aaf_engine::git::hydration::hydrate_fresh_worktree_with_snaps;
use aaf_engine::git::warm_slot::{
    SlotKind, WarmSlot, base_snapshots_dir, promote_from_pure_base_slot,
    restore_clean_base_snapshot,
};
use aaf_engine::git::{
    GhClient, GitError, MergeStrategy, PrCheck, PrInfo, PrMergeOutcome, PrMergeRequest, merge_pr,
    worktree,
};
use aaf_engine::proof::{ProofError, capture_and_record_proof, is_likely_blank_png, verify_proof};
use aaf_engine::throttle::{SystemProbe, Throttle};
use tempfile::TempDir;

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
        .env("GIT_AUTHOR_NAME", "dev")
        .env("GIT_AUTHOR_EMAIL", "dev@example.invalid")
        .env("GIT_COMMITTER_NAME", "dev")
        .env("GIT_COMMITTER_EMAIL", "dev@example.invalid")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_repo(dir: &Path, config_toml: &str) -> String {
    fs::create_dir_all(dir.join(".autofactory")).unwrap();
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::create_dir_all(dir.join("tests")).unwrap();
    git(dir, &["init", "-b", "main"]);
    fs::write(dir.join(".gitignore"), "target/\n").unwrap();
    fs::write(dir.join(".autofactory/config.toml"), config_toml).unwrap();
    fs::write(dir.join("src/lib.rs"), "pub fn v() -> i32 { 1 }\n").unwrap();
    fs::write(dir.join("tests/basic.rs"), "// test\n").unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-m", "initial"]);
    git(dir, &["rev-parse", "HEAD"])
}

fn commit_config(repo: &Path, toml: &str, msg: &str) -> String {
    fs::write(repo.join(".autofactory/config.toml"), toml).unwrap();
    git(repo, &["add", ".autofactory/config.toml"]);
    git(repo, &["commit", "-m", msg]);
    git(repo, &["rev-parse", "HEAD"])
}

fn acq(repo: &Path, data: &Path, cid: &str, sha: &str, tag: &str) -> WarmSlot {
    WarmSlot::acquire(repo, data, cid, SlotKind::Merge, sha, tag).unwrap()
}

fn crc32(kind: &[u8], data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in kind.iter().chain(data.iter()) {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

fn push_chunk(out: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
    out.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
    out.extend_from_slice(&kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&kind, data).to_be_bytes());
}

fn build_png(width: u32, height: u32, idat: &[u8]) -> Vec<u8> {
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    push_chunk(&mut out, *b"IHDR", &ihdr);
    push_chunk(&mut out, *b"IDAT", idat);
    push_chunk(&mut out, *b"IEND", &[]);
    out
}

fn valid_ui_png() -> Vec<u8> {
    let mut idat = vec![0x78, 0x9C];
    idat.extend((2..9000usize).map(|i| ((i * 37 + 13) % 251) as u8));
    build_png(1080, 1920, &idat)
}

fn blank_solid_png() -> Vec<u8> {
    let mut idat = vec![0x78, 0x9C];
    idat.extend((2..9000usize).map(|i| if i % 2 == 0 { 0x55 } else { 0xAA }));
    build_png(1080, 1920, &idat)
}

fn calm_machine() -> MachineConfig {
    MachineConfig {
        max_load_percent: 0,
        ..Default::default()
    }
}

struct StubGh(PrInfo);
impl GhClient for StubGh {
    fn view_pr(&self, _: &Path, _: u64) -> Result<PrInfo, GitError> {
        Ok(self.0.clone())
    }
    fn pr_checks(&self, _: &Path, _: u64) -> Result<Vec<PrCheck>, GitError> {
        Ok(vec![])
    }
    fn merge_pr(
        &self,
        _: &Path,
        _: u64,
        _: &str,
        _: MergeStrategy,
        _: Option<&str>,
        _: Option<&Path>,
    ) -> Result<(), GitError> {
        Ok(())
    }
}

#[test]
fn promote_from_pure_base_slot_and_restore_enforce_security_invariants() {
    let (repo, data, wts) = (
        TempDir::new().unwrap(),
        TempDir::new().unwrap(),
        TempDir::new().unwrap(),
    );
    let base_sha = init_repo(repo.path(), "[worktree]\nclone_dirs = [\"target\"]\n");
    git(repo.path(), &["checkout", "-b", "feat/pr"]);
    fs::write(repo.path().join("src/lib.rs"), "pub fn v() -> i32 { 2 }\n").unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-m", "pr commit"]);
    let pr_sha = git(repo.path(), &["rev-parse", "HEAD"]);
    git(repo.path(), &["checkout", "main"]);

    let cid = stats_repo_id(repo.path());
    let snaps = base_snapshots_dir(data.path(), &cid);

    // 1. PR-HEAD slot must be refused.
    {
        let pr_slot = acq(repo.path(), data.path(), &cid, &pr_sha, "pr-refuse");
        fs::create_dir_all(pr_slot.path().join("target")).unwrap();
        fs::write(pr_slot.path().join("target/evil.bin"), b"pr").unwrap();
        let err = promote_from_pure_base_slot(&pr_slot, data.path(), &cid, &base_sha, &["target"])
            .unwrap_err();
        assert!(matches!(err, GitError::Invalid(_)), "got {err:?}");
        assert!(!snaps.join(&base_sha).exists());
    }

    // 2. Dirty base slot refused; clean pure base slot promotes cleanly.
    {
        let slot = acq(repo.path(), data.path(), &cid, &base_sha, "pure-base");
        fs::create_dir_all(slot.path().join("target")).unwrap();
        fs::write(slot.path().join("target/warm.bin"), b"pure-base-cache").unwrap();
        fs::write(slot.path().join("src/lib.rs"), "pub fn v() -> i32 { 99 }\n").unwrap();
        let err = promote_from_pure_base_slot(&slot, data.path(), &cid, &base_sha, &["target"])
            .unwrap_err();
        assert!(matches!(err, GitError::Dirty(_)), "got {err:?}");
        git(slot.path(), &["checkout", "--", "src/lib.rs"]);
        promote_from_pure_base_slot(&slot, data.path(), &cid, &base_sha, &["target"]).unwrap();
    }
    assert_eq!(
        fs::read(snaps.join(&base_sha).join("target/warm.bin")).unwrap(),
        b"pure-base-cache"
    );

    // 3. CoW restore into fresh worktree, skip existing/tracked, reject symlinks.
    let wt = worktree::create(repo.path(), wts.path(), "t-restore", "main").unwrap();
    assert_eq!(
        restore_clean_base_snapshot(&wt.path, &snaps, &base_sha, &["target"]).unwrap(),
        1
    );
    assert_eq!(
        fs::read(wt.path.join("target/warm.bin")).unwrap(),
        b"pure-base-cache"
    );
    assert_eq!(
        restore_clean_base_snapshot(&wt.path, &snaps, &base_sha, &["target"]).unwrap(),
        0
    );
    fs::create_dir_all(snaps.join(&base_sha).join("src")).unwrap();
    fs::write(
        snaps.join(&base_sha).join("src/lib.rs"),
        "pub fn v() -> i32 { 777 }\n",
    )
    .unwrap();
    assert_eq!(
        restore_clean_base_snapshot(&wt.path, &snaps, &base_sha, &["src"]).unwrap(),
        0
    );
    assert_eq!(
        fs::read_to_string(wt.path.join("src/lib.rs")).unwrap(),
        "pub fn v() -> i32 { 1 }\n"
    );

    #[cfg(unix)]
    {
        let wt2 = worktree::create(repo.path(), wts.path(), "t-sym", "main").unwrap();
        std::os::unix::fs::symlink("/etc/hosts", snaps.join(&base_sha).join("target/escape"))
            .unwrap();
        assert_eq!(
            restore_clean_base_snapshot(&wt2.path, &snaps, &base_sha, &["target"]).unwrap(),
            0
        );
        assert!(!wt2.path.join("target").exists());
    }

    // 4. GC sweeps expired shared `base-snaps/<cid>/<base_sha>` when unlocked.
    let gc_wts = TempDir::new().unwrap();
    let opts = GcOptions {
        older_than_days: 0,
        ..GcOptions::default()
    };
    let report = run_gc(data.path(), gc_wts.path(), &opts).unwrap();
    assert!(!snaps.join(&base_sha).exists());
    assert!(report.removed_files.iter().any(|p| p.contains(&base_sha)));
}

#[test]
fn merge_oracle_and_hydration_seed_from_shared_base_snapshot() {
    let (bare, repo, data, wts, marker_dir) = (
        TempDir::new().unwrap(),
        TempDir::new().unwrap(),
        TempDir::new().unwrap(),
        TempDir::new().unwrap(),
        TempDir::new().unwrap(),
    );
    let oracle_marker = marker_dir.path().join("oracle-seeded.ok");
    git(bare.path(), &["init", "--bare", "-b", "main"]);
    let cfg_toml = "[project]\nname = \"demo\"\nprofile = \"rust\"\n\
        [commands]\ntest_scoped = \"test -f target/prewarmed.stamp\"\ntest_full = \"true\"\n\
        [gate]\nbase_ref = \"origin/main\"\n\
        [cross_audit]\nrequire_different_provider = false\n\
        [worktree]\nclone_dirs = [\"target\"]\n";
    let base_sha = init_repo(repo.path(), cfg_toml);
    git(
        repo.path(),
        &["remote", "add", "origin", bare.path().to_str().unwrap()],
    );
    git(repo.path(), &["push", "-u", "origin", "main"]);

    let cid = stats_repo_id(repo.path());
    {
        let seed = acq(repo.path(), data.path(), &cid, &base_sha, "seed-shared");
        fs::create_dir_all(seed.path().join("target")).unwrap();
        fs::write(seed.path().join("target/prewarmed.stamp"), b"warm-ok").unwrap();
        promote_from_pure_base_slot(&seed, data.path(), &cid, &base_sha, &["target"]).unwrap();
    }
    assert!(!repo.path().join("target").exists());

    // 1. Hydration falls back to shared base snapshot when main checkout lacks `target/`.
    let wt = worktree::create(repo.path(), wts.path(), "t-hyd-snap", "main").unwrap();
    let hydrated =
        hydrate_fresh_worktree_with_snaps(repo.path(), &wt, &base_sha, data.path(), &cid).unwrap();
    assert_eq!(
        fs::read(wt.path.join("target/prewarmed.stamp")).unwrap(),
        b"warm-ok"
    );
    assert_eq!(hydrated.write_denied, vec![wt.path.join("target")]);

    // 2. Merge ScratchWorktree seeds `target/prewarmed.stamp` before running gate.
    git(repo.path(), &["checkout", "-b", "feat/merge-seed"]);
    fs::write(repo.path().join("src/lib.rs"), "pub fn v() -> i32 { 2 }\n").unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-m", "pr edit"]);
    let pr_sha = git(repo.path(), &["rev-parse", "HEAD"]);
    git(repo.path(), &["push", "-u", "origin", "feat/merge-seed"]);
    git(repo.path(), &["checkout", "main"]);

    let gh = StubGh(PrInfo {
        number: 42,
        state: "OPEN".into(),
        head_ref_oid: pr_sha.clone(),
        head_ref_name: "feat/merge-seed".into(),
        base_ref_name: "main".into(),
        mergeable: "MERGEABLE".into(),
        is_draft: false,
        title: "PR #42".into(),
        url: "https://example.invalid/pr/42".into(),
        ..Default::default()
    });
    let noop = |_: &aaf_engine::throttle::Progress<'_>| {};
    let req = PrMergeRequest {
        repo_root: repo.path().to_path_buf(),
        data_dir: data.path().to_path_buf(),
        pr_number: 42,
        expect_head: pr_sha,
        remote: "origin".into(),
        mode: GateMode::Scoped,
        strategy: MergeStrategy::Squash,
        subject: None,
        body_file: None,
        ci_wait_secs: 0,
        require_checks: false,
        lock_timeout: Duration::from_secs(10),
        dry_run: true,
        machine: calm_machine(),
        global_config: None,
    };
    let merge_out = merge_pr(&req, &gh, &FixedClock(100), &noop).unwrap();
    assert!(
        matches!(merge_out, PrMergeOutcome::DryRunClear { .. }),
        "{merge_out:?}"
    );

    // 3. Oracle ScratchWorktree seeds `target/prewarmed.stamp` on reverted tree.
    git(repo.path(), &["checkout", "feat/merge-seed"]);
    fs::write(repo.path().join("tests/basic.rs"), "// updated test\n").unwrap();
    let mut oracle_cfg = RepoConfig::default();
    oracle_cfg.gate.red_green_oracle = OracleMode::Block;
    oracle_cfg.commands.test_scoped = format!(
        "test -f target/prewarmed.stamp && touch \"{}\" && \
         (! grep -q \"updated test\" tests/basic.rs || grep -q \"{{ 2 }}\" src/lib.rs || {{ echo \"test result: FAILED. 0 passed; 1 failed;\"; exit 1; }})",
        oracle_marker.display()
    );
    let (machine, probe, clock) = (calm_machine(), SystemProbe::detect(), FixedClock(101));
    let throttle = Throttle::system(&machine, &probe, &noop);
    let heavy = HeavyContext {
        data_dir: data.path(),
        stats_repo_id: &cid,
        clock: &clock,
        throttle: &throttle,
    };
    let changes = vec![
        Change {
            kind: ChangeKind::Modified,
            path: "src/lib.rs".into(),
            old_path: None,
        },
        Change {
            kind: ChangeKind::Modified,
            path: "tests/basic.rs".into(),
            old_path: None,
        },
    ];
    let verdict = oracle::evaluate(
        repo.path(),
        &base_sha,
        &oracle_cfg,
        GateMode::Scoped,
        &changes,
        &[],
        &heavy,
    )
    .unwrap();
    assert!(oracle_marker.exists(), "oracle slot must be pre-warmed");
    assert!(
        matches!(verdict, OracleVerdict::Applied { status: None, ref reason } if reason.contains("red_green=proven")),
        "got {verdict:?}"
    );
}

#[test]
fn blank_png_detection_and_capture_auto_retry_recover_cleanly() {
    let (valid_ui, solid_large) = (valid_ui_png(), blank_solid_png());
    let tiny_idat = build_png(1080, 1920, &[0x78, 0x9C, 1, 2, 3, 4, 5, 6, 7, 8]);
    assert!(!is_likely_blank_png(&valid_ui));
    assert!(is_likely_blank_png(&solid_large));
    assert!(is_likely_blank_png(&tiny_idat));
    assert!(!is_likely_blank_png(b"not a png"));

    let tmp = TempDir::new().unwrap();
    let (repo, data) = (tmp.path().join("repo"), tmp.path().join("data"));
    let (good_png, blank_png) = (tmp.path().join("good.png"), tmp.path().join("blank.png"));
    let (c_blank, c_exit) = (
        tmp.path().join("c-blank.txt"),
        tmp.path().join("c-exit.txt"),
    );
    fs::write(&good_png, &valid_ui).unwrap();
    fs::write(&blank_png, &solid_large).unwrap();
    let cfg = ProofConfig {
        required: true,
        ..ProofConfig::default()
    };
    let clock = FixedClock(500);

    // Case 1: Blank PNG on attempt 1 -> valid UI PNG on attempt 2 recovers.
    let retry_blank = format!(
        "n=0; [ -f '{c}' ] && n=$(cat '{c}'); n=$((n + 1)); printf '%s' \"$n\" > '{c}'; \
         if [ \"$n\" -eq 1 ]; then cp '{b}' \"$AAF_PROOF_OUT\"; else cp '{g}' \"$AAF_PROOF_OUT\"; fi",
        c = c_blank.display(),
        b = blank_png.display(),
        g = good_png.display()
    );
    let sha1 = init_repo(
        &repo,
        &format!("[proof]\nrequired = true\ncapture_cmd = {retry_blank:?}\n"),
    );
    let fp1 = aaf_fp::tree_fp(&repo, aaf_fp::GIT_TIMEOUT).unwrap();
    capture_and_record_proof(&repo, &data, &sha1, &cfg, &fp1, &clock).unwrap();
    assert_eq!(fs::read_to_string(&c_blank).unwrap().trim(), "2");
    assert!(verify_proof(&repo, &data, &cfg, Some(&fp1)).is_ok());

    // Case 2: Non-zero exit on attempt 1 -> valid UI PNG on attempt 2 recovers.
    let retry_exit = format!(
        "n=0; [ -f '{c}' ] && n=$(cat '{c}'); n=$((n + 1)); printf '%s' \"$n\" > '{c}'; \
         if [ \"$n\" -eq 1 ]; then exit 1; else cp '{g}' \"$AAF_PROOF_OUT\"; fi",
        c = c_exit.display(),
        g = good_png.display()
    );
    let sha2 = commit_config(
        &repo,
        &format!("[proof]\nrequired = true\ncapture_cmd = {retry_exit:?}\n"),
        "retry exit",
    );
    let fp2 = aaf_fp::tree_fp(&repo, aaf_fp::GIT_TIMEOUT).unwrap();
    capture_and_record_proof(&repo, &data, &sha2, &cfg, &fp2, &clock).unwrap();
    assert_eq!(fs::read_to_string(&c_exit).unwrap().trim(), "2");

    // Case 3: Persistent blank PNG returns explicit blank/solid color error.
    let always_blank = format!("cp '{}' \"$AAF_PROOF_OUT\"", blank_png.display());
    let sha3 = commit_config(
        &repo,
        &format!("[proof]\nrequired = true\ncapture_cmd = {always_blank:?}\n"),
        "always blank",
    );
    let fp3 = aaf_fp::tree_fp(&repo, aaf_fp::GIT_TIMEOUT).unwrap();
    let err = capture_and_record_proof(&repo, &data, &sha3, &cfg, &fp3, &clock).unwrap_err();
    assert!(
        matches!(err, ProofError::Invalid(ref m) if m.contains("blank/solid color")),
        "{err:?}"
    );
}

#[test]
fn default_proof_trigger_globs_cover_ui_stacks_and_skip_non_ui() {
    let android = default_proof_trigger_globs("android");
    assert!(android.contains(&"res/layout/**".to_string()));
    assert!(android.contains(&"**/ui/**".to_string()));
    assert!(android.contains(&"**/*.kt".to_string()));
    let ios = default_proof_trigger_globs("ios");
    assert!(ios.contains(&"**/*.swift".to_string()));
    assert!(ios.contains(&"**/*.storyboard".to_string()));
    assert_eq!(default_proof_trigger_globs("flutter"), vec!["lib/**"]);
    assert_eq!(
        default_proof_trigger_globs("react-native"),
        vec!["src/**", "app/**"]
    );
    assert_eq!(
        default_proof_trigger_globs("expo"),
        vec!["src/**", "app/**"]
    );
    assert_eq!(
        default_proof_trigger_globs("web"),
        vec!["src/**", "app/**", "pages/**"]
    );
    assert!(default_proof_trigger_globs("rust").is_empty());
    assert!(default_proof_trigger_globs("backend").is_empty());
    assert!(default_proof_trigger_globs("auto").is_empty());
}
