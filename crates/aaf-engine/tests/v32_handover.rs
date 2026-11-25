#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Phase 2 v3.2 handover engine tests: std base64, HTML CSP + escaping + safe
//! proof embedding, compact event fallback, solo-dev badge, and PR sanitizer.

use std::path::Path;
use std::process::Command;

use aaf_domain::config::ProofConfig;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::gate::{
    CommandResult, GateMode, GateStatus, GoldenScore, HeadLockReport, LockStatus,
    PostGateAuditRecord, RECEIPT_SCHEMA_VERSION, Receipt, StoreComplianceReport,
};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::handover::{
    CSP_META, SOLO_DEV_BADGE, build_handover_from_parts, compose_pr_body, encode_base64,
    render_html, render_json, render_markdown,
};
use aaf_engine::proof::{ProofInput, record_proof};
use serde_json::json;

fn sample_task(id: &str, repo: &str, title: &str, state: TaskState) -> Task {
    let mut task = Task::new_root(
        id,
        repo,
        title,
        ImmutableSpec {
            description: "Implement handover pipeline".to_string(),
            acceptance: vec![
                "renders report".to_string(),
                "sanitizes PR body".to_string(),
            ],
        },
        Priority::P1,
        1_700_000_000,
    );
    task.state = state;
    task
}

fn clear_receipt(repo: &str, tree_fp: &str) -> Receipt {
    Receipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        repo_root: repo.to_string(),
        tree_fp: tree_fp.to_string(),
        head_sha: Some("abcdef1234567890".to_string()),
        base_sha: Some("1234567890abcdef".to_string()),
        mode: GateMode::Scoped,
        status: GateStatus::Clear,
        reasons: vec![],
        config_hash: "cfg-hash-1".to_string(),
        commands: vec![CommandResult {
            name: "test_scoped".to_string(),
            command: "cargo test".to_string(),
            exit_code: Some(0),
            timed_out: false,
            duration_ms: 420,
            log_tail: "ok".to_string(),
        }],
        static_findings: vec![],
        suppressions: 0,
        head_lock: HeadLockReport {
            status: LockStatus::Clear,
            lock_fp: String::new(),
            locked_changes: vec![],
            approval: None,
        },
        store_compliance: StoreComplianceReport::not_applicable(),
        created_at: 1_700_000_100,
    }
}

fn all_seven_scores(score: u8, reason: &str) -> Vec<GoldenScore> {
    ["G1", "G2", "G3", "G4", "G5", "G6", "G7"]
        .into_iter()
        .map(|id| GoldenScore {
            id: id.to_string(),
            score,
            deduction_reason: reason.to_string(),
        })
        .collect()
}

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

#[test]
fn encode_base64_matches_rfc_4648_and_png_magic() {
    let vectors = [
        (&b""[..], ""),
        (&b"f"[..], "Zg=="),
        (&b"fo"[..], "Zm8="),
        (&b"foo"[..], "Zm9v"),
        (&b"foob"[..], "Zm9vYg=="),
        (&b"fooba"[..], "Zm9vYmE="),
        (&b"foobar"[..], "Zm9vYmFy"),
        (
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A][..],
            "iVBORw0KGgo=",
        ),
    ];
    for (input, expected) in vectors {
        assert_eq!(encode_base64(input), expected, "input={input:?}");
    }
}

