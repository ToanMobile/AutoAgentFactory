//! Integration tests for `aaf watch --once` (read-only live dashboard)
//! and macOS `keychain_sandbox_exposure` advisory warning in `aaf doctor`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "cli_support.rs"]
mod support;

use std::fs::{self, File};
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::blocked::BlockedReason;
use aaf_domain::gate::{
    GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION, Receipt, Severity,
    StaticFinding, StoreComplianceReport,
};
use aaf_domain::ports::{AccountStore, RunStore, TaskStore};
use aaf_domain::run::{RunRecord, RunStatus};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use serde_json::json;
use support::{Sandbox, write};

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(1_700_000_000, |d| {
            i64::try_from(d.as_secs()).unwrap_or(i64::MAX)
        })
}

fn sample_spec() -> ImmutableSpec {
    ImmutableSpec {
        description: "Verify watch snapshot".to_string(),
        acceptance: vec!["passes tests".to_string()],
    }
}

#[test]
fn watch_once_is_read_only_and_never_creates_receipt_key() {
    let sb = Sandbox::new();
    let (code, out) = sb.json(&["watch", "--once"]);

    assert_eq!(code, 0, "{out}");
    assert_eq!(out["ok"], json!(true));
    assert_eq!(out["command"], json!("watch"));
    assert_eq!(out["tasks"]["counts"]["total"], json!(0));
    assert!(out["receipt"].is_null(), "{out}");
    assert!(!sb.data.exists(), "watch must not create data dir");
    assert!(!sb.data.join("receipt.key").exists());
    assert!(!sb.data.join("aaf.db").exists());
}

