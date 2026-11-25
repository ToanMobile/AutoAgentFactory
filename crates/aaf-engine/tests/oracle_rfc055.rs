#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Oracle tests for RFC-055:
//! 1. `[proof].capture_lock_globs` loaded strictly from `base_sha:.autofactory/config.toml`
//!    and enforced in `head_lock` as `LockTier::Hard` before `capture_cmd` runs.
//! 2. All `AAF_*` environment variables scrubbed from `capture_cmd` child env
//!    before setting explicit `AAF_PROOF_OUT`.

use std::fs;
use std::path::Path;
use std::process::Command;

use aaf_domain::config::{MachineConfig, ProofConfig, RepoConfig};
use aaf_domain::gate::{GateMode, GateStatus, LockStatus, LockTier};
use aaf_domain::ports::Clock;
use aaf_engine::config::load_from_env;
use aaf_engine::gate::{head_lock, run_gate_with};
use aaf_engine::proof::{capture_and_record_proof, verify_proof};
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

fn valid_ui_png() -> Vec<u8> {
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&1080u32.to_be_bytes());
    ihdr.extend_from_slice(&1920u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    push_chunk(&mut out, *b"IHDR", &ihdr);
    let mut idat = vec![0x78, 0x9C];
    idat.extend((2..9000usize).map(|i| ((i * 37 + 13) % 251) as u8));
    push_chunk(&mut out, *b"IDAT", &idat);
    push_chunk(&mut out, *b"IEND", &[]);
    out
}

fn calm_machine() -> MachineConfig {
    MachineConfig {
        max_load_percent: 0,
        ..Default::default()
    }
}

