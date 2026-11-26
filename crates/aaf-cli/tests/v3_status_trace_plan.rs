#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Pillar 2 CLI: `aaf status`, `aaf board trace <id>`, and `aaf plan "<goal>"`.

#[path = "factory_support.rs"]
mod support;

use std::fs::{self, OpenOptions};
use std::process::Command;

use aaf_domain::blocked::BlockedReason;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{EventSink, RunStore, TaskStore};
use aaf_domain::run::{RunFinish, RunRecord, RunStatus};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use serde_json::json;
use support::Fixture;

#[test]
fn status_and_trace_and_plan_commands_succeed_with_expected_json() {
    let fx = Fixture::new();
    let id = fx.add("Refactor retry queue");
    {
        let db = fx.db();
        let mut blocked = Task::new_root(
            "t-blk",
            fx.repo.to_string_lossy().into_owned(),
            "Blocked auth",
            ImmutableSpec {
                description: "Needs auth".to_string(),
                acceptance: vec![],
            },
            Priority::P1,
            1000,
        );
        blocked.state = TaskState::Blocked;
        blocked.blocked_reason = Some(BlockedReason::Infra {
            detail: "auth".to_string(),
        });
        db.insert_task(&blocked).unwrap();

        db.insert_run(&RunRecord {
            id: "r-cli".to_string(),
            task_id: id.clone(),
            account_id: None,
            executor: "claude".to_string(),
            worktree_path: fx.repo.to_string_lossy().into_owned(),
            branch: "aaf/t".to_string(),
            status: RunStatus::Running,
            exit_code: None,
            tokens_in: 0,
            tokens_out: 0,
            started_at: 1000,
            ended_at: None,
        })
        .unwrap();
        db.finish_run(
            "r-cli",
            &RunFinish {
                status: RunStatus::Succeeded,
                exit_code: Some(0),
                tokens_in: 20_000,
                tokens_out: 4_000,
                ended_at: 1012,
            },
        )
        .unwrap();
        db.emit(
            &FactoryEvent::new(EventKind::GateResult, r#"{"status":"CLEAR"}"#, 1013).for_task(&id),
        )
        .unwrap();
        db.emit(
            &FactoryEvent::new(EventKind::RunFinished, r#"{"status":"succeeded"}"#, 1014)
                .for_task(&id),
        )
        .unwrap();
    }

    // Hold one live throttle slot lock so `aaf status` reports active_slots=1.
    let throttle_dir = fx.data.join("throttle");
    fs::create_dir_all(&throttle_dir).unwrap();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(throttle_dir.join("heavy.0.lock"))
        .unwrap();
    lock.try_lock().unwrap();

    let status = fx.aaf(&["status"]);
    assert_eq!(status.code, 0, "{}", status.stderr);
    assert_eq!(status.json["ok"], json!(true));
    assert_eq!(
        status.json["scope"],
        json!(fx.repo.to_string_lossy().into_owned())
    );
    assert_eq!(status.json["tokens_in"], json!(20_000));
    assert_eq!(status.json["tokens_out"], json!(4_000));
    assert_eq!(status.json["tasks"]["ready"], json!(1));
    assert_eq!(status.json["tasks"]["blocked"], json!(1));
    assert_eq!(status.json["tasks"]["backlog"], json!(0));
    assert_eq!(status.json["tasks"]["cancelled"], json!(0));
    assert_eq!(status.json["blocked_reasons"]["infra"], json!(1));
    assert_eq!(status.json["runs"]["running"], json!(0));
    assert_eq!(status.json["runs"]["succeeded"], json!(1));
    assert_eq!(status.json["throttle"]["active_slots"], json!(1));
    assert_eq!(status.json["throttle"]["slot_lock_files"], json!(1));
    assert_eq!(status.json["throttle"]["draining"], json!(false));

    // Text mode prints scope, backlog/cancelled, blocked_reasons, active_slots and draining.
    let text_out = Command::new(env!("CARGO_BIN_EXE_aaf"))
        .arg("status")
        .current_dir(&fx.repo)
        .env_clear()
            .env("AAF_NO_BUILTIN_DEFAULTS", "1")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", fx.root().join("home"))
        .env("AAF_DATA_DIR", &fx.data)
        .env("AAF_WORKTREE_DIR", &fx.worktrees)
        .output()
        .unwrap();
    assert!(text_out.status.success());
    let text = String::from_utf8(text_out.stdout).unwrap();
    assert!(text.contains("scope:"), "{text}");
    assert!(text.contains("backlog="), "{text}");
    assert!(text.contains("cancelled="), "{text}");
    assert!(text.contains("blocked_reasons: infra=1"), "{text}");
    assert!(text.contains("active_slots=1"), "{text}");
    assert!(text.contains("draining=false"), "{text}");

    drop(lock);

    // Outside git repo, scope states `all (outside git repo)`.
    let outside = fx.aaf_in(&fx.root(), &["status"], &[]);
    assert_eq!(outside.code, 0, "{}", outside.stderr);
    assert_eq!(outside.json["scope"], json!("all (outside git repo)"));

    let trace = fx.aaf(&["board", "trace", &id]);
    assert_eq!(trace.code, 0, "{}", trace.stderr);
    assert_eq!(trace.json["trace"]["task_id"], json!(id));
    assert_eq!(trace.json["trace"]["total_wall_secs"], json!(12));
    assert_eq!(trace.json["trace"]["events_truncated"], json!(false));
    assert!(trace.json["trace"]["entries"].as_array().unwrap().len() >= 3);

    let truncated = fx.aaf(&["board", "trace", &id, "--limit", "1"]);
    assert_eq!(truncated.code, 0, "{}", truncated.stderr);
    assert_eq!(truncated.json["events_truncated"], json!(true));
    assert_eq!(truncated.json["trace"]["events_truncated"], json!(true));

    fs::write(fx.repo.join("notes.txt"), "architecture context").unwrap();
    let plan = fx.aaf(&[
        "plan",
        "Add retry jitter",
        "--file",
        "notes.txt",
        "--dry-run",
    ]);
    assert_eq!(plan.code, 0, "{}", plan.stderr);
    assert_eq!(plan.json["read_only"], json!(true));
    assert!(plan.json["prompt"].as_str().unwrap().contains("READ-ONLY"));
    assert!(
        plan.json["prompt"]
            .as_str()
            .unwrap()
            .contains("architecture context")
    );
}

#[test]
fn status_approval_reports_clear_missing_and_signed_state() {
    let fx = Fixture::new();
    // Initial clean tree -> clear.
    let clear = fx.aaf(&["status", "--approval"]);
    assert_eq!(clear.code, 0, "{}", clear.stderr);
    assert_eq!(clear.json["approval"]["status"], json!("clear"));
    assert_eq!(clear.json["approval"]["approved"], json!(false));

    // Add and commit a locked test file on main, then modify it -> missing.
    fs::create_dir_all(fx.repo.join("tests")).unwrap();
    fs::write(fx.repo.join("tests/sample_test.rs"), "// v1\n").unwrap();
    for args in [
        &["add", "tests/sample_test.rs"][..],
        &["commit", "-m", "add test"][..],
        &["checkout", "-b", "feat/edit-locked"][..],
    ] {
        let st = Command::new("git")
            .args(args)
            .current_dir(&fx.repo)
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?}");
    }
    fs::write(fx.repo.join("tests/sample_test.rs"), "// v2\n").unwrap();

    let missing = fx.aaf(&["status", "--approval"]);
    assert_eq!(missing.code, 0, "{}", missing.stderr);
    assert_eq!(missing.json["approval"]["status"], json!("missing"));
    assert_eq!(missing.json["approval"]["approved"], json!(false));
    assert_eq!(
        missing.json["approval"]["independent_approved"],
        json!(false)
    );
    assert!(
        !missing.json["approval"]["lock_fp"]
            .as_str()
            .unwrap()
            .is_empty()
    );

    // Sign an independent approval -> approved & independent_approved = true.
    let mut cfg = aaf_domain::config::RepoConfig::default();
    cfg.gate.base_ref = "main".to_string();
    struct FixedClock(i64);
    impl aaf_domain::ports::Clock for FixedClock {
        fn now(&self) -> i64 {
            self.0
        }
    }
    aaf_engine::gate::head_lock::approve(
        &fx.data,
        &fx.repo,
        &cfg,
        "antigravity-audit",
        false,
        &FixedClock(100),
    )
    .unwrap();

    let signed = fx.aaf(&["status", "--approval"]);
    assert_eq!(signed.code, 0, "{}", signed.stderr);
    assert_eq!(signed.json["approval"]["status"], json!("approved"));
    assert_eq!(signed.json["approval"]["approved"], json!(true));
    assert_eq!(signed.json["approval"]["independent_approved"], json!(true));
    assert_eq!(
        signed.json["approval"]["reviewer"],
        json!("antigravity-audit")
    );
}
