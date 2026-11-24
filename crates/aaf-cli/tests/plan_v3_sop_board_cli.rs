#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Plan v3 Wave 4 Slice 1 CLI tests:
//! - `aaf board ready <id>` and `aaf board ready --all`
//! - `aaf board unblock --all`
//! - `aaf board approve-plan --all`
//! - `aaf board undo --yes` (non-interactive + `AAF_RUN_ID` refusal)
//! - `aaf sop run` base-SHA custom SOP loading, worktree isolation, and `aaf sop approve --resume`
//! - `aaf plan --prompt-only`

#[path = "factory_support.rs"]
mod support;

use std::fs;
use std::path::Path;

use aaf_domain::blocked::BlockedReason;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{EventSink, TaskStore};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::factory::snapshots::create_turn_snapshot_ref;
use aaf_engine::git::worktree;
use serde_json::json;
use support::{Fixture, git, init_repo};

fn insert_task_in_state(
    fx: &Fixture,
    id: &str,
    repo: &Path,
    title: &str,
    state: TaskState,
    blocked_reason: Option<BlockedReason>,
) {
    let db = fx.db();
    let mut task = Task::new_root(
        id,
        repo.to_string_lossy().into_owned(),
        title,
        ImmutableSpec {
            description: format!("spec for {title}"),
            acceptance: vec!["it works".to_string()],
        },
        Priority::P2,
        100,
    );
    task.state = state;
    task.blocked_reason = blocked_reason;
    db.insert_task(&task).unwrap();
}

#[test]
fn board_ready_moves_single_and_all_backlog_tasks_in_current_repo_only() {
    let fx = Fixture::new();
    let other = init_repo(&fx.root(), "other-repo");

    insert_task_in_state(
        &fx,
        "t-bl1",
        &fx.repo,
        "Backlog 1",
        TaskState::Backlog,
        None,
    );
    insert_task_in_state(
        &fx,
        "t-bl2",
        &fx.repo,
        "Backlog 2",
        TaskState::Backlog,
        None,
    );
    insert_task_in_state(
        &fx,
        "t-bl3",
        &fx.repo,
        "Backlog 3",
        TaskState::Backlog,
        None,
    );
    insert_task_in_state(
        &fx,
        "t-other",
        &other,
        "Other backlog",
        TaskState::Backlog,
        None,
    );

    let single = fx.aaf(&["board", "ready", "t-bl1"]);
    assert_eq!(single.code, 0, "{}", single.stderr);
    assert_eq!(single.json["task_id"], json!("t-bl1"));
    assert_eq!(single.json["state"], json!("ready"));
    assert_eq!(fx.state("t-bl1"), "ready");

    // Moving an already-ready task is refused with policy exit code 2.
    let again = fx.aaf(&["board", "ready", "t-bl1"]);
    assert_eq!(again.code, 2, "{}", again.stderr);
    assert_eq!(again.json["status"], json!("refused"));

    // Inside an agent run, `board ready` is refused.
    let inside = [("AAF_RUN_ID", "run-agent-1")];
    let refused = fx.aaf_in(&fx.repo, &["board", "ready", "--all"], &inside);
    assert_eq!(refused.code, 2, "{}", refused.stderr);
    assert_eq!(refused.json["status"], json!("refused"));
    assert_eq!(fx.state("t-bl2"), "backlog");

    // `--all` moves remaining backlog tasks in the current repo only.
    let all = fx.aaf(&["board", "ready", "--all"]);
    assert_eq!(all.code, 0, "{}", all.stderr);
    assert_eq!(all.json["all"], json!(true));
    assert_eq!(all.json["count"], json!(2));
    assert_eq!(fx.state("t-bl2"), "ready");
    assert_eq!(fx.state("t-bl3"), "ready");
    assert_eq!(fx.state("t-other"), "backlog");
}