#[test]
fn watch_once_reports_tasks_by_state_unblock_cycles_and_blocked_reason() {
    let sb = Sandbox::new();
    let now = unix_now();
    let repo = sb.repo.to_string_lossy().into_owned();
    fs::create_dir_all(&sb.data).unwrap();
    let db = Db::open(&sb.data.join("aaf.db")).unwrap();

    let mut ready = Task::new_root(
        "t-ready01",
        &repo,
        "Ready item",
        sample_spec(),
        Priority::P1,
        now,
    );
    ready.state = TaskState::Ready;
    db.insert_task(&ready).unwrap();

    let mut blocked = Task::new_root(
        "t-block01",
        &repo,
        "Blocked item",
        sample_spec(),
        Priority::P0,
        now,
    );
    blocked.state = TaskState::Blocked;
    blocked.unblock_cycles = 2;
    blocked.blocked_reason = Some(BlockedReason::NeedsHuman {
        why: "missing api key".to_string(),
    });
    db.insert_task(&blocked).unwrap();

    let mut done = Task::new_root(
        "t-done001",
        &repo,
        "Done item",
        sample_spec(),
        Priority::P2,
        now,
    );
    done.state = TaskState::Done;
    db.insert_task(&done).unwrap();

    let (code, out) = sb.json(&["watch", "--once"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["tasks"]["counts"]["total"], json!(3));
    assert_eq!(out["tasks"]["counts"]["ready"], json!(1));
    assert_eq!(out["tasks"]["counts"]["blocked"], json!(1));
    assert_eq!(out["tasks"]["counts"]["done"], json!(1));

    let active = out["tasks"]["active"].as_array().unwrap();
    assert_eq!(active.len(), 2, "{out}");
    let blk = active
        .iter()
        .find(|t| t["id"] == json!("t-block01"))
        .unwrap();
    assert_eq!(blk["state"], json!("blocked"));
    assert_eq!(blk["priority"], json!("P0"));
    assert_eq!(blk["unblock_cycles"], json!(2));
    assert_eq!(blk["blocked_reason"], json!("needs_human: missing api key"));
    assert!(active.iter().all(|t| t["id"] != json!("t-done001")));

    let text = String::from_utf8(sb.aaf(&["watch", "--once"]).stdout).unwrap();
    assert!(text.contains("[Tasks]"), "{text}");
    assert!(text.contains("[Heavy Slots]"), "{text}");
    assert!(text.contains("[Accounts]"), "{text}");
    assert!(text.contains("[Receipt]"), "{text}");
    assert!(text.contains("t-block01"), "{text}");
    assert!(text.contains("needs_human: missing api key"), "{text}");
}

#[test]
fn watch_once_probes_heavy_slots_drain_lock_and_stats() {
    let sb = Sandbox::new();
    let throttle = sb.data.join("throttle");
    fs::create_dir_all(&throttle).unwrap();

    let slot0_path = throttle.join("heavy.0.lock");
    let mut slot0 = File::options()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&slot0_path)
        .unwrap();
    write!(slot0, "reserved=536870912\npgid=4242\nlabel=gradle\n").unwrap();
    slot0.try_lock().unwrap();

    write(&throttle.join("heavy.1.lock"), "");
    write(
        &throttle.join("heavy.2.lock"),
        "reserved=268435456\npgid=2147483646\n",
    );
    let live_pgid: u32 = std::process::Command::new("/bin/ps")
        .args(["-o", "pgid=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .filter(|p| *p > 1)
        .unwrap_or_else(std::process::id);
    write(
        &throttle.join("heavy.3.lock"),
        &format!("reserved=134217728\npgid={live_pgid}\n"),
    );

    let drain_path = throttle.join("drain.lock");
    let drain = File::options()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&drain_path)
        .unwrap();
    drain.try_lock().unwrap();

    write(
        &sb.data.join("heavy_stats.json"),
        &json!({
            "entries": {
                "repoA/build": [{"peak_rss_bytes": 104_857_600, "duration_ms": 1200}],
                "repoA/test_scoped": [{"peak_rss_bytes": 209_715_200, "duration_ms": 2400}]
            }
        })
        .to_string(),
    );

    let (code, out) = sb.json(&["watch", "--once"]);
    drop(slot0);
    drop(drain);

    assert_eq!(code, 0, "{out}");
    assert_eq!(out["heavy_slots"]["drain_active"], json!(true));
    assert_eq!(out["heavy_slots"]["measured_commands"], json!(2));
    let slots = out["heavy_slots"]["slots"].as_array().unwrap();
    assert_eq!(slots.len(), 4, "{out}");

    assert_eq!(slots[0]["slot"], json!(0));
    assert_eq!(slots[0]["state"], json!("busy"));
    assert_eq!(slots[0]["pgid"], json!(4242));
    assert_eq!(slots[0]["label"], json!("gradle"));
    assert_eq!(slots[0]["reserved_ram_mb"], json!(512));

    assert_eq!(slots[1]["slot"], json!(1));
    assert_eq!(slots[1]["state"], json!("free"));
    assert!(slots[1]["pgid"].is_null());

    assert_eq!(slots[2]["slot"], json!(2));
    assert_eq!(slots[2]["state"], json!("stale"));
    assert_eq!(slots[2]["pgid"], json!(2147483646));
    assert_eq!(slots[2]["reserved_ram_mb"], json!(256));

    assert_eq!(slots[3]["slot"], json!(3));
    assert_eq!(slots[3]["state"], json!("orphan"));
    assert_eq!(slots[3]["pgid"], json!(live_pgid));
    assert_eq!(slots[3]["reserved_ram_mb"], json!(128));
}

#[test]
fn watch_once_reports_accounts_selection_cooldown_and_5h_tokens() {
    let sb = Sandbox::new();
    let now = unix_now();
    let repo = sb.repo.to_string_lossy().into_owned();
    write(
        &sb.home.join(".config/autofactory/config.toml"),
        "[accounts]\nselection = \"sticky\"\n",
    );

    fs::create_dir_all(&sb.data).unwrap();
    let db = Db::open(&sb.data.join("aaf.db")).unwrap();
    let dir_a = sb.home.join(".claude-work");
    let dir_b = sb.home.join(".claude-alt");
    fs::create_dir_all(&dir_a).unwrap();
    fs::create_dir_all(&dir_b).unwrap();

    db.upsert_account(&AccountProfile {
        id: "claude_code-work".to_string(),
        provider: ProviderKind::ClaudeCode,
        config_dir: dir_a.to_string_lossy().into_owned(),
        name: "work".to_string(),
    })
    .unwrap();
    db.upsert_account(&AccountProfile {
        id: "claude_code-alt".to_string(),
        provider: ProviderKind::ClaudeCode,
        config_dir: dir_b.to_string_lossy().into_owned(),
        name: "alt".to_string(),
    })
    .unwrap();
    let cooldown_until = now + 7200;
    db.set_cooldown("claude_code-alt", Some(cooldown_until))
        .unwrap();

    let mut task = Task::new_root(
        "t-acct01",
        &repo,
        "Track tokens",
        sample_spec(),
        Priority::P2,
        now,
    );
    task.state = TaskState::Running;
    db.insert_task(&task).unwrap();

    db.insert_run(&RunRecord {
        id: "run-recent".to_string(),
        task_id: "t-acct01".to_string(),
        account_id: Some("claude_code-work".to_string()),
        executor: "claude".to_string(),
        worktree_path: repo.clone(),
        branch: "aaf/t-acct01".to_string(),
        status: RunStatus::Succeeded,
        exit_code: Some(0),
        tokens_in: 1500,
        tokens_out: 500,
        started_at: now - 600,
        ended_at: Some(now - 500),
    })
    .unwrap();
    db.insert_run(&RunRecord {
        id: "run-old".to_string(),
        task_id: "t-acct01".to_string(),
        account_id: Some("claude_code-work".to_string()),
        executor: "claude".to_string(),
        worktree_path: repo,
        branch: "aaf/t-acct01".to_string(),
        status: RunStatus::Succeeded,
        exit_code: Some(0),
        tokens_in: 9000,
        tokens_out: 1000,
        started_at: now - 30_000,
        ended_at: Some(now - 29_900),
    })
    .unwrap();

    let (code, out) = sb.json(&["watch", "--once"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["accounts"]["selection_mode"], json!("sticky"));
    assert_eq!(out["accounts"]["pinned_account"], json!("claude_code-work"));

    let entries = out["accounts"]["entries"].as_array().unwrap();
    let work = entries
        .iter()
        .find(|a| a["id"] == json!("claude_code-work"))
        .unwrap();
    assert_eq!(work["state"], json!("available"));
    assert!(work["cooldown_until"].is_null());
    assert_eq!(work["tokens_5h"], json!(2000));

    let alt = entries
        .iter()
        .find(|a| a["id"] == json!("claude_code-alt"))
        .unwrap();
    assert_eq!(alt["state"], json!("cooldown"));
    assert_eq!(alt["cooldown_until"], json!(cooldown_until));
    assert_eq!(alt["tokens_5h"], json!(0));
}

#[test]
fn watch_once_summarizes_receipt_and_supports_all_and_repo_flags() {
    let sb = Sandbox::new();
    let other_tmp = tempfile::tempdir().unwrap();
    let other_repo = fs::canonicalize(other_tmp.path()).unwrap();
    let now = unix_now();

    fs::create_dir_all(&sb.data).unwrap();
    let db = Db::open(&sb.data.join("aaf.db")).unwrap();
    let mut t1 = Task::new_root(
        "t-repoa",
        sb.repo.to_string_lossy(),
        "Repo A task",
        sample_spec(),
        Priority::P1,
        now,
    );
    t1.state = TaskState::Ready;
    db.insert_task(&t1).unwrap();

    let mut t2 = Task::new_root(
        "t-repob",
        other_repo.to_string_lossy(),
        "Repo B task",
        sample_spec(),
        Priority::P2,
        now,
    );
    t2.state = TaskState::Ready;
    db.insert_task(&t2).unwrap();

    let key = aaf_fp::load_or_create_key(&sb.data).unwrap();
    let receipt = Receipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        repo_root: sb.repo.to_string_lossy().into_owned(),
        tree_fp: "git-tree:0123456789abcdef999999999999999999999999".to_string(),
        head_sha: None,
        base_sha: None,
        mode: GateMode::Scoped,
        status: GateStatus::Clear,
        reasons: vec!["warning: work tree: ignored file".to_string()],
        config_hash: "cfg".to_string(),
        commands: Vec::new(),
        static_findings: vec![StaticFinding {
            rule: "LOG-001".to_string(),
            severity: Severity::Warn,
            file: "src/app.ts".to_string(),
            line: Some(10),
            message: "console.log".to_string(),
            fix: "use structured logger".to_string(),
        }],
        suppressions: 0,
        head_lock: HeadLockReport {
            status: LockStatus::Clear,
            lock_fp: String::new(),
            locked_changes: Vec::new(),
            approval: None,
        },
        store_compliance: StoreComplianceReport::not_applicable(),
        created_at: 1_700_000_123,
    };
    aaf_fp::write_receipt(&sb.data, &key, &receipt).unwrap();

    let (code, scoped) = sb.json(&["watch", "--once"]);
    assert_eq!(code, 0, "{scoped}");
    assert_eq!(scoped["tasks"]["counts"]["total"], json!(1));
    assert_eq!(scoped["receipt"]["status"], json!("CLEAR"));
    assert_eq!(scoped["receipt"]["mode"], json!("scoped"));
    assert_eq!(scoped["receipt"]["tree_fp_short"], json!("0123456789ab"));
    assert_eq!(scoped["receipt"]["warnings"], json!(2));
    assert_eq!(scoped["receipt"]["created_at"], json!(1_700_000_123));

    let (code, all) = sb.json(&["watch", "--once", "--all"]);
    assert_eq!(code, 0, "{all}");
    assert_eq!(all["scope"], json!("all"));
    assert_eq!(all["tasks"]["counts"]["total"], json!(2));
    assert!(all["receipt"].is_null());

    let (code, by_repo) = sb.json(&["watch", "--once", "--repo", other_repo.to_str().unwrap()]);
    assert_eq!(code, 0, "{by_repo}");
    assert_eq!(by_repo["tasks"]["counts"]["total"], json!(1));
    assert_eq!(by_repo["tasks"]["active"][0]["id"], json!("t-repob"));
    assert!(by_repo["receipt"].is_null());
}

#[cfg(target_os = "macos")]
#[test]
fn doctor_surfaces_keychain_sandbox_exposure_advisory_warning() {
    let sb = Sandbox::new();
    assert_eq!(sb.json(&["init"]).0, 0);

    let (code, out) = sb.json(&["doctor"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["status"], json!("ok"));
    let check = out["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == json!("keychain_sandbox_exposure"))
        .unwrap_or_else(|| panic!("missing keychain_sandbox_exposure: {out}"));
    assert_eq!(check["ok"], json!(false));
    assert_eq!(check["required"], json!(false));
    assert_eq!(check["applicable"], json!(true));
    let detail = check["detail"].as_str().unwrap();
    assert!(detail.contains("~/Library/Keychains"), "{detail}");
    assert!(detail.contains("SECURITY.md"), "{detail}");
}

#[cfg(unix)]
#[test]
fn watch_opens_existing_database_strictly_read_only() {
    use std::os::unix::fs::PermissionsExt;

    let sb = Sandbox::new();
    std::fs::create_dir_all(&sb.data).unwrap();
    let db_path = sb.data.join("aaf.db");
    {
        let db = Db::open(&db_path).unwrap();
        let repo = std::fs::canonicalize(&sb.repo)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let mut task = Task::new_root(
            "t-ro",
            &repo,
            "Read only task",
            sample_spec(),
            Priority::P2,
            100,
        );
        task.state = TaskState::Ready;
        db.insert_task(&task).unwrap();
    }
    std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o444)).unwrap();

    let (code, out) = sb.json(&["watch", "--once"]);
    // Restore write bit before assertions so TempDir always cleans up cleanly.
    let _ = std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o600));
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["tasks"]["counts"]["total"], json!(1));
    assert_eq!(out["tasks"]["active"][0]["id"], json!("t-ro"));
}