#[test]
fn build_handover_handles_compact_events_xong_chua_xong_and_solo_badge() {
    let task_done = sample_task(
        "t-compact",
        "/tmp/repo",
        "Cleaned worktree task",
        TaskState::Done,
    );
    let scores = all_seven_scores(96, "");
    let events = vec![
        FactoryEvent::new(
            EventKind::GateResult,
            json!({
                "status": "CLEAR",
                "tree_fp": "git-tree:deadbeef",
                "commands_passed": 3,
                "commands_total": 3,
                "static_critical": 0,
                "static_warn": 1,
                "suppressions": 0,
                "store_status": "not_applicable",
                "head_lock_status": "CLEAR",
                "mechanical_score": 97
            })
            .to_string(),
            100,
        )
        .for_task("t-compact"),
        FactoryEvent::new(
            EventKind::CrossAudit,
            json!({
                "verdict": "PASS",
                "reviewer": "cross-audit:claude/default",
                "same_account": true,
                "mechanical_score": 97,
                "llm_average": 96,
                "effective_score": 96,
                "scores": scores
            })
            .to_string(),
            101,
        )
        .for_task("t-compact"),
    ];

    // After worktree cleanup (receipt=None, audit=None), compact events still yield XONG
    // and display the solo-dev badge when !require_different_provider && same_account.
    let done = build_handover_from_parts(
        &task_done, &events, None, None, None, false, true, 95, 90, false,
    );
    assert_eq!(done.status, "XONG", "missing={:?}", done.missing);
    assert!(done.missing.is_empty());
    assert!(done.same_account_warning);
    assert!(done.code_safety.contains(SOLO_DEV_BADGE));
    assert!(done.prevent_old_bug.contains("[tự chạy]"));
    assert!(done.new_bug_risk.contains("[tự chạy]"));
    assert_eq!(done.effective_score, Some(96));

    let md = render_markdown(&done);
    assert!(md.contains("XONG"));
    assert!(md.contains(SOLO_DEV_BADGE));
    assert!(md.contains("[tự chạy]"));
    assert!(md.contains("[agent báo]"));
    assert_eq!(render_json(&done)["status"], json!("XONG"));

    // Missing proof when proof_required = true -> CHƯA XONG
    let missing_proof = build_handover_from_parts(
        &task_done, &events, None, None, None, true, false, 95, 90, false,
    );
    assert_eq!(missing_proof.status, "CHƯA XONG");
    assert!(missing_proof.missing.iter().any(|m| m.contains("proof")));

    // Low score under score_gate = true -> CHƯA XONG
    let receipt = clear_receipt("/tmp/repo", "git-tree:abc");
    let low_audit = PostGateAuditRecord {
        repo_root: "/tmp/repo".to_string(),
        tree_fp: "git-tree:abc".to_string(),
        config_hash: "cfg-hash-1".to_string(),
        verdict: "PASS".to_string(),
        reviewer: "cross-audit:codex/default".to_string(),
        same_account: false,
        mechanical_score: 98,
        llm_average: Some(92),
        effective_score: 92,
        scores: all_seven_scores(92, "needs edge test"),
        adjudicated: false,
        dual_consensus: false,
        reviewers: Vec::new(),
        created_at: 200,
    };
    let low = build_handover_from_parts(
        &task_done,
        &[],
        Some(&receipt),
        Some(&low_audit),
        None,
        false,
        true,
        95,
        90,
        true,
    );
    assert_eq!(low.status, "CHƯA XONG");
    assert!(low.missing.iter().any(|m| m.contains("min_average")));

    // Running task without receipt -> CHƯA XONG
    let running = sample_task("t-run", "/tmp/repo", "Still running", TaskState::Running);
    let incomplete =
        build_handover_from_parts(&running, &[], None, None, None, false, false, 95, 90, false);
    assert_eq!(incomplete.status, "CHƯA XONG");
    assert!(incomplete.missing.len() >= 2);
}

#[test]
fn render_html_sets_csp_escapes_xss_embeds_verified_png_and_degrades_safely() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);

    let img = tmp.path().join("ok.png");
    let png_bytes = build_png(1080, 1920, 9_000);
    std::fs::write(&img, &png_bytes).unwrap();

    let proof_cfg = ProofConfig::default();
    let recorded = record_proof(
        &repo,
        &data,
        &proof_cfg,
        &ProofInput {
            image_path: img.clone(),
            source_tool: "mobile-mcp:mobile_save_screenshot".to_string(),
            device_serial: "emulator-5554".to_string(),
            task_id: Some("t-html".to_string()),
            now: 1_700_000_500,
        },
    )
    .unwrap();

    let mut task = sample_task(
        "t-html",
        &repo.to_string_lossy(),
        r#"Fix <script>alert(1)</script> & "quotes" 'single'"#,
        TaskState::Review,
    );
    task.context_notes
        .push("<img src=x onerror=alert(2)>".to_string());
    let receipt = clear_receipt(&repo.to_string_lossy(), &recorded.record.tree_fp);
    let audit = PostGateAuditRecord {
        repo_root: repo.to_string_lossy().into_owned(),
        tree_fp: recorded.record.tree_fp.clone(),
        config_hash: receipt.config_hash.clone(),
        verdict: "PASS".to_string(),
        reviewer: "cross-audit:codex/default".to_string(),
        same_account: false,
        mechanical_score: 100,
        llm_average: Some(98),
        effective_score: 98,
        scores: all_seven_scores(98, "checked <script>evil</script>"),
        adjudicated: false,
        dual_consensus: false,
        reviewers: Vec::new(),
        created_at: 1_700_000_600,
    };

    let handover = build_handover_from_parts(
        &task,
        &[],
        Some(&receipt),
        Some(&audit),
        Some(&recorded.record),
        true,
        true,
        95,
        90,
        false,
    );
    assert_eq!(handover.status, "XONG");

    // 1. Valid verified PNG embeds as base64 data URI, CSP present, XSS escaped.
    let html = render_html(&handover, &data);
    assert!(html.contains(CSP_META), "missing CSP meta tag");
    assert!(!html.contains("<script>alert(1)</script>"));
    assert!(!html.contains("<script>evil</script>"));
    assert!(!html.contains("<img src=x onerror=alert(2)>"));
    assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(html.contains("&quot;quotes&quot;"));
    assert!(html.contains("&#39;single&#39;"));
    assert!(html.contains(&format!(
        "data:image/png;base64,{}",
        encode_base64(&png_bytes)
    )));

    // 2. Tamper live image bytes -> sha256 mismatch degrades gracefully (no base64 embed).
    let mut tampered = png_bytes.clone();
    tampered[40] ^= 0xFF;
    std::fs::write(&img, &tampered).unwrap();
    let html_tampered = render_html(&handover, &data);
    assert!(!html_tampered.contains("data:image/png;base64,"));
    assert!(html_tampered.contains("Proof image omitted:"));

    // 3. Delete live image -> missing image degrades gracefully.
    std::fs::remove_file(&img).unwrap();
    let html_missing = render_html(&handover, &data);
    assert!(!html_missing.contains("data:image/png;base64,"));
    assert!(html_missing.contains("Proof image omitted:"));

    // 4. Oversized > 4 MiB verified PNG -> degrades gracefully without embedding.
    let big_img = tmp.path().join("big.png");
    let big_bytes = build_png(1080, 1920, 4 * 1024 * 1024 + 4096);
    std::fs::write(&big_img, &big_bytes).unwrap();
    let big_rec = record_proof(
        &repo,
        &data,
        &proof_cfg,
        &ProofInput {
            image_path: big_img,
            source_tool: "mobile-mcp:mobile_save_screenshot".to_string(),
            device_serial: "emulator-5554".to_string(),
            task_id: Some("t-html".to_string()),
            now: 1_700_000_700,
        },
    )
    .unwrap();
    let big_handover = build_handover_from_parts(
        &task,
        &[],
        Some(&receipt),
        Some(&audit),
        Some(&big_rec.record),
        true,
        true,
        95,
        90,
        false,
    );
    let html_big = render_html(&big_handover, &data);
    assert!(!html_big.contains("data:image/png;base64,"));
    assert!(html_big.contains("exceeding 4 MiB"));
}