#[test]
fn board_unblock_all_unblocks_multiple_blocked_tasks_in_current_repo_only() {
    let fx = Fixture::new();
    let other = init_repo(&fx.root(), "other-unblock");

    insert_task_in_state(
        &fx,
        "t-blk1",
        &fx.repo,
        "Blocked 1",
        TaskState::Blocked,
        Some(BlockedReason::NeedsHuman {
            why: "manual review".to_string(),
        }),
    );
    insert_task_in_state(
        &fx,
        "t-blk2",
        &fx.repo,
        "Blocked 2",
        TaskState::Blocked,
        Some(BlockedReason::Stuck {
            detail: "flaky".to_string(),
        }),
    );
    insert_task_in_state(
        &fx,
        "t-other-blk",
        &other,
        "Other blocked",
        TaskState::Blocked,
        Some(BlockedReason::Stuck {
            detail: "other repo".to_string(),
        }),
    );

    let inside = [("AAF_RUN_ID", "run-agent-2")];
    let refused = fx.aaf_in(&fx.repo, &["board", "unblock", "--all"], &inside);
    assert_eq!(refused.code, 2, "{}", refused.stderr);
    assert_eq!(refused.json["status"], json!("refused"));

    let out = fx.aaf(&["board", "unblock", "--all"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["all"], json!(true));
    assert_eq!(out.json["count"], json!(2));
    assert_eq!(fx.state("t-blk1"), "ready");
    assert_eq!(fx.state("t-blk2"), "ready");
    assert_eq!(fx.state("t-other-blk"), "blocked");

    let notes = fx.show("t-blk1")["task"]["context_notes"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        notes
            .iter()
            .any(|v| v.as_str() == Some("unblocked: aaf board unblock --all")),
        "{notes:?}"
    );

    // Second run with no blocked tasks remaining succeeds with count=0.
    let empty = fx.aaf(&["board", "unblock", "--all"]);
    assert_eq!(empty.code, 0, "{}", empty.stderr);
    assert_eq!(empty.json["count"], json!(0));
}

#[test]
fn board_approve_plan_all_approves_only_plan_gated_tasks() {
    let fx = Fixture::new();

    insert_task_in_state(
        &fx,
        "t-plan1",
        &fx.repo,
        "Plan 1",
        TaskState::Blocked,
        Some(BlockedReason::NeedsHuman {
            why: "plan approval required: review plan 1".to_string(),
        }),
    );
    insert_task_in_state(
        &fx,
        "t-plan2",
        &fx.repo,
        "Plan 2",
        TaskState::Blocked,
        Some(BlockedReason::NeedsHuman {
            why: "plan approval required: review plan 2".to_string(),
        }),
    );
    insert_task_in_state(
        &fx,
        "t-stuck",
        &fx.repo,
        "Unrelated blocked",
        TaskState::Blocked,
        Some(BlockedReason::Stuck {
            detail: "not a plan gate".to_string(),
        }),
    );
    {
        let db = fx.db();
        db.emit(
            &FactoryEvent::new(
                EventKind::PlanGenerated,
                json!({ "plan": "1. Edit foo.rs" }).to_string(),
                101,
            )
            .for_task("t-plan1"),
        )
        .unwrap();
        db.emit(
            &FactoryEvent::new(
                EventKind::PlanGenerated,
                json!({ "plan": "1. Edit bar.rs" }).to_string(),
                102,
            )
            .for_task("t-plan2"),
        )
        .unwrap();
    }

    let inside = [("AAF_RUN_ID", "run-agent-3")];
    let refused = fx.aaf_in(&fx.repo, &["board", "approve-plan", "--all"], &inside);
    assert_eq!(refused.code, 2, "{}", refused.stderr);
    assert_eq!(refused.json["status"], json!("refused"));

    let out = fx.aaf(&["board", "approve-plan", "--all"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["all"], json!(true));
    assert_eq!(out.json["count"], json!(2));
    assert_eq!(out.json["approved"], json!(true));
    assert_eq!(fx.state("t-plan1"), "ready");
    assert_eq!(fx.state("t-plan2"), "ready");
    assert_eq!(fx.state("t-stuck"), "blocked");

    let notes = fx.show("t-plan1")["task"]["context_notes"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        notes
            .iter()
            .any(|v| v.as_str().is_some_and(|s| s.starts_with("plan_approved:"))),
        "{notes:?}"
    );
}

#[test]
fn board_undo_yes_works_non_interactively_and_refuses_inside_agent_run() {
    let fx = Fixture::new();
    insert_task_in_state(
        &fx,
        "t-undo1",
        &fx.repo,
        "Undo CLI test",
        TaskState::Review,
        None,
    );

    let wt = worktree::create(&fx.repo, &fx.worktrees, "t-undo1", "main").unwrap();
    fs::write(wt.path.join("work.txt"), "v1\n").unwrap();
    git(&wt.path, &["add", "work.txt"]);
    git(&wt.path, &["commit", "-q", "-m", "before turn"]);

    let (ref_name, oid) =
        create_turn_snapshot_ref(&fx.repo, &wt.path, "t-undo1", "run-1", 200).unwrap();
    fx.db()
        .insert_snapshot("t-undo1", "run-1", &ref_name, &oid, 200)
        .unwrap();

    fs::write(wt.path.join("work.txt"), "v2\n").unwrap();
    git(&wt.path, &["add", "work.txt"]);
    git(&wt.path, &["commit", "-q", "-m", "after turn"]);

    // Without `--yes` on non-TTY stdin, undo is refused.
    let no_tty = fx.aaf(&["board", "undo", "t-undo1"]);
    assert_eq!(no_tty.code, 2, "{}", no_tty.stderr);
    assert_eq!(no_tty.json["status"], json!("refused"));
    assert_eq!(no_tty.json["undone"], json!(false));

    // Even with `--yes`, inside an agent run (`AAF_RUN_ID`) undo is strictly refused.
    let inside = [("AAF_RUN_ID", "run-agent-undo")];
    let agent = fx.aaf_in(&fx.repo, &["board", "undo", "t-undo1", "--yes"], &inside);
    assert_eq!(agent.code, 2, "{}", agent.stderr);
    assert_eq!(agent.json["status"], json!("refused"));
    assert_eq!(
        fs::read_to_string(wt.path.join("work.txt")).unwrap(),
        "v2\n"
    );

    // With `--yes` outside an agent run, undo rolls back non-interactively.
    let ok = fx.aaf(&["board", "undo", "t-undo1", "--yes"]);
    assert_eq!(ok.code, 0, "{}", ok.stderr);
    assert_eq!(ok.json["undone"], json!(true));
    assert_eq!(ok.json["commit_oid"], json!(oid));
    assert_eq!(ok.json["state"], json!("ready"));
    assert_eq!(fx.state("t-undo1"), "ready");
    assert_eq!(
        fs::read_to_string(wt.path.join("work.txt")).unwrap(),
        "v1\n"
    );
}

#[test]
fn sop_run_loads_strictly_from_base_sha_isolates_worktree_and_resumes_on_approve() {
    let fx = Fixture::new();
    let sops_dir = fx.repo.join(".autofactory/sops");
    fs::create_dir_all(&sops_dir).unwrap();

    let committed_toml = r#"
name = "custom-safe"
version = 1
description = "Committed safe SOP"
profile = "auto"
max_reworks = 1

[[stages]]
id = "s1"
title = "Write marker in worktree"
kind = { type = "script", command = "echo safe_from_base > sop_marker.txt" }

[[stages]]
id = "s2"
title = "Human checkpoint"
depends_on = ["s1"]
kind = { type = "human_gate", prompt = "Approve custom-safe" }

[[stages]]
id = "s3"
title = "Finish in worktree"
depends_on = ["s2"]
kind = { type = "script", command = "echo after_approve >> sop_marker.txt" }
"#;
    fs::write(sops_dir.join("custom-safe.toml"), committed_toml).unwrap();
    git(&fx.repo, &["add", ".autofactory/sops/custom-safe.toml"]);
    git(&fx.repo, &["commit", "-q", "-m", "commit safe SOP"]);

    // Tamper with the working tree copy AND add an uncommitted SOP file.
    let tampered_toml = r#"
name = "custom-safe"
version = 1
description = "Tampered dirty SOP"
profile = "auto"
max_reworks = 0

[[stages]]
id = "pwned"
title = "Tampered stage"
kind = { type = "script", command = "echo PWNED > pwned.txt" }
"#;
    fs::write(sops_dir.join("custom-safe.toml"), tampered_toml).unwrap();
    fs::write(
        sops_dir.join("dirty-only.toml"),
        tampered_toml.replace("custom-safe", "dirty-only"),
    )
    .unwrap();

    // 1. `--dry-run` loads strictly from `base_sha` and creates no worktree or checkpoint.
    let dry = fx.aaf(&["sop", "run", "custom-safe", "--dry-run"]);
    assert_eq!(dry.code, 0, "{}", dry.stderr);
    assert_eq!(dry.json["dry_run"], json!(true));
    assert_eq!(dry.json["order"], json!(["s1", "s2", "s3"]));
    assert!(
        dry.json["source"]
            .as_str()
            .unwrap()
            .ends_with(":.autofactory/sops/custom-safe.toml"),
        "{}",
        dry.json["source"]
    );
    assert!(!fx.worktrees.exists(), "dry-run must not create worktrees");

    // 2. Uncommitted dirty-only SOP is completely ignored.
    let dirty_only = fx.aaf(&["sop", "run", "dirty-only", "--dry-run"]);
    assert_eq!(dirty_only.code, 1, "{}", dirty_only.stderr);

    // 3. Real `aaf sop run` runs `s1` in an isolated `aaf/sop-*` worktree and pauses at `s2`.
    let run = fx.aaf(&["sop", "run", "custom-safe"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.json["run_status"], json!("waiting_human"));
    let run_id = run.json["run_id"].as_str().unwrap().to_string();
    let wt_path_str = run.json["worktree_path"].as_str().unwrap().to_string();
    let branch = run.json["branch"].as_str().unwrap().to_string();
    assert!(branch.starts_with("aaf/sop-"), "{branch}");
    let wt_path = Path::new(&wt_path_str);
    assert!(wt_path.is_dir(), "expected SOP worktree at {wt_path_str}");
    assert!(
        !fx.repo.join("sop_marker.txt").exists(),
        "script stage must not mutate main checkout"
    );
    assert!(
        !fx.repo.join("pwned.txt").exists() && !wt_path.join("pwned.txt").exists(),
        "dirty working-tree SOP tampering must never execute"
    );
    assert_eq!(
        fs::read_to_string(wt_path.join("sop_marker.txt")).unwrap(),
        "safe_from_base\n"
    );

    // 4. `aaf sop approve --resume` approves `s2` and runs `s3` in 1 command.
    let resumed = fx.aaf(&["sop", "approve", &run_id, "--reviewer", "alice", "--resume"]);
    assert_eq!(resumed.code, 0, "{}", resumed.stderr);
    assert_eq!(resumed.json["approved_stage"], json!("s2"));
    assert_eq!(resumed.json["reviewer"], json!("alice"));
    assert_eq!(resumed.json["resumed"], json!(true));
    assert_eq!(resumed.json["run_status"], json!("completed"));
    assert_eq!(
        fs::read_to_string(wt_path.join("sop_marker.txt")).unwrap(),
        "safe_from_base\nafter_approve\n"
    );
}

#[test]
fn plan_prompt_only_prints_read_only_prompt_without_creating_db() {
    let fx = Fixture::new();
    let out = fx.aaf(&["plan", "Add structured logging", "--prompt-only"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["read_only"], json!(true));
    assert_eq!(out.json["prompt_only"], json!(true));
    assert_eq!(out.json["enqueued"], json!(false));
    let prompt = out.json["prompt"].as_str().unwrap();
    assert!(prompt.contains("READ-ONLY"), "{prompt}");
    assert!(prompt.contains("Add structured logging"), "{prompt}");
    assert!(!fx.data.join("aaf.db").exists());
}
