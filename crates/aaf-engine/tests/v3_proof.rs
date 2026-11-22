#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Pillar 3: mobile-mcp (`mobile-mcp:mobile_save_screenshot`) proof verification and HMAC-signed binding.

use std::path::Path;
use std::process::Command;

use aaf_domain::config::ProofConfig;
use aaf_domain::gate::{
    GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION, Receipt,
    StoreComplianceReport,
};
use aaf_engine::git::TaskWorktree;
use aaf_engine::git::pr::{PrRequest, pr_body_with_proof};
use aaf_engine::proof::{ProofError, ProofInput, record_proof, show_proof, verify_proof};

fn init_git_repo(dir: &Path) {
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["config", "user.email", "test@example.com"][..],
        &["config", "user.name", "Tester"][..],
    ] {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}");
    }
    std::fs::write(dir.join("README.md"), "hello\n").unwrap();
    for args in [&["add", "."][..], &["commit", "-q", "-m", "init"][..]] {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}");
    }
}

fn crc32(kind: &[u8], data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in kind.iter().chain(data.iter()) {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn push_chunk(out: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
    let len = u32::try_from(data.len()).unwrap();
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&kind, data).to_be_bytes());
}

fn build_png(width: u32, height: u32, min_total_bytes: usize) -> Vec<u8> {
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    push_chunk(&mut out, *b"IHDR", &ihdr);
    let pad_len = min_total_bytes.saturating_sub(out.len() + 24).max(32);
    let mut idat = Vec::with_capacity(pad_len);
    idat.extend_from_slice(&[0x78, 0x9C]);
    for i in 2..pad_len {
        idat.push(((i * 37 + 13) % 251) as u8);
    }
    push_chunk(&mut out, *b"IDAT", &idat);
    push_chunk(&mut out, *b"IEND", &[]);
    out
}

fn proof_input(image_path: &Path, tool: &str, device: &str, now: i64) -> ProofInput {
    ProofInput {
        image_path: image_path.to_path_buf(),
        source_tool: tool.to_string(),
        device_serial: device.to_string(),
        task_id: None,
        now,
    }
}

struct FixedClock(i64);
impl aaf_domain::ports::Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

#[test]
fn valid_png_records_verifies_and_embeds_into_pr_body() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);

    let img = tmp.path().join("screen.png");
    std::fs::write(&img, build_png(1080, 2400, 9000)).unwrap();

    let cfg = ProofConfig::default();
    let mut inp = proof_input(
        &img,
        "mobile-mcp:mobile_save_screenshot",
        "emulator-5554",
        12345,
    );
    inp.task_id = Some("t-1".to_string());
    let recorded = record_proof(&repo, &data, &cfg, &inp).unwrap();
    assert_eq!(recorded.record.width, 1080);
    assert_eq!(recorded.record.height, 2400);

    let verified = verify_proof(&repo, &data, &cfg, None).unwrap();
    assert_eq!(verified.image_sha256, recorded.record.image_sha256);
    assert_eq!(
        show_proof(&repo, &data, None).unwrap().device_serial,
        "emulator-5554"
    );

    let receipt = Receipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        repo_root: repo.to_string_lossy().into_owned(),
        tree_fp: verified.tree_fp.clone(),
        head_sha: None,
        base_sha: None,
        mode: GateMode::Scoped,
        status: GateStatus::Clear,
        reasons: vec![],
        config_hash: "cfg".to_string(),
        commands: vec![],
        static_findings: vec![],
        suppressions: 0,
        head_lock: HeadLockReport {
            status: LockStatus::Clear,
            lock_fp: String::new(),
            locked_changes: vec![],
            approval: None,
        },
        store_compliance: StoreComplianceReport::not_applicable(),
        created_at: 100,
    };
    let req = PrRequest {
        repo_root: repo.clone(),
        worktree: TaskWorktree {
            path: repo.clone(),
            branch: "aaf/t-1".to_string(),
        },
        base: "main".to_string(),
        remote: "origin".to_string(),
        title: "Add home badge".to_string(),
        receipt,
    };
    let body = pr_body_with_proof(&req, Some(&verified));
    assert!(body.contains("mobile-mcp:mobile_save_screenshot"));
    assert!(body.contains("emulator-5554"));
    assert!(body.contains("1080x2400"));

    // Dirtying the working tree invalidates tree_fp verification!
    std::fs::write(repo.join("README.md"), "changed\n").unwrap();
    assert!(verify_proof(&repo, &data, &cfg, None).is_err());
}

