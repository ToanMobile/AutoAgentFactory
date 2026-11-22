#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Proactive split CLI (`aaf board split`) and `aaf run --auto-split` / `--max-parallel auto`.

#[path = "factory_support.rs"]
mod support;

use std::fs;

use aaf_domain::blocked::BlockedReason;
use aaf_domain::ports::TaskStore;
use aaf_domain::task::{ImmutableSpec, Task, TaskState};
use serde_json::json;
use support::{Fixture, git};

const TWO_SUBTASKS_JSON: &str = r#"[{"key":"part-a","title":"Part A","description":"Implement part A","acceptance":["a.txt exists"],"owned_paths":["src/a.rs"],"depends_on":[]},{"key":"part-b","title":"Part B","description":"Implement part B","acceptance":["b.txt exists"],"owned_paths":["src/b.rs"],"depends_on":[]}]"#;

/// Commits `.autofactory/config.toml` with a custom executor running `script`
/// (receiving the prompt as `$0`) and `test` as the scoped test command.
fn configure(fx: &Fixture, script: &str, test: &str) {
    let dir = fx.repo.join(".autofactory");
    fs::create_dir_all(&dir).unwrap();
    let config = format!(
        "[commands]\ntest_scoped = {}\n\n[run]\nexecutor = \"custom\"\n\
         executor_cmd = [\"/bin/sh\", \"-c\", {}, \"{{prompt}}\"]\nrun_timeout_secs = 60\n",
        json!(test),
        json!(script)
    );
    fs::write(dir.join("config.toml"), config).unwrap();
    git(&fx.repo, &["add", "-A"]);
    git(
        &fx.repo,
        &["commit", "--quiet", "-m", "configure custom executor"],
    );
}

#[test]
fn board_split_refuses_inside_agent_run() {
    let fx = Fixture::new();
    configure(&fx, "echo '[]'", "true");
    let id = fx.add("split inside agent");

    let out = fx.aaf_in(
        &fx.repo,
        &["board", "split", &id, "--unsandboxed"],
        &[("AAF_RUN_ID", "run-abc")],
    );

    assert_eq!(out.code, 2, "{}", out.stderr);
    assert_eq!(out.json["status"], json!("refused"));
    assert_eq!(fx.db().list_tasks(None, None).unwrap().len(), 1);
}

#[test]
fn board_split_refuses_subtask_and_non_ready_states() {
    let fx = Fixture::new();
    configure(&fx, "echo '[]'", "true");

    let root_id = fx.add("root task");
    let done_id = fx.add("done task");
    let blocked_id = fx.add("blocked task");

    {
        let db = fx.db();
        let parent = db.get_task(&root_id).unwrap();
        let child = Task::new_subtask(
            format!("{root_id}-s1"),
            &parent,
            "existing subtask",
            ImmutableSpec {
                description: "child".to_string(),
                acceptance: vec!["ok".to_string()],
            },
            10,
        )
        .unwrap();
        db.insert_task(&child).unwrap();

        db.set_state(&done_id, TaskState::Running, None, 11)
            .unwrap();
        db.set_state(&done_id, TaskState::Gating, None, 12).unwrap();
        db.set_state(&done_id, TaskState::Done, None, 13).unwrap();

        let reason = BlockedReason::NeedsHuman {
            why: "manual".to_string(),
        };
        db.set_state(&blocked_id, TaskState::Blocked, Some(&reason), 14)
            .unwrap();
    }

    let sub_refused = fx.aaf(&["board", "split", &format!("{root_id}-s1"), "--unsandboxed"]);
    assert_eq!(sub_refused.code, 2, "{}", sub_refused.stderr);
    assert_eq!(sub_refused.json["status"], json!("refused"));

    let done_refused = fx.aaf(&["board", "split", &done_id, "--unsandboxed"]);
    assert_eq!(done_refused.code, 2, "{}", done_refused.stderr);
    assert_eq!(done_refused.json["status"], json!("refused"));

    let blocked_refused = fx.aaf(&["board", "split", &blocked_id, "--unsandboxed"]);
    assert_eq!(blocked_refused.code, 2, "{}", blocked_refused.stderr);
    assert_eq!(blocked_refused.json["status"], json!("refused"));
}