fn write_executable(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[test]
fn capture_lock_globs_from_base_sha_blocks_capture_script_as_hard_lock_before_capture_runs() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let data = tmp.path().join("data");
    let fixture_png = tmp.path().join("fixture.png");
    let capture_marker = tmp.path().join("capture-ran.log");
    fs::write(&fixture_png, valid_ui_png()).unwrap();

    fs::create_dir_all(repo.join(".autofactory/scripts")).unwrap();
    fs::create_dir_all(repo.join("src/ui")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);

    write_executable(
        &repo.join("scripts/shot.sh"),
        &format!(
            "#!/bin/sh\nset -e\nprintf 'ran' > '{}'\ncp '{}' \"$AAF_PROOF_OUT\"\n",
            capture_marker.display(),
            fixture_png.display()
        ),
    );
    write_executable(
        &repo.join(".autofactory/scripts/helper.sh"),
        "#!/bin/sh\necho helper\n",
    );
    fs::write(repo.join("src/ui/App.tsx"), "export const App = () => 1;\n").unwrap();
    fs::write(
        repo.join(".autofactory/config.toml"),
        "[commands]\ntest_scoped = \"true\"\n\n\
         [proof]\nrequired = true\n\
         capture_cmd = \"./scripts/shot.sh\"\n\
         trigger_globs = [\"src/ui/**\"]\n\
         capture_lock_globs = [\"scripts/shot.sh\", \".autofactory/scripts/**\"]\n",
    )
    .unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    let base_sha = git(&repo, &["rev-parse", "HEAD"]);

    let clock = FixedClock(1000);
    let machine = calm_machine();

    // (a1) PR modifies scripts/shot.sh + src/ui/App.tsx -> blocked at HeadLock (Hard), capture_cmd never runs.
    git(&repo, &["checkout", "-q", "-b", "feat/tamper-shot"]);
    write_executable(
        &repo.join("scripts/shot.sh"),
        &format!(
            "#!/bin/sh\nprintf 'pwned' > '{}'\ncp '{}' \"$AAF_PROOF_OUT\"\n",
            capture_marker.display(),
            fixture_png.display()
        ),
    );
    fs::write(repo.join("src/ui/App.tsx"), "export const App = () => 2;\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "tamper shot script"]);

    let cfg = load_from_env(&repo).unwrap().repo;
    let receipt = run_gate_with(&data, &repo, &cfg, GateMode::Scoped, &clock, &machine).unwrap();
    assert_eq!(receipt.status, GateStatus::Blocked, "{:?}", receipt.reasons);
    assert_eq!(receipt.head_lock.status, LockStatus::Blocked);
    let shot_lock = receipt
        .head_lock
        .locked_changes
        .iter()
        .find(|c| c.path == "scripts/shot.sh")
        .expect("scripts/shot.sh must be head-locked");
    assert_eq!(shot_lock.tier, LockTier::Hard);
    assert!(
        !capture_marker.exists(),
        "capture_cmd must NOT execute when capture_lock_globs triggers HeadLock"
    );

    // Direct head_lock::evaluate matches gate receipt and refuses same_account=true approval.
    let eval = head_lock::evaluate(&data, &repo, &cfg, &base_sha);
    assert_eq!(eval.status, LockStatus::Blocked);
    assert_eq!(eval.lock_fp, receipt.head_lock.lock_fp);
    assert!(
        eval.locked_changes
            .iter()
            .any(|c| c.path == "scripts/shot.sh" && c.tier == LockTier::Hard)
    );
    head_lock::approve(&data, &repo, &cfg, "self-agent", true, &clock).unwrap();
    let after_same = head_lock::evaluate(&data, &repo, &cfg, &base_sha);
    assert_eq!(
        after_same.status,
        LockStatus::Blocked,
        "LockTier::Hard capture_lock_globs must reject same_account=true approval"
    );

    // (a2) PR edits .autofactory/config.toml to delete capture_lock_globs AND modifies scripts/shot.sh.
    // Still read strictly from base_sha, so scripts/shot.sh remains locked as LockTier::Hard!
    git(&repo, &["checkout", "-q", "main"]);
    git(&repo, &["checkout", "-q", "-b", "feat/drop-lock-globs"]);
    fs::write(
        repo.join(".autofactory/config.toml"),
        "[commands]\ntest_scoped = \"true\"\n\n\
         [proof]\nrequired = true\n\
         capture_cmd = \"./scripts/shot.sh\"\n\
         trigger_globs = [\"src/ui/**\"]\n",
    )
    .unwrap();
    write_executable(
        &repo.join("scripts/shot.sh"),
        &format!(
            "#!/bin/sh\nprintf 'pwned2' > '{}'\ncp '{}' \"$AAF_PROOF_OUT\"\n",
            capture_marker.display(),
            fixture_png.display()
        ),
    );
    fs::write(repo.join("src/ui/App.tsx"), "export const App = () => 3;\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "drop capture_lock_globs"]);

    let mut tampered_cfg = load_from_env(&repo).unwrap().repo;
    // Even if caller passed empty head_lock_globs, base_sha capture_lock_globs still enforces Hard lock.
    tampered_cfg.gate.head_lock_globs.clear();
    let tampered_receipt = run_gate_with(
        &data,
        &repo,
        &tampered_cfg,
        GateMode::Scoped,
        &clock,
        &machine,
    )
    .unwrap();
    assert_eq!(tampered_receipt.status, GateStatus::Blocked);
    assert_eq!(tampered_receipt.head_lock.status, LockStatus::Blocked);
    assert!(
        tampered_receipt
            .head_lock
            .locked_changes
            .iter()
            .any(|c| c.path == "scripts/shot.sh" && c.tier == LockTier::Hard),
        "scripts/shot.sh must stay Hard-locked from base_sha even after PR deletes capture_lock_globs: {:?}",
        tampered_receipt.head_lock.locked_changes
    );
    assert!(
        !capture_marker.exists(),
        "capture_cmd must NOT execute when PR drops capture_lock_globs and edits script"
    );

    // (a3) Adding a brand-new file under `.autofactory/scripts/**` (even without touching UI) is Hard-locked.
    git(&repo, &["checkout", "-q", "main"]);
    git(&repo, &["checkout", "-q", "-b", "feat/add-helper"]);
    write_executable(
        &repo.join(".autofactory/scripts/extra.sh"),
        "#!/bin/sh\necho new\n",
    );
    let added_cfg = load_from_env(&repo).unwrap().repo;
    let added_receipt =
        run_gate_with(&data, &repo, &added_cfg, GateMode::Scoped, &clock, &machine).unwrap();
    assert_eq!(added_receipt.status, GateStatus::Blocked);
    assert!(
        added_receipt
            .head_lock
            .locked_changes
            .iter()
            .any(|c| c.path == ".autofactory/scripts/extra.sh" && c.tier == LockTier::Hard),
        "newly added file matching capture_lock_globs must be Hard-locked: {:?}",
        added_receipt.head_lock.locked_changes
    );
    fs::remove_file(repo.join(".autofactory/scripts/extra.sh")).unwrap();

    // (a4) Clean UI-only edit does NOT trigger head-lock; capture_cmd runs and gate clears.
    git(&repo, &["checkout", "-q", "main"]);
    git(&repo, &["checkout", "-q", "-b", "feat/clean-ui"]);
    fs::write(
        repo.join("src/ui/App.tsx"),
        "export const App = () => 42;\n",
    )
    .unwrap();
    let clean_cfg = load_from_env(&repo).unwrap().repo;
    let clean_receipt =
        run_gate_with(&data, &repo, &clean_cfg, GateMode::Scoped, &clock, &machine).unwrap();
    assert_eq!(
        clean_receipt.status,
        GateStatus::Clear,
        "{:?}",
        clean_receipt.reasons
    );
    assert_eq!(clean_receipt.head_lock.status, LockStatus::Clear);
    assert!(
        capture_marker.exists(),
        "capture_cmd must run on clean UI PR"
    );
}

#[test]
fn capture_lock_globs_never_downgrades_test_globs_to_soft_and_fails_closed_on_symlink_config() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let data = tmp.path().join("data");
    fs::create_dir_all(repo.join(".autofactory")).unwrap();
    fs::create_dir_all(repo.join("tests")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("tests/shot_test.sh"), "#!/bin/sh\necho ok\n").unwrap();
    fs::write(
        repo.join(".autofactory/config.toml"),
        "[commands]\ntest_scoped = \"true\"\n\n\
         [proof]\ncapture_lock_globs = [\"**/tests/**\"]\n",
    )
    .unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    let base_sha = git(&repo, &["rev-parse", "HEAD"]);

    git(&repo, &["checkout", "-q", "-b", "feat/test-capture"]);
    fs::write(repo.join("tests/shot_test.sh"), "#!/bin/sh\necho changed\n").unwrap();
    fs::write(repo.join("tests/new_helper.sh"), "#!/bin/sh\necho added\n").unwrap();

    let cfg = load_from_env(&repo).unwrap().repo;
    let eval = head_lock::evaluate(&data, &repo, &cfg, &base_sha);
    assert_eq!(eval.status, LockStatus::Blocked);
    for expected in ["tests/shot_test.sh", "tests/new_helper.sh"] {
        let found = eval
            .locked_changes
            .iter()
            .find(|c| c.path == expected)
            .unwrap_or_else(|| panic!("{expected} must be locked: {:?}", eval.locked_changes));
        assert_eq!(
            found.tier,
            LockTier::Hard,
            "{expected} must NEVER downgrade to LockTier::Soft"
        );
    }

    #[cfg(unix)]
    {
        let sym_repo = tmp.path().join("sym-repo");
        fs::create_dir_all(sym_repo.join(".autofactory")).unwrap();
        git(&sym_repo, &["init", "-q", "-b", "main"]);
        fs::write(
            sym_repo.join("real.toml"),
            "[commands]\ntest_scoped = \"true\"\n",
        )
        .unwrap();
        std::os::unix::fs::symlink("../real.toml", sym_repo.join(".autofactory/config.toml"))
            .unwrap();
        git(&sym_repo, &["add", "-A"]);
        git(&sym_repo, &["commit", "-q", "-m", "symlink config"]);
        let sym_sha = git(&sym_repo, &["rev-parse", "HEAD"]);
        let sym_eval = head_lock::evaluate(&data, &sym_repo, &RepoConfig::default(), &sym_sha);
        assert_eq!(
            sym_eval.status,
            LockStatus::Blocked,
            "symlink .autofactory/config.toml at base_sha must fail closed"
        );
    }
}

#[test]
fn proof_capture_scrubs_all_aaf_env_vars_while_preserving_aaf_proof_out() {
    if std::env::var_os("RFC055_SCRUB_CHILD").is_none() {
        let exe = std::env::current_exe().expect("current test binary");
        let out = Command::new(exe)
            .args([
                "--exact",
                "proof_capture_scrubs_all_aaf_env_vars_while_preserving_aaf_proof_out",
                "--nocapture",
            ])
            .env("RFC055_SCRUB_CHILD", "1")
            .env("RFC055_BENIGN_VAR", "keep-this-value")
            .env("AAF_UI_TOKEN", "super-secret-ui")
            .env("AAF_TELEGRAM_BOT_TOKEN", "super-secret-tg")
            .env("AAF_CONFIG_DIR", "/tmp/secret-aaf-cfg")
            .env("AAF_RUN_ID", "secret-run-42")
            .env("AAF_DATA_DIR", "/tmp/secret-aaf-data")
            .env("aaf_lowercase_secret", "super-secret-lower")
            .output()
            .expect("spawn child test");
        assert!(
            out.status.success(),
            "child test failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        return;
    }

    // Confirm secrets are present in the host process environment before capture runs.
    assert_eq!(std::env::var("AAF_UI_TOKEN").unwrap(), "super-secret-ui");
    assert_eq!(
        std::env::var("AAF_TELEGRAM_BOT_TOKEN").unwrap(),
        "super-secret-tg"
    );

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let data = tmp.path().join("data");
    let fixture_png = tmp.path().join("fixture.png");
    let env_dump = tmp.path().join("capture-env.txt");
    fs::write(&fixture_png, valid_ui_png()).unwrap();

    fs::create_dir_all(repo.join(".autofactory")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    let capture_cmd = format!(
        "env > '{}' && cp '{}' \"$AAF_PROOF_OUT\"",
        env_dump.display(),
        fixture_png.display()
    );
    fs::write(
        repo.join(".autofactory/config.toml"),
        format!("[proof]\nrequired = true\ncapture_cmd = {capture_cmd:?}\n"),
    )
    .unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    let base_sha = git(&repo, &["rev-parse", "HEAD"]);

    let fp = aaf_fp::tree_fp(&repo, aaf_fp::GIT_TIMEOUT).unwrap();
    let cfg = ProofConfig {
        required: true,
        ..ProofConfig::default()
    };
    let clock = FixedClock(2000);
    capture_and_record_proof(&repo, &data, &base_sha, &cfg, &fp, &clock).unwrap();
    assert!(verify_proof(&repo, &data, &cfg, Some(&fp)).is_ok());

    let dumped = fs::read_to_string(&env_dump).unwrap();
    for forbidden in [
        "super-secret-ui",
        "super-secret-tg",
        "/tmp/secret-aaf-cfg",
        "secret-run-42",
        "/tmp/secret-aaf-data",
        "super-secret-lower",
        "AAF_UI_TOKEN=",
        "AAF_TELEGRAM_BOT_TOKEN=",
        "AAF_CONFIG_DIR=",
        "AAF_RUN_ID=",
        "AAF_DATA_DIR=",
        "aaf_lowercase_secret=",
    ] {
        assert!(
            !dumped.contains(forbidden),
            "capture_cmd environment leaked {forbidden:?}"
        );
    }
    let proof_out_line = dumped
        .lines()
        .find(|line| line.starts_with("AAF_PROOF_OUT="))
        .expect("AAF_PROOF_OUT must be explicitly provided to capture_cmd");
    assert!(
        !proof_out_line
            .strip_prefix("AAF_PROOF_OUT=")
            .unwrap()
            .is_empty(),
        "AAF_PROOF_OUT must be non-empty"
    );
    assert!(
        dumped.contains("RFC055_BENIGN_VAR=keep-this-value"),
        "non-AAF env vars should remain intact"
    );
}

#[test]
fn gitignored_files_matching_capture_lock_globs_are_hard_locked_before_capture_runs() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let data = tmp.path().join("data");
    let fixture_png = tmp.path().join("fixture.png");
    let capture_marker = tmp.path().join("capture-ran.log");
    fs::write(&fixture_png, valid_ui_png()).unwrap();

    fs::create_dir_all(repo.join(".autofactory")).unwrap();
    fs::create_dir_all(repo.join("src/ui")).unwrap();
    fs::create_dir_all(repo.join("e2e")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);

    write_executable(
        &repo.join("scripts/shot.sh"),
        &format!(
            "#!/bin/sh\nset -e\nprintf 'ran' > '{}'\ncp '{}' \"$AAF_PROOF_OUT\"\n",
            capture_marker.display(),
            fixture_png.display()
        ),
    );
    fs::write(repo.join(".gitignore"), "e2e/*.local.ts\ntarget/\n").unwrap();
    fs::write(repo.join("e2e/base.spec.ts"), "export const ok = true;\n").unwrap();
    fs::write(repo.join("src/ui/App.tsx"), "export const App = () => 1;\n").unwrap();
    // Includes a leading `/` to verify normalize_capture_lock_globs strips `/` as well as `./`.
    fs::write(
        repo.join(".autofactory/config.toml"),
        "[commands]\ntest_scoped = \"true\"\n\n\
         [proof]\nrequired = true\n\
         capture_cmd = \"./scripts/shot.sh\"\n\
         trigger_globs = [\"src/ui/**\"]\n\
         capture_lock_globs = [\"/e2e/**\", \"/scripts/shot.sh\"]\n",
    )
    .unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    let base_sha = git(&repo, &["rev-parse", "HEAD"]);

    let clock = FixedClock(3000);
    let machine = calm_machine();

    // Case 1: Pre-existing .gitignore rule hides e2e/bypass.local.ts -> MUST Hard-lock before capture_cmd!
    git(
        &repo,
        &["checkout", "-q", "-b", "feat/ignored-capture-bypass"],
    );
    fs::write(repo.join("src/ui/App.tsx"), "export const App = () => 2;\n").unwrap();
    fs::write(
        repo.join("e2e/bypass.local.ts"),
        "export const fake = true;\n",
    )
    .unwrap();
    fs::create_dir_all(repo.join("target")).unwrap();
    fs::write(repo.join("target/benign.txt"), "build artifact\n").unwrap();

    let cfg = load_from_env(&repo).unwrap().repo;
    let receipt = run_gate_with(&data, &repo, &cfg, GateMode::Scoped, &clock, &machine).unwrap();
    assert_eq!(receipt.status, GateStatus::Blocked, "{:?}", receipt.reasons);
    assert_eq!(receipt.head_lock.status, LockStatus::Blocked);
    assert!(
        receipt
            .head_lock
            .locked_changes
            .iter()
            .any(|c| c.path == "e2e/bypass.local.ts" && c.tier == LockTier::Hard),
        "gitignored file matching capture_lock_globs must be LockTier::Hard: {:?}",
        receipt.head_lock.locked_changes
    );
    assert!(
        !receipt
            .head_lock
            .locked_changes
            .iter()
            .any(|c| c.path.starts_with("target/")),
        "unrelated gitignored artifacts must not trigger capture_lock_globs"
    );
    assert!(
        !capture_marker.exists(),
        "capture_cmd must NOT execute when gitignored file violates capture_lock_globs"
    );
    let eval = head_lock::evaluate(&data, &repo, &cfg, &base_sha);
    assert_eq!(eval.status, LockStatus::Blocked);
    assert_eq!(eval.lock_fp, receipt.head_lock.lock_fp);

    // Case 2: PR edits .gitignore to ignore a new e2e/zz.spec.ts -> MUST Hard-lock too!
    fs::remove_file(repo.join("e2e/bypass.local.ts")).unwrap();
    fs::write(
        repo.join(".gitignore"),
        "e2e/*.local.ts\ne2e/zz.spec.ts\ntarget/\n",
    )
    .unwrap();
    fs::write(repo.join("e2e/zz.spec.ts"), "export const evil = true;\n").unwrap();
    let receipt2 = run_gate_with(&data, &repo, &cfg, GateMode::Scoped, &clock, &machine).unwrap();
    assert_eq!(receipt2.status, GateStatus::Blocked);
    assert_eq!(receipt2.head_lock.status, LockStatus::Blocked);
    assert!(
        receipt2
            .head_lock
            .locked_changes
            .iter()
            .any(|c| c.path == "e2e/zz.spec.ts" && c.tier == LockTier::Hard),
        "PR-added .gitignore entry must not hide e2e/zz.spec.ts: {:?}",
        receipt2.head_lock.locked_changes
    );
    assert!(
        !capture_marker.exists(),
        "capture_cmd must NOT execute when PR hides e2e/zz.spec.ts via .gitignore"
    );

    // Case 3: Remove ignored e2e/zz.spec.ts (keeping benign target/benign.txt) -> gate clears!
    fs::remove_file(repo.join("e2e/zz.spec.ts")).unwrap();
    let clean = run_gate_with(&data, &repo, &cfg, GateMode::Scoped, &clock, &machine).unwrap();
    assert_eq!(clean.status, GateStatus::Clear, "{:?}", clean.reasons);
    assert_eq!(clean.head_lock.status, LockStatus::Clear);
    assert!(
        capture_marker.exists(),
        "capture_cmd should run once violating ignored files are removed"
    );
}