#[test]
fn rejects_tiny_corrupt_bad_crc_nonempty_iend_uniform_idat_and_small_dims() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);
    let cfg = ProofConfig::default();
    let tool = "mobile-mcp:mobile_save_screenshot";

    // 1. Below min_bytes
    let tiny = tmp.path().join("tiny.png");
    std::fs::write(&tiny, build_png(120, 120, 200)).unwrap();
    assert!(
        record_proof(
            &repo,
            &data,
            &cfg,
            &proof_input(&tiny, tool, "emulator-5554", 1)
        )
        .is_err()
    );

    // 2. Non-PNG bytes
    let corrupt = tmp.path().join("corrupt.png");
    std::fs::write(&corrupt, vec![b'X'; 10_000]).unwrap();
    assert!(
        record_proof(
            &repo,
            &data,
            &cfg,
            &proof_input(&corrupt, tool, "emulator-5554", 1)
        )
        .is_err()
    );

    // 3. Valid >= 9000B PNG but dimensions 50x50 below min_width/min_height (100x100)
    let small_dims = tmp.path().join("small-dims.png");
    std::fs::write(&small_dims, build_png(50, 50, 9000)).unwrap();
    assert!(
        record_proof(
            &repo,
            &data,
            &cfg,
            &proof_input(&small_dims, tool, "emulator-5554", 1)
        )
        .is_err()
    );

    // 4. Bad CRC-32
    let mut bad_crc = build_png(1080, 1920, 9000);
    let last = bad_crc.len() - 1;
    bad_crc[last] ^= 0xFF;
    let bad_crc_path = tmp.path().join("bad-crc.png");
    std::fs::write(&bad_crc_path, bad_crc).unwrap();
    assert!(
        record_proof(
            &repo,
            &data,
            &cfg,
            &proof_input(&bad_crc_path, tool, "emulator-5554", 1)
        )
        .is_err()
    );

    // 5. Non-empty IEND (even with valid CRC)
    let mut nonempty_iend = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&1080u32.to_be_bytes());
    ihdr.extend_from_slice(&1920u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    push_chunk(&mut nonempty_iend, *b"IHDR", &ihdr);
    let mut diverse = vec![0x78, 0x9C];
    for i in 2..9000usize {
        diverse.push(((i * 37 + 13) % 251) as u8);
    }
    push_chunk(&mut nonempty_iend, *b"IDAT", &diverse);
    push_chunk(&mut nonempty_iend, *b"IEND", &[1, 2, 3, 4]);
    let nonempty_iend_path = tmp.path().join("nonempty-iend.png");
    std::fs::write(&nonempty_iend_path, nonempty_iend).unwrap();
    assert!(
        record_proof(
            &repo,
            &data,
            &cfg,
            &proof_input(&nonempty_iend_path, tool, "emulator-5554", 1)
        )
        .is_err()
    );

    // 6. Uniform/blank IDAT (valid CRC + valid zlib header, but < 16 distinct bytes)
    let mut uniform = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    push_chunk(&mut uniform, *b"IHDR", &ihdr);
    let mut blank_idat = vec![0x78, 0x9C];
    blank_idat.resize(9000, 0x00);
    push_chunk(&mut uniform, *b"IDAT", &blank_idat);
    push_chunk(&mut uniform, *b"IEND", &[]);
    let uniform_path = tmp.path().join("uniform.png");
    std::fs::write(&uniform_path, uniform).unwrap();
    assert!(
        record_proof(
            &repo,
            &data,
            &cfg,
            &proof_input(&uniform_path, tool, "emulator-5554", 1)
        )
        .is_err()
    );
}