#[test]
fn board_split_dry_run_writes_nothing_and_live_split_creates_disjoint_subtasks() {
    let fx = Fixture::new();
    let script = format!("printf '%s\\n' '{TWO_SUBTASKS_JSON}'");
    configure(&fx, &script, "true");
    let parent_id = fx.add("multi-part feature");

    // Dry run previews both subtasks without modifying the database.
    let dry = fx.aaf(&["board", "split", &parent_id, "--dry-run", "--unsandboxed"]);
    assert_eq!(dry.code, 0, "{}", dry.stderr);
    assert_eq!(dry.json["ok"], json!(true));
    assert_eq!(dry.json["parent_id"], json!(parent_id));
    assert_eq!(dry.json["dry_run"], json!(true));
    assert_eq!(dry.json["split"], json!(true));
    let dry_subs = dry.json["subtasks"].as_array().unwrap();
    assert_eq!(dry_subs.len(), 2, "{}", dry.stderr);
    {
        let db = fx.db();
        assert_eq!(db.list_tasks(None, None).unwrap().len(), 1);
        assert!(db.children_of(&parent_id).unwrap().is_empty());
        assert!(db.deps_of(&parent_id).unwrap().is_empty());
    }

    // Live split writes both subtasks with owned_paths and wires DAG deps.
    let live = fx.aaf(&["board", "split", &parent_id, "--max", "2", "--unsandboxed"]);
    assert_eq!(live.code, 0, "{}", live.stderr);
    assert_eq!(live.json["ok"], json!(true));
    assert_eq!(live.json["parent_id"], json!(parent_id));
    assert_eq!(live.json["dry_run"], json!(false));
    assert_eq!(live.json["split"], json!(true));
    let live_subs = live.json["subtasks"].as_array().unwrap();
    assert_eq!(live_subs.len(), 2, "{}", live.stderr);
    assert_eq!(live_subs[0]["owned_paths"], json!(["src/a.rs"]));
    assert_eq!(live_subs[1]["owned_paths"], json!(["src/b.rs"]));

    let db = fx.db();
    let children = db.children_of(&parent_id).unwrap();
    assert_eq!(children.len(), 2);
    assert!(children.iter().all(|c| c.depth == 1));
    assert!(
        children
            .iter()
            .any(|c| c.spec.description.contains("src/a.rs"))
    );
    assert!(
        children
            .iter()
            .any(|c| c.spec.description.contains("src/b.rs"))
    );

    let deps = db.deps_of(&parent_id).unwrap();
    assert_eq!(deps.len(), 2);
    for child in &children {
        assert!(deps.contains(&child.id), "{deps:?} missing {}", child.id);
    }
}

#[test]
fn run_max_parallel_auto_parses_and_zero_is_rejected() {
    let fx = Fixture::new();
    configure(&fx, "true", "true");

    let zero = fx.aaf(&["run", "--unsandboxed", "--max-parallel", "0"]);
    assert_eq!(zero.code, 2, "{}", zero.stderr);
    assert_eq!(zero.json["status"], json!("usage_error"));

    let bad = fx.aaf(&["run", "--unsandboxed", "--max-parallel", "not-a-number"]);
    assert_eq!(bad.code, 2, "{}", bad.stderr);
    assert_eq!(bad.json["status"], json!("usage_error"));

    let auto = fx.aaf(&["run", "--unsandboxed", "--max-parallel", "auto"]);
    assert_eq!(auto.code, 0, "{}", auto.stderr);
    assert_eq!(auto.json["status"], json!("ok"));
    let parallel = auto.json["max_parallel"].as_u64().unwrap();
    assert!((1..=4).contains(&parallel), "max_parallel={parallel}");
    assert_eq!(auto.json["auto_split_count"], json!(0));

    let upper = fx.aaf(&["run", "--unsandboxed", "--max-parallel", "AUTO"]);
    assert_eq!(upper.code, 0, "{}", upper.stderr);
    assert!(upper.json["max_parallel"].as_u64().unwrap() >= 1);
}

