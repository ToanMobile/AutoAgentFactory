#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Oracle tests for DevKit-standard acceptance & handover reports:
//! 1. HTML & Markdown rendering (3-line B10 summary, `Nguyên nhân:`, 5 acceptance items,
//!    Automated QA Suite `[tự chạy]`, 7 Golden Standards, `CSP_META`, XSS escaping,
//!    verified base64 PNG embedding, and `#[serde(default)]` backward compatibility).
//! 2. Atomic mode `0600` auto-export (`write_auto_handover_reports`), traversal & symlink
//!    refusal, read-only `receipt.key` invariant, and `gc` cleanup of both `.html` & `.md`.

use std::fs;
use std::path::Path;
use std::process::Command;

use aaf_domain::config::{ProofConfig, RepoConfig};
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::gate::{
    CommandResult, GateMode, GateStatus, GoldenScore, HeadLockReport, LockStatus,
    PostGateAuditRecord, RECEIPT_SCHEMA_VERSION, Receipt, StoreComplianceReport,
};
use aaf_domain::ports::{EventSink, TaskStore};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::gc::{GcOptions, run_gc};
use aaf_engine::handover::{
    CSP_META, HandoverData, HandoverError, build_handover_from_parts, encode_base64, render_html,
    render_markdown, write_auto_handover_reports,
};
use aaf_engine::proof::{ProofInput, record_proof};
use serde_json::json;

fn init_git_repo(dir: &Path) {
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["config", "user.email", "oracle@example.invalid"][..],
        &["config", "user.name", "Oracle"][..],
    ] {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}");
    }
    fs::write(dir.join("README.md"), "# devkit handover\n").unwrap();
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

fn seven_scores(score: u8) -> Vec<GoldenScore> {
    ["G1", "G2", "G3", "G4", "G5", "G6", "G7"]
        .into_iter()
        .map(|id| GoldenScore {
            id: id.to_string(),
            score,
            deduction_reason: String::new(),
        })
        .collect()
}