#[test]
fn rejects_denylisted_unallowlisted_empty_tools_symlink_and_hmac_tamper() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);

    let cfg = ProofConfig::default();
    let tool = "mobile-mcp:mobile_save_screenshot";
    let good = tmp.path().join("good.png");
    std::fs::write(&good, build_png(1080, 1920, 9000)).unwrap();

    // Case-insensitive .adb-denylist
    std::fs::write(
        repo.join(".adb-denylist"),
        "# production phone\nR58M123456\n",
    )
    .unwrap();
    for serial in ["R58M123456", "r58m123456"] {
        let err =
            record_proof(&repo, &data, &cfg, &proof_input(&good, tool, serial, 1)).unwrap_err();
        assert!(matches!(err, ProofError::DeniedDevice(_)), "{err}");
    }

    // Case-insensitive .adb-allowlist
    std::fs::write(
        repo.join(".adb-allowlist"),
        "# approved simulators\nemulator-5554, iPhone-16-Sim\n",
    )
    .unwrap();
    let not_allowed = record_proof(
        &repo,
        &data,
        &cfg,
        &proof_input(&good, tool, "emulator-5556", 1),
    )
    .unwrap_err();
    assert!(
        matches!(not_allowed, ProofError::DeniedDevice(_)),
        "{not_allowed}"
    );

    // Empty allowed_tools must fail closed
    let empty_tools_cfg = ProofConfig {
        allowed_tools: vec![],
        ..ProofConfig::default()
    };
    assert!(
        record_proof(
            &repo,
            &data,
            &empty_tools_cfg,
            &proof_input(&good, tool, "emulator-5554", 1)
        )
        .is_err()
    );

    // Symlink image_path rejected
    #[cfg(unix)]
    {
        let link = tmp.path().join("link.png");
        std::os::unix::fs::symlink(&good, &link).unwrap();
        assert!(
            record_proof(
                &repo,
                &data,
                &cfg,
                &proof_input(&link, tool, "emulator-5554", 1)
            )
            .is_err()
        );
    }

    // Legacy/non-mobile-mcp capture tools are rejected by default ProofConfig.
    for rejected_tool in [
        "replicant-mcp:ui-capture",
        "adb-screencap",
        "simctl-screenshot",
    ] {
        assert!(
            record_proof(
                &repo,
                &data,
                &cfg,
                &proof_input(&good, rejected_tool, "emulator-5554", 1)
            )
            .is_err(),
            "{rejected_tool} must be rejected"
        );
    }

    // Record cleanly for allowlisted device (case-insensitive match), then tamper HMAC file.
    let saved = record_proof(
        &repo,
        &data,
        &cfg,
        &proof_input(&good, tool, "EMULATOR-5554", 2),
    )
    .unwrap();
    let tampered = std::fs::read_to_string(&saved.proof_path)
        .unwrap()
        .replace("EMULATOR-5554", "emulator-9999");
    std::fs::write(&saved.proof_path, tampered).unwrap();
    assert!(verify_proof(&repo, &data, &cfg, None).is_err());
}

#[test]
fn gate_enforces_proof_skips_heavy_commands_invalidates_config_flip_and_catches_tamper() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);

    let machine = aaf_domain::config::MachineConfig {
        max_load_percent: 0,
        ..Default::default()
    };
    let clock = FixedClock(100);

    // 1. Pass gate with proof.required = false -> cached Clear receipt
    let mut cfg = aaf_domain::config::RepoConfig::default();
    cfg.commands.test_scoped = "true".to_string();
    cfg.proof.required = false;
    let first =
        aaf_engine::gate::run_gate_with(&data, &repo, &cfg, GateMode::Scoped, &clock, &machine)
            .unwrap();
    assert_eq!(first.status, GateStatus::Clear, "{:?}", first.reasons);

    // 2. Flip proof.required false -> true: config_hash changes & heavy command is skipped!
    let marker = tmp.path().join("should-not-exist");
    cfg.proof.required = true;
    cfg.commands.test_scoped = format!("touch '{}' && true", marker.display());
    let blocked =
        aaf_engine::gate::run_gate_with(&data, &repo, &cfg, GateMode::Scoped, &clock, &machine)
            .unwrap();
    assert_eq!(blocked.status, GateStatus::Blocked, "{:?}", blocked.reasons);
    assert!(
        blocked.commands.is_empty(),
        "heavy commands must be skipped"
    );
    assert!(
        !marker.exists(),
        "command must not execute when proof blocks"
    );
    assert!(
        blocked
            .reasons
            .iter()
            .any(|r| r.contains("proof required:")),
        "{:?}",
        blocked.reasons
    );

    // 3. Record proof -> CLEAR
    let img = tmp.path().join("ios-sim.png");
    std::fs::write(&img, build_png(1170, 2532, 9000)).unwrap();
    cfg.commands.test_scoped = "true".to_string();
    record_proof(
        &repo,
        &data,
        &cfg.proof,
        &proof_input(
            &img,
            "mobile-mcp:mobile_save_screenshot",
            "iPhone-16-Sim",
            101,
        ),
    )
    .unwrap();
    let clear =
        aaf_engine::gate::run_gate_with(&data, &repo, &cfg, GateMode::Scoped, &clock, &machine)
            .unwrap();
    assert_eq!(clear.status, GateStatus::Clear, "{:?}", clear.reasons);

    // 4. Command deletes out-of-repo proof image mid-gate -> post-command verify blocks!
    cfg.commands.test_scoped = format!("rm -f '{}' && true", img.display());
    let mid_tamper =
        aaf_engine::gate::run_gate_with(&data, &repo, &cfg, GateMode::Scoped, &clock, &machine)
            .unwrap();
    assert_eq!(
        mid_tamper.status,
        GateStatus::Blocked,
        "{:?}",
        mid_tamper.reasons
    );

    // 5. Re-write image, re-record, then corrupt live screenshot -> must NOT reuse CLEAR -> BLOCKED
    std::fs::write(&img, build_png(1170, 2532, 9000)).unwrap();
    cfg.commands.test_scoped = "true".to_string();
    record_proof(
        &repo,
        &data,
        &cfg.proof,
        &proof_input(
            &img,
            "mobile-mcp:mobile_save_screenshot",
            "iPhone-16-Sim",
            102,
        ),
    )
    .unwrap();
    let clear2 =
        aaf_engine::gate::run_gate_with(&data, &repo, &cfg, GateMode::Scoped, &clock, &machine)
            .unwrap();
    assert_eq!(clear2.status, GateStatus::Clear, "{:?}", clear2.reasons);

    std::fs::write(&img, vec![0u8; 9000]).unwrap();
    let reblocked =
        aaf_engine::gate::run_gate_with(&data, &repo, &cfg, GateMode::Scoped, &clock, &machine)
            .unwrap();
    assert_eq!(
        reblocked.status,
        GateStatus::Blocked,
        "{:?}",
        reblocked.reasons
    );
}