#[cfg(unix)]
#[test]
fn watch_opens_wal_database_in_read_only_directory_without_creating_shm() {
    use std::os::unix::fs::PermissionsExt;

    let sb = Sandbox::new();
    fs::create_dir_all(&sb.data).unwrap();
    let db_path = sb.data.join("aaf.db");
    {
        let db = Db::open(&db_path).unwrap();
        let repo = fs::canonicalize(&sb.repo)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let mut task = Task::new_root(
            "t-rodir",
            &repo,
            "Read-only dir WAL task",
            sample_spec(),
            Priority::P1,
            100,
        );
        task.state = TaskState::Ready;
        db.insert_task(&task).unwrap();
    }
    assert!(!sb.data.join("aaf.db-wal").exists());
    assert!(!sb.data.join("aaf.db-shm").exists());

    fs::set_permissions(&db_path, fs::Permissions::from_mode(0o444)).unwrap();
    fs::set_permissions(&sb.data, fs::Permissions::from_mode(0o555)).unwrap();

    let (w_code, w_out) = sb.json(&["watch", "--once"]);
    let (s_code, s_out) = sb.json(&["status"]);
    let shm_created = sb.data.join("aaf.db-shm").exists();
    let wal_created = sb.data.join("aaf.db-wal").exists();

    // Restore permissions before assertions so TempDir always cleans up cleanly.
    let _ = fs::set_permissions(&sb.data, fs::Permissions::from_mode(0o700));
    let _ = fs::set_permissions(&db_path, fs::Permissions::from_mode(0o600));

    assert_eq!(w_code, 0, "{w_out}");
    assert_eq!(w_out["tasks"]["counts"]["total"], json!(1));
    assert_eq!(w_out["tasks"]["active"][0]["id"], json!("t-rodir"));
    assert_eq!(s_code, 0, "{s_out}");
    assert_eq!(s_out["tasks"]["total"], json!(1));
    assert!(
        !shm_created,
        "read-only WAL open must not create aaf.db-shm"
    );
    assert!(
        !wal_created,
        "read-only WAL open must not create aaf.db-wal"
    );
}

