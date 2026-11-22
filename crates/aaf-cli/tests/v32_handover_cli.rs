#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Phase 2 v3.2 CLI tests for `aaf handover <task_id>`.

#[path = "cli_support.rs"]
mod support;

use std::fs;
use std::path::Path;

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{EventSink, TaskStore};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::handover::CSP_META;
use serde_json::{Value, json};
use support::{Sandbox, one_object};

fn seed_done_task(sb: &Sandbox, task_id: &str) {
    fs::create_dir_all(&sb.data).unwrap();
    let db = Db::open(&sb.data.join("aaf.db")).unwrap();
    let mut task = Task::new_root(
        task_id,
        sb.repo.to_string_lossy(),
        "Deliver handover CLI",
        ImmutableSpec {
            description: "Render md/html/json handover safely".to_string(),
            acceptance: vec!["read-only DB".to_string()],
        },
        Priority::P1,
        1_700_000_000,
    );
    task.state = TaskState::Done;
    db.insert_task(&task).unwrap();
    db.emit(
        &FactoryEvent::new(
            EventKind::GateResult,
            json!({
                "status": "CLEAR",
                "tree_fp": "git-tree:cli123",
                "commands_passed": 2,
                "commands_total": 2,
                "static_critical": 0,
                "static_warn": 0,
                "head_lock_status": "CLEAR"
            })
            .to_string(),
            1_700_000_010,
        )
        .for_task(task_id),
    )
    .unwrap();
}

#[test]
fn handover_formats_md_html_json_are_read_only_and_never_create_receipt_key() {
    let sb = Sandbox::new();
    seed_done_task(&sb, "t-ho-1");
    let key_path = sb.data.join("receipt.key");
    assert!(!key_path.exists());

    // 1. Markdown output
    let md = sb.aaf(&["handover", "t-ho-1", "--format", "md"]);
    assert_eq!(
        md.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&md.stderr)
    );
    let md_text = String::from_utf8(md.stdout).unwrap();
    assert!(md_text.contains("XONG"), "{md_text}");
    assert!(md_text.contains("Deliver handover CLI"), "{md_text}");
    assert!(md_text.contains("[tự chạy]"), "{md_text}");

    // 2. HTML output contains CSP meta tag
    let html = sb.aaf(&["handover", "t-ho-1", "--format", "html"]);
    assert_eq!(
        html.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&html.stderr)
    );
    let html_text = String::from_utf8(html.stdout).unwrap();
    assert!(html_text.contains(CSP_META), "{html_text}");
    assert!(html_text.contains("XONG"), "{html_text}");

    // 3. Raw `--format json` prints valid JSON handover payload
    let raw_json = sb.aaf(&["handover", "t-ho-1", "--format", "json"]);
    assert_eq!(raw_json.status.code(), Some(0));
    let parsed: Value = serde_json::from_slice(&raw_json.stdout).unwrap();
    assert_eq!(parsed["task_id"], json!("t-ho-1"));
    assert_eq!(parsed["status"], json!("XONG"));

    // 4. Global `--json` envelope contract
    let (code, env_json) = sb.json(&["handover", "t-ho-1", "--format", "md"]);
    assert_eq!(code, 0, "{env_json}");
    assert_eq!(env_json["ok"], json!(true));
    assert_eq!(env_json["command"], json!("handover"));
    assert_eq!(env_json["handover_status"], json!("XONG"));
    assert_eq!(env_json["handover"]["status"], json!("XONG"));

    // Strictly read-only: receipt.key was NEVER created!
    assert!(!key_path.exists(), "handover must not create receipt.key");
}