#[test]
fn devkit_html_and_markdown_render_b10_root_cause_qa_suite_golden_csp_xss_and_proof() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let data = tmp.path().join("data");
    fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);

    let img = tmp.path().join("proof.png");
    let png_bytes = build_png(1080, 1920, 9_000);
    fs::write(&img, &png_bytes).unwrap();

    let proof_cfg = ProofConfig::default();
    let recorded = record_proof(
        &repo,
        &data,
        &proof_cfg,
        &ProofInput {
            image_path: img,
            source_tool: "mobile-mcp:mobile_save_screenshot".to_string(),
            device_serial: "emulator-5554".to_string(),
            task_id: Some("t-devkit-1".to_string()),
            now: 1_700_000_500,
        },
    )
    .unwrap();

    let mut task = Task::new_root(
        "t-devkit-1",
        repo.to_string_lossy(),
        r#"Sửa lỗi <script>alert("title")</script> trên màn thanh toán"#,
        ImmutableSpec {
            description: "Khắc phục timeout thanh toán và thêm báo cáo DevKit".to_string(),
            acceptance: vec!["Báo cáo đủ 5 mục".to_string()],
        },
        Priority::P1,
        1_700_000_000,
    );
    task.state = TaskState::Review;
    task.context_notes.push(
        "Nguyên nhân: Thiếu guard timeout khi gọi gateway <img src=x onerror=alert(9)>".to_string(),
    );

    let receipt = Receipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        repo_root: repo.to_string_lossy().into_owned(),
        tree_fp: recorded.record.tree_fp.clone(),
        head_sha: Some("1".repeat(40)),
        base_sha: Some("0".repeat(40)),
        mode: GateMode::Scoped,
        status: GateStatus::Clear,
        reasons: vec![],
        config_hash: "cfg-devkit".to_string(),
        commands: vec![
            CommandResult {
                name: "test_scoped".to_string(),
                command: r#"cargo test -- "<script>cmd()</script>""#.to_string(),
                exit_code: Some(0),
                timed_out: false,
                duration_ms: 350,
                log_tail: "ok".to_string(),
            },
            CommandResult {
                name: "lint_check".to_string(),
                command: "cargo clippy -- -D warnings".to_string(),
                exit_code: Some(0),
                timed_out: false,
                duration_ms: 120,
                log_tail: "clean".to_string(),
            },
        ],
        static_findings: vec![],
        suppressions: 0,
        head_lock: HeadLockReport {
            status: LockStatus::Clear,
            lock_fp: String::new(),
            locked_changes: vec![],
            approval: None,
        },
        store_compliance: StoreComplianceReport::not_applicable(),
        created_at: 1_700_000_600,
    };

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
        scores: seven_scores(98),
        adjudicated: false,
        dual_consensus: false,
        reviewers: Vec::new(),
        created_at: 1_700_000_650,
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
    assert_eq!(
        handover.verdict_label,
        "PASS — ĐỦ ĐIỀU KIỆN NGHIỆM THU & BÀN GIAO"
    );
    assert_eq!(handover.generated_at, 1_700_000_600);
    assert!(handover.root_cause.contains("Thiếu guard timeout"));
    assert_eq!(handover.next_step, "Sẵn sàng merge (aaf merge)");
    assert!(
        handover.gate_proof_next_line.contains("gate: PASS/exit 0")
            && handover.gate_proof_next_line.contains("emulator-5554")
            && handover
                .gate_proof_next_line
                .contains("bước tiếp theo: Sẵn sàng merge (aaf merge)"),
        "{}",
        handover.gate_proof_next_line
    );
    assert_eq!(handover.gate_commands.len(), 2);

    // HTML assertions: CSP, B10 3-line summary, Root Cause, 5 items, QA Suite, 7 Golden, XSS escaped, PNG embedded.
    let html = render_html(&handover, &data);
    assert!(html.contains(CSP_META));
    assert!(html.contains("Báo Cáo Nghiệm Thu &amp; Kiểm Toán Chất Lượng"));
    assert!(html.contains("✔ XONG"));
    assert!(html.contains("PASS — ĐỦ ĐIỀU KIỆN NGHIỆM THU &amp; BÀN GIAO"));
    assert!(html.contains("Tóm Tắt 3 Dòng (Chuẩn B10 DevKit)"));
    assert!(html.contains("1. Đã làm gì / Đã fix gì"));
    assert!(html.contains("Nguyên nhân:"));
    assert!(html.contains("2. Chặn bug cũ"));
    assert!(html.contains("3. Nguy cơ bug mới"));
    assert!(html.contains("4. An toàn mã nguồn"));
    assert!(html.contains("5. Còn hở / chưa kiểm"));
    assert!(html.contains("Automated QA Suite"));
    assert!(html.contains("test_scoped") && html.contains("lint_check"));
    assert!(html.contains("[tự chạy]"));
    assert!(html.contains("Quality &amp; 7 Golden Standards"));
    for g in ["G1", "G2", "G3", "G4", "G5", "G6", "G7"] {
        assert!(html.contains(&format!("<code>{g}</code>")), "missing {g}");
    }
    assert!(html.contains(&format!(
        "data:image/png;base64,{}",
        encode_base64(&png_bytes)
    )));
    // XSS neutralization on title, root_cause, and gate command
    assert!(!html.contains("<script>alert(\"title\")</script>"));
    assert!(!html.contains("<img src=x onerror=alert(9)>"));
    assert!(!html.contains("<script>cmd()</script>"));
    assert!(html.contains("&lt;script&gt;cmd()&lt;/script&gt;"));
    assert!(html.contains("&lt;img src=x onerror=alert(9)&gt;"));

    // Markdown assertions: starts with 3 B10 lines, includes Root Cause, 5 items, QA Suite, Scores, Proof.
    let md = render_markdown(&handover);
    let mut lines = md.lines();
    assert_eq!(lines.next(), Some("XONG"));
    assert!(lines.next().unwrap().contains("Sửa lỗi"));
    assert!(lines.next().unwrap().contains("gate: PASS/exit 0"));
    assert!(md.contains("   - **Nguyên nhân:** Thiếu guard timeout"));
    assert!(md.contains("## Automated QA Suite [tự chạy]"));
    assert!(md.contains("| test_scoped |"));
    assert!(md.contains("## Scores"));
    assert!(md.contains("## Verification Proof"));

    // Backward compatibility: legacy HandoverData JSON missing all 7 new fields still deserializes!
    let legacy_json = json!({
        "task_id": "t-legacy",
        "title": "Legacy",
        "state": "done",
        "repo_root": "/tmp/legacy",
        "tree_fp": null,
        "status": "XONG",
        "missing": [],
        "what_done": "done",
        "prevent_old_bug": "ok",
        "new_bug_risk": "none",
        "code_safety": "ok",
        "unverified": "không có",
        "mechanical_score": 100,
        "llm_average": null,
        "effective_score": 100,
        "scores": [],
        "same_account_warning": false,
        "proof_required": false,
        "proof": null
    });
    let parsed: HandoverData = serde_json::from_value(legacy_json).unwrap();
    assert_eq!(parsed.task_id, "t-legacy");
    assert!(parsed.gate_commands.is_empty());
    assert!(parsed.verdict_label.is_empty());
}