#[test]
fn watch_sanitizes_ansi_and_osc_sequences_from_untrusted_text() {
    let sb = Sandbox::new();
    let now = unix_now();
    let repo = sb.repo.to_string_lossy().into_owned();
    fs::create_dir_all(&sb.data).unwrap();
    let db = Db::open(&sb.data.join("aaf.db")).unwrap();
    let mut task = Task::new_root(
        "t-esc01",
        &repo,
        "\x1b[31mEvil\x1b[0m \x1b]0;spoof\x07title\r\nnext",
        sample_spec(),
        Priority::P1,
        now,
    );
    task.state = TaskState::Blocked;
    task.blocked_reason = Some(BlockedReason::NeedsHuman {
        why: "\x1b[2Jbad\x1b]8;;https://evil.example\x1b\\link\x1b]8;;\x1b\\\x07".to_string(),
    });
    db.insert_task(&task).unwrap();

    let text = String::from_utf8(sb.aaf(&["watch", "--once"]).stdout).unwrap();
    assert!(!text.contains('\x1b'), "ESC leaked into output: {text:?}");
    assert!(!text.contains('\x07'), "BEL leaked into output: {text:?}");
    assert!(text.contains("Evil title  next"), "{text}");
    assert!(text.contains("needs_human: badlink"), "{text}");
}
