#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Phase 1 v3.2 CLI tests: `board add --scope`, `board dep add|rm`, and `board graph`.

#[path = "factory_support.rs"]
mod support;

use std::process::Command;

use aaf_domain::blocked::BlockedReason;
use aaf_domain::ports::TaskStore;
use aaf_domain::task::TaskState;
use serde_json::json;
use support::{Fixture, init_repo};

fn block_task(fx: &Fixture, id: &str) {
    let db = fx.db();
    let reason = BlockedReason::NeedsHuman {
        why: "waiting on spec".to_string(),
    };
    db.set_state(id, TaskState::Blocked, Some(&reason), 10)
        .unwrap();
}

#[test]
fn board_add_validates_deduplicates_and_stores_scope_paths() {
    let fx = Fixture::new();
    let out = fx.aaf(&[
        "board",
        "add",
        "--title",
        "Scoped task",
        "--spec",
        "Touch api and db only",
        "--accept",
        "tests pass",
        "--scope",
        "./src/api/",
        "--scope",
        "src/db",
        "--scope",
        "SRC/API",
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["scope_paths"], json!(["src/api", "src/db"]));
    assert_eq!(
        out.json["task"]["scope_paths"],
        json!(["src/api", "src/db"])
    );

    let id = out.json["task"]["id"].as_str().unwrap();
    let shown = fx.show(id);
    assert_eq!(shown["task"]["scope_paths"], json!(["src/api", "src/db"]));

    // Invalid scopes must fail cleanly without inserting a task.
    for bad in ["../outside", "/etc/passwd", "src/*.rs", ""] {
        let res = fx.aaf(&[
            "board",
            "add",
            "--title",
            "Bad scope",
            "--spec",
            "s",
            "--accept",
            "a",
            "--scope",
            bad,
        ]);
        assert_eq!(
            res.code, 1,
            "expected error for --scope {bad:?}: {}",
            res.stderr
        );
    }
    assert_eq!(fx.aaf(&["board", "list"]).json["count"], json!(1));
}

#[test]
fn board_dep_add_and_rm_enforce_cross_repo_cycles_and_agent_run_guard() {
    let fx = Fixture::new();
    let a = fx.add("Task A");
    let b = fx.add("Task B");
    let c = fx.add("Task C");

    // Add valid edges: b blocked by a, c blocked by b.
    let add_ab = fx.aaf(&["board", "dep", "add", &b, "--blocked-by", &a]);
    assert_eq!(add_ab.code, 0, "{}", add_ab.stderr);
    assert_eq!(add_ab.json["status"], json!("added"));
    assert_eq!(add_ab.json["task_id"], json!(b));
    assert_eq!(add_ab.json["blocked_by"], json!(a));

    let add_bc = fx.aaf(&["board", "dep", "add", &c, "--blocked-by", &b]);
    assert_eq!(add_bc.code, 0, "{}", add_bc.stderr);

    // Cycle and self-dep must be rejected.
    let self_dep = fx.aaf(&["board", "dep", "add", &a, "--blocked-by", &a]);
    assert_eq!(self_dep.code, 1, "self-dep must fail: {}", self_dep.stderr);

    let cycle = fx.aaf(&["board", "dep", "add", &a, "--blocked-by", &c]);
    assert_eq!(cycle.code, 1, "cycle must fail: {}", cycle.stderr);
    assert!(
        cycle.json["error"].as_str().unwrap_or("").contains("cycle"),
        "{}",
        cycle.json
    );

    // Cross-repo dep must be rejected.
    let other_repo = init_repo(&fx.root(), "other");
    let other_add = fx.aaf_in(
        &other_repo,
        &[
            "board", "add", "--title", "Other", "--spec", "s", "--accept", "a",
        ],
        &[],
    );
    assert_eq!(other_add.code, 0, "{}", other_add.stderr);
    let other_id = other_add.json["task"]["id"].as_str().unwrap().to_string();

    let cross = fx.aaf(&["board", "dep", "add", &a, "--blocked-by", &other_id]);
    assert_eq!(cross.code, 1, "cross-repo dep must fail: {}", cross.stderr);
    assert!(
        cross.json["error"]
            .as_str()
            .unwrap_or("")
            .contains("cross-repo"),
        "{}",
        cross.json
    );

    // Inside AAF_RUN_ID, both add and rm must be refused (exit 2).
    let inside = [("AAF_RUN_ID", "run-999")];
    let ref_add = fx.aaf_in(
        &fx.repo,
        &["board", "dep", "add", &c, "--blocked-by", &a],
        &inside,
    );
    assert_eq!(ref_add.code, 2, "{}", ref_add.stderr);
    assert_eq!(ref_add.json["status"], json!("refused"));

    let ref_rm = fx.aaf_in(
        &fx.repo,
        &["board", "dep", "rm", &c, "--blocked-by", &b],
        &inside,
    );
    assert_eq!(ref_rm.code, 2, "{}", ref_rm.stderr);
    assert_eq!(ref_rm.json["status"], json!("refused"));

    // Remove existing edge, then verify second removal returns removed: false.
    let rm1 = fx.aaf(&["board", "dep", "rm", &c, "--blocked-by", &b]);
    assert_eq!(rm1.code, 0, "{}", rm1.stderr);
    assert_eq!(rm1.json["removed"], json!(true));

    let rm2 = fx.aaf(&["board", "dep", "rm", &c, "--blocked-by", &b]);
    assert_eq!(rm2.code, 0, "{}", rm2.stderr);
    assert_eq!(rm2.json["removed"], json!(false));
}

#[test]
fn board_graph_works_read_only_on_empty_db_and_renders_json_and_ascii() {
    let fx = Fixture::new();

    // Before any DB exists: board graph succeeds read-only and does NOT create aaf.db.
    let empty = fx.aaf(&["board", "graph"]);
    assert_eq!(empty.code, 0, "{}", empty.stderr);
    assert_eq!(empty.json["nodes"], json!([]));
    assert_eq!(empty.json["edges"], json!([]));
    assert_eq!(empty.json["topo_order"], json!([]));
    assert_eq!(empty.json["critical_path"], json!([]));
    assert_eq!(empty.json["blocked_upstream"], json!([]));
    assert!(
        !fx.data.join("aaf.db").exists(),
        "board graph must not create aaf.db"
    );

    // Build DAG: a (Blocked) <- b (Ready) <- c (Ready), plus solo task d.
    let a = fx.add("Root A");
    let b = fx.add("Middle B");
    let c = fx.add("Leaf C");
    let d = fx.add("Solo D");
    assert_eq!(
        fx.aaf(&["board", "dep", "add", &b, "--blocked-by", &a])
            .code,
        0
    );
    assert_eq!(
        fx.aaf(&["board", "dep", "add", &c, "--blocked-by", &b])
            .code,
        0
    );
    block_task(&fx, &a);

    // Also add a task in a second repo to verify default filtering vs --all.
    let other_repo = init_repo(&fx.root(), "other-graph");
    let other_add = fx.aaf_in(
        &other_repo,
        &[
            "board",
            "add",
            "--title",
            "Other Repo Task",
            "--spec",
            "s",
            "--accept",
            "a",
        ],
        &[],
    );
    assert_eq!(other_add.code, 0, "{}", other_add.stderr);

    let g = fx.aaf(&["board", "graph"]);
    assert_eq!(g.code, 0, "{}", g.stderr);
    assert_eq!(g.json["nodes"].as_array().unwrap().len(), 4);
    assert_eq!(g.json["critical_path"], json!([a, b, c]));

    let topo: Vec<&str> = g.json["topo_order"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let pos = |id: &str| topo.iter().position(|&x| x == id).unwrap();
    assert!(pos(&a) < pos(&b));
    assert!(pos(&b) < pos(&c));
    assert!(topo.contains(&d.as_str()));

    let blocked = g.json["blocked_upstream"].as_array().unwrap();
    assert_eq!(blocked.len(), 2);
    assert!(blocked.contains(&json!({ "task_id": b, "blocked_by": a })));
    assert!(blocked.contains(&json!({ "task_id": c, "blocked_by": a })));

    // --all includes the other repo's task.
    let all = fx.aaf(&["board", "graph", "--all"]);
    assert_eq!(all.code, 0, "{}", all.stderr);
    assert_eq!(all.json["nodes"].as_array().unwrap().len(), 5);

    // Human ASCII mode (without --json) includes [CRIT] and [CASCADE-BLOCKED].
    let ascii = Command::new(env!("CARGO_BIN_EXE_aaf"))
        .args(["board", "graph"])
        .current_dir(&fx.repo)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", fx.root().join("home"))
        .env("AAF_DATA_DIR", &fx.data)
        .env("AAF_WORKTREE_DIR", &fx.worktrees)
        .output()
        .unwrap();
    assert!(
        ascii.status.success(),
        "{}",
        String::from_utf8_lossy(&ascii.stderr)
    );
    let stdout = String::from_utf8_lossy(&ascii.stdout);
    assert!(stdout.contains("[CRIT]"), "missing [CRIT]:\n{stdout}");
    assert!(
        stdout.contains("[CASCADE-BLOCKED]"),
        "missing [CASCADE-BLOCKED]:\n{stdout}"
    );
    assert!(
        stdout.contains("critical path:"),
        "missing critical path footer:\n{stdout}"
    );
}