#[test]
fn auto_handover_export_atomic_0600_blocks_traversal_symlinks_no_key_and_gc_cleans_md_html() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let data = root.join("data");
    let worktrees = root.join("worktrees");
    let repo = root.join("repo");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&worktrees).unwrap();
    fs::create_dir_all(&repo).unwrap();

    let db = Db::open(&data.join("aaf.db")).unwrap();
    let mut task = Task::new_root(
        "t-exp-1",
        repo.to_string_lossy(),
        "Export handover safely",
        ImmutableSpec {
            description: "Xuất HTML + MD an toàn".to_string(),
            acceptance: vec!["mode 0600".to_string()],
        },
        Priority::P2,
        1_700_000_000,
    );
    task.state = TaskState::Done;
    db.insert_task(&task).unwrap();
    db.emit(
        &FactoryEvent::new(
            EventKind::GateResult,
            json!({
                "status": "CLEAR",
                "tree_fp": "git-tree:exp123",
                "commands_passed": 1,
                "commands_total": 1
            })
            .to_string(),
            1_700_000_010,
        )
        .for_task("t-exp-1"),
    )
    .unwrap();

    let cfg = RepoConfig::default();

    // 1. Path traversal task_id MUST be rejected.
    for bad_id in ["../evil", "a/b", "", ".."] {
        let err = write_auto_handover_reports(&data, &db, bad_id, &cfg).unwrap_err();
        assert!(
            matches!(err, HandoverError::InvalidTaskId(_)),
            "bad_id={bad_id:?} err={err:?}"
        );
    }

    // 2. Symlinked reports directory MUST be rejected.
    #[cfg(unix)]
    {
        let symlink_data = root.join("data-symlink");
        let outside = root.join("outside");
        fs::create_dir_all(&symlink_data).unwrap();
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, symlink_data.join("reports")).unwrap();
        let err = write_auto_handover_reports(&symlink_data, &db, "t-exp-1", &cfg).unwrap_err();
        assert!(
            matches!(err, HandoverError::UnsafePath(_)),
            "expected UnsafePath for symlink reports dir, got {err:?}"
        );
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
    }

    // 3. Normal export writes atomic mode 0600 HTML + MD and NEVER creates receipt.key.
    assert!(!data.join("receipt.key").exists());
    let paths = write_auto_handover_reports(&data, &db, "t-exp-1", &cfg).unwrap();
    assert_eq!(paths.status, "XONG");
    assert!(paths.html.is_file());
    assert!(paths.markdown.is_file());
    assert!(
        !data.join("receipt.key").exists(),
        "auto handover export must never create receipt.key"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for p in [&paths.html, &paths.markdown] {
            let mode = fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode,
                0o600,
                "expected mode 0600 for {}, got {mode:o}",
                p.display()
            );
        }
    }

    let html = fs::read_to_string(&paths.html).unwrap();
    let md = fs::read_to_string(&paths.markdown).unwrap();
    assert!(html.contains(CSP_META));
    assert!(html.contains("Nguyên nhân:"));
    assert!(html.contains("Automated QA Suite"));
    assert!(md.starts_with("XONG\n"));
    assert!(md.contains("Đã bàn giao (Done)"));

    // 4. `aaf gc --older-than-days 0` cleans BOTH `.html` and `.md` in `reports/`.
    let gc_report = run_gc(
        &data,
        &worktrees,
        &GcOptions {
            older_than_days: 0,
            ..GcOptions::default()
        },
    )
    .unwrap();
    assert!(!paths.html.exists(), "gc must clean expired .html report");
    assert!(!paths.markdown.exists(), "gc must clean expired .md report");
    assert!(
        gc_report
            .removed_files
            .iter()
            .any(|f| f.ends_with("handover-t-exp-1.html"))
    );
    assert!(
        gc_report
            .removed_files
            .iter()
            .any(|f| f.ends_with("handover-t-exp-1.md"))
    );
}