#[test]
fn handover_refuses_out_and_open_inside_aaf_run_id_with_exit_2() {
    let sb = Sandbox::new();
    seed_done_task(&sb, "t-agent");

    let report_out = sb.data.join("reports/blocked.html");
    let out_res = sb
        .command(&[
            "handover",
            "t-agent",
            "--format",
            "html",
            "--out",
            report_out.to_str().unwrap(),
            "--json",
        ])
        .env("AAF_RUN_ID", "run-sandbox-7")
        .output()
        .unwrap();
    assert_eq!(out_res.status.code(), Some(2));
    let body = one_object(&out_res);
    assert_eq!(body["ok"], json!(false));
    assert_eq!(body["status"], json!("refused"));
    assert!(!report_out.exists());

    let open_res = sb
        .command(&[
            "handover", "t-agent", "--format", "html", "--open", "--json",
        ])
        .env("AAF_RUN_ID", "run-sandbox-7")
        .output()
        .unwrap();
    assert_eq!(open_res.status.code(), Some(2));
    assert_eq!(one_object(&open_res)["status"], json!("refused"));

    // Read-only stdout inside AAF_RUN_ID without --out/--open is still allowed.
    let ro = sb
        .command(&["handover", "t-agent", "--format", "md", "--json"])
        .env("AAF_RUN_ID", "run-sandbox-7")
        .output()
        .unwrap();
    assert_eq!(ro.status.code(), Some(0));
}

#[test]
fn handover_out_blocks_git_data_dir_protected_and_symlinks_but_allows_reports_0600() {
    let sb = Sandbox::new();
    seed_done_task(&sb, "t-out");

    // 1. Block .git/
    let git_out = sb.repo.join(".git/handover.html");
    let (code, body) = sb.json(&[
        "handover",
        "t-out",
        "--format",
        "html",
        "--out",
        git_out.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "{body}");
    assert_eq!(body["status"], json!("refused"));
    assert!(!git_out.exists());

    // 2. Block $AAF_DATA_DIR/evil.html (outside reports/)
    let evil_data = sb.data.join("evil.html");
    let (code, body) = sb.json(&[
        "handover",
        "t-out",
        "--format",
        "html",
        "--out",
        evil_data.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "{body}");
    assert_eq!(body["status"], json!("refused"));
    assert!(!evil_data.exists());

    // 3. Allow <data_dir>/reports/out.html (auto-creating reports/) with mode 0600 on Unix!
    let good_out = sb.data.join("reports/out.html");
    let (code, body) = sb.json(&[
        "handover",
        "t-out",
        "--format",
        "html",
        "--out",
        good_out.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{body}");
    assert!(good_out.is_file());
    let written = fs::read_to_string(&good_out).unwrap();
    assert!(written.contains(CSP_META));
    assert!(written.contains("XONG"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&good_out).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected mode 0600, got {mode:o}");
    }

    // 4. Block traversal $AAF_DATA_DIR/reports/../evil.html
    let escape_reports = sb.data.join("reports/../evil.html");
    let (code, body) = sb.json(&[
        "handover",
        "t-out",
        "--format",
        "html",
        "--out",
        escape_reports.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "{body}");
    assert_eq!(body["status"], json!("refused"));
    assert!(!evil_data.exists());

    // 5. Block protected files (.env.local and .claude/evil.md)
    let env_file = sb.repo.join(".env.local");
    let (code, _) = sb.json(&["handover", "t-out", "--out", env_file.to_str().unwrap()]);
    assert_eq!(code, 2);
    assert!(!env_file.exists());

    let claude_dir = sb.repo.join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    let claude_out = claude_dir.join("evil.md");
    let (code, _) = sb.json(&["handover", "t-out", "--out", claude_out.to_str().unwrap()]);
    assert_eq!(code, 2);
    assert!(!claude_out.exists());

    // 6. Block symlink target
    #[cfg(unix)]
    {
        let victim = sb.repo.join("victim.txt");
        fs::write(&victim, "original\n").unwrap();
        let link = sb.data.join("reports/link.html");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        let (code, body) = sb.json(&[
            "handover",
            "t-out",
            "--format",
            "html",
            "--out",
            link.to_str().unwrap(),
        ]);
        assert_eq!(code, 2, "{body}");
        assert_eq!(body["status"], json!("refused"));
        assert_eq!(fs::read_to_string(&victim).unwrap(), "original\n");
    }
    assert!(!Path::new(&sb.data).join("receipt.key").exists());
}

#[test]
fn handover_open_refuses_non_tty_with_exit_2() {
    let sb = Sandbox::new();
    seed_done_task(&sb, "t-open");

    // Sandbox::command attaches Stdio::null() -> stdin_is_tty == false -> exit 2!
    let (code, body) = sb.json(&["handover", "t-open", "--format", "html", "--open"]);
    assert_eq!(code, 2, "{body}");
    assert_eq!(body["ok"], json!(false));
    assert_eq!(body["status"], json!("refused"));
}