#[test]
fn compose_pr_body_sanitizes_html_links_secrets_base64_dedupes_proof_and_truncates() {
    let mut task = sample_task("t-pr", "/tmp/repo", "Sanitize PR body", TaskState::Review);
    // Construct secret-looking test strings from fragments so static checks never trip.
    let fake_ghp = format!("{}{}", "ghp_", "A".repeat(36));
    let fake_sk = format!("{}{}", "sk-ant-", "z".repeat(24));
    let fake_akia = format!("{}{}", "AKIA", "0123456789ABCDEF");
    let fake_pem = format!(
        "{}{}{}\nabc\n{}{}{}",
        "-----BEGIN ", "RSA PRIVATE", " KEY-----", "-----END ", "RSA PRIVATE", " KEY-----"
    );
    let dirty_note = format!(
        "## Verification Proof <script>alert(1)</script> <img src=x> <a href=\"https://x\">click</a> \
         ![ext](https://evil.example/a.png) data:image/png;base64,iVBORw0KGgo= \
         Bearer supersecret999999 {fake_ghp} {fake_sk} {fake_akia} {fake_pem} \
         https://alice:hunter2@db.example.com/app"
    );
    task.context_notes.push(dirty_note);

    let receipt = clear_receipt("/tmp/repo", "git-tree:pr123");
    let proof = aaf_domain::proof::ProofRecord {
        schema_version: 1,
        repo_root: "/tmp/repo".to_string(),
        tree_fp: "git-tree:pr123".to_string(),
        image_path: "/tmp/screen.png".to_string(),
        image_sha256: "a".repeat(64),
        width: 1080,
        height: 1920,
        byte_len: 12_345,
        source_tool: "mobile-mcp:mobile_save_screenshot".to_string(),
        device_serial: "emulator-5554".to_string(),
        task_id: Some("t-pr".to_string()),
        recorded_at: 1_700_000_800,
        base_sha: None,
        flows: Vec::new(),
    };
    let mut handover = build_handover_from_parts(
        &task,
        &[],
        Some(&receipt),
        None,
        Some(&proof),
        true,
        false,
        95,
        90,
        false,
    );
    let body = compose_pr_body(&receipt, Some(&handover));

    assert!(body.len() < 60 * 1024);
    assert_eq!(body.matches("## Verification Proof").count(), 1, "{body}");
    assert!(!body.contains("data:image/png;base64"));
    assert!(!body.contains("<script"));
    assert!(!body.contains("<img"));
    assert!(!body.contains("<a "));
    assert!(!body.contains("https://evil.example/a.png"));
    assert!(!body.contains("supersecret999999"));
    assert!(!body.contains(&fake_ghp));
    assert!(!body.contains(&fake_sk));
    assert!(!body.contains(&fake_akia));
    assert!(!body.contains("hunter2"));
    assert!(!body.contains("RSA PRIVATE"));

    // Huge multi-byte UTF-8 body truncates cleanly strictly below 60 KiB.
    handover.what_done = "Đánh giá tiếng Việt — ".repeat(5_000);
    let truncated = compose_pr_body(&receipt, Some(&handover));
    assert!(truncated.len() < 60 * 1024, "len={}", truncated.len());
    assert!(truncated.ends_with("...(truncated)"));
}