#[test]
fn run_auto_split_with_max_parallel_auto_splits_and_runs_subtasks() {
    let fx = Fixture::new();
    let split_json = r#"[{"key":"part-a","title":"Part A","description":"Create a.txt","acceptance":["a.txt exists"],"owned_paths":["a.txt"],"depends_on":[]},{"key":"part-b","title":"Part B","description":"Create b.txt","acceptance":["b.txt exists"],"owned_paths":["b.txt"],"depends_on":[]}]"#;
    let script = format!(
        "case \"$0\" in \
         *\"READ-ONLY SPLIT PLANNING MODE\"*) printf '%s\\n' '{split_json}' ;; \
         *\"Part A\"*) echo a > a.txt ;; \
         *\"Part B\"*) echo b > b.txt ;; \
         *) echo root > root.txt ;; \
         esac"
    );
    configure(&fx, &script, "test -f a.txt -o -f b.txt");
    let parent_id = fx.add("split and execute in parallel");

    let out = fx.aaf(&[
        "run",
        "--unsandboxed",
        "--auto-split",
        "--max-parallel",
        "auto",
    ]);

    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["status"], json!("ok"));
    assert_eq!(out.json["auto_split_count"], json!(1), "{}", out.stderr);
    let parallel = out.json["max_parallel"].as_u64().unwrap();
    assert!((1..=4).contains(&parallel), "max_parallel={parallel}");
    assert_eq!(out.json["agent_runs"], json!(2), "{}", out.stderr);

    let db = fx.db();
    let children = db.children_of(&parent_id).unwrap();
    assert_eq!(children.len(), 2);
    for child in &children {
        assert_eq!(child.state, TaskState::Review, "child {}", child.id);
    }
    let deps = db.deps_of(&parent_id).unwrap();
    assert_eq!(deps.len(), 2);
    let parent = db.get_task(&parent_id).unwrap();
    assert_eq!(parent.state, TaskState::Ready);
    assert!(
        parent.context_notes.iter().any(|n| n.starts_with("split:")),
        "{:?}",
        parent.context_notes
    );
}

#[test]
fn board_split_runs_in_throwaway_worktree_and_checks_claude_hooks() {
    let fx = Fixture::new();
    // Attempt to write a marker file in cwd during split planning: main checkout must stay clean.
    let script = format!("echo dirty > planner-touch.txt; printf '%s\\n' '{TWO_SUBTASKS_JSON}'");
    configure(&fx, &script, "true");
    let id = fx.add("isolated split");

    let out = fx.aaf(&["board", "split", &id, "--dry-run", "--unsandboxed"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["split"], json!(true));
    assert!(
        !fx.repo.join("planner-touch.txt").exists(),
        "planner must run in a throwaway worktree, never in repo_root"
    );

    // Claude executor without committed .claude/settings.json hook fails closed.
    let fx2 = Fixture::new();
    let dir = fx2.repo.join(".autofactory");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("config.toml"),
        "[commands]\ntest_scoped = \"true\"\n\n[run]\nexecutor = \"claude\"\n",
    )
    .unwrap();
    git(&fx2.repo, &["add", "-A"]);
    git(&fx2.repo, &["commit", "--quiet", "-m", "claude executor"]);
    let id2 = fx2.add("claude missing hook");

    let refused = fx2.aaf(&["board", "split", &id2, "--dry-run", "--unsandboxed"]);
    assert_eq!(refused.code, 2, "{}", refused.stderr);
    assert_eq!(refused.json["status"], json!("refused"));
}

#[test]
fn run_auto_split_auto_merge_completes_container_parent_without_no_change() {
    let fx = Fixture::new();
    let split_json = r#"[{"key":"p1","title":"Part 1","description":"Create p1.txt","acceptance":["p1 exists"],"owned_paths":["p1.txt"],"depends_on":[]},{"key":"p2","title":"Part 2","description":"Create p2.txt","acceptance":["p2 exists"],"owned_paths":["p2.txt"],"depends_on":[]},{"key":"p3","title":"Part 3","description":"Create p3.txt","acceptance":["p3 exists"],"owned_paths":["p3.txt"],"depends_on":[]}]"#;
    let script = format!(
        "case \"$0\" in \
         *\"READ-ONLY SPLIT PLANNING MODE\"*) printf '%s\\n' '{split_json}' ;; \
         *\"Part 1\"*) echo 1 > p1.txt ;; \
         *\"Part 2\"*) echo 2 > p2.txt ;; \
         *\"Part 3\"*) echo 3 > p3.txt ;; \
         *) exit 0 ;; \
         esac"
    );
    configure(&fx, &script, "true");
    let parent_id = fx.add("three-way auto-merge split");

    let out = fx.aaf(&["run", "--unsandboxed", "--auto-split", "--auto-merge"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["status"], json!("ok"));
    assert_eq!(out.json["agent_runs"], json!(3), "{}", out.stderr);
    assert_eq!(out.json["frozen"], json!(false));

    let db = fx.db();
    let parent = db.get_task(&parent_id).unwrap();
    assert_eq!(parent.state, TaskState::Done);
    assert_eq!(parent.unblock_cycles, 0);
    assert!(
        !parent
            .context_notes
            .iter()
            .any(|n| n.contains("without changing anything")),
        "{:?}",
        parent.context_notes
    );
    for child in db.children_of(&parent_id).unwrap() {
        assert_eq!(child.state, TaskState::Done, "child {}", child.id);
    }
}