#[test]
fn rejects_empty_allowlist_fdict_noncontiguous_idat_missing_plte_and_text_padding() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);
    let cfg = ProofConfig::default();
    let tool = "mobile-mcp:mobile_save_screenshot";
    let try_rec = |name: &str, bytes: Vec<u8>| {
        let p = tmp.path().join(name);
        std::fs::write(&p, bytes).unwrap();
        record_proof(&repo, &data, &cfg, &proof_input(&p, tool, "emu-1", 1))
    };

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&1080u32.to_be_bytes());
    ihdr.extend_from_slice(&1920u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    let diverse: Vec<u8> = (0..9000usize)
        .map(|i| ((i * 37 + 13) % 251) as u8)
        .collect();
    let magic = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

    // 1. Zlib FDICT bit set (0x78, 0xBB -> 0x78BB % 31 == 0, flg & 0x20 != 0) rejected.
    let mut fdict = magic.to_vec();
    push_chunk(&mut fdict, *b"IHDR", &ihdr);
    let mut fdict_idat = vec![0x78, 0xBB];
    fdict_idat.extend_from_slice(&diverse[2..]);
    push_chunk(&mut fdict, *b"IDAT", &fdict_idat);
    push_chunk(&mut fdict, *b"IEND", &[]);
    assert!(try_rec("fdict.png", fdict).is_err());

    // 2. Non-contiguous IDAT chunks (tEXt between two IDATs) rejected.
    let mut split = magic.to_vec();
    push_chunk(&mut split, *b"IHDR", &ihdr);
    let mut part1 = vec![0x78, 0x9C];
    part1.extend_from_slice(&diverse[2..4500]);
    push_chunk(&mut split, *b"IDAT", &part1);
    push_chunk(&mut split, *b"tEXt", b"k\0v");
    push_chunk(&mut split, *b"IDAT", &diverse[4500..]);
    push_chunk(&mut split, *b"IEND", &[]);
    assert!(try_rec("split.png", split).is_err());

    // 3. Indexed PNG (color_type=3) without PLTE rejected.
    let mut indexed = magic.to_vec();
    let mut ihdr_idx = ihdr.clone();
    ihdr_idx[9] = 3;
    push_chunk(&mut indexed, *b"IHDR", &ihdr_idx);
    let mut good_idat = vec![0x78, 0x9C];
    good_idat.extend_from_slice(&diverse[2..]);
    push_chunk(&mut indexed, *b"IDAT", &good_idat);
    push_chunk(&mut indexed, *b"IEND", &[]);
    assert!(try_rec("indexed.png", indexed).is_err());

    // 4. 8 KiB dummy tEXt padding + tiny 18-byte IDAT rejected.
    let mut padded = magic.to_vec();
    push_chunk(&mut padded, *b"IHDR", &ihdr);
    push_chunk(&mut padded, *b"tEXt", &vec![b'A'; 9000]);
    let mut tiny_idat = vec![0x78, 0x9C];
    tiny_idat.extend_from_slice(&(0u8..16).collect::<Vec<_>>());
    push_chunk(&mut padded, *b"IDAT", &tiny_idat);
    push_chunk(&mut padded, *b"IEND", &[]);
    assert!(try_rec("padded.png", padded).is_err());

    // 5. Empty repo `.adb-allowlist` (comments/whitespace only) fails closed.
    std::fs::write(repo.join(".adb-allowlist"), "# empty\n  \n").unwrap();
    let err = try_rec("good.png", build_png(1080, 1920, 9000)).unwrap_err();
    assert!(matches!(err, ProofError::DeniedDevice(_)), "{err}");
}
