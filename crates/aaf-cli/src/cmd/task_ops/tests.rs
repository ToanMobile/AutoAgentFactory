use super::*;
use aaf_domain::task::{ImmutableSpec, Priority};

#[test]
fn unblock_task_appends_unblocked_note_and_transitions_to_ready() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    let mut task = Task::new_root(
        "t-blk",
        "/repo",
        "Blocked task",
        ImmutableSpec {
            description: "desc".into(),
            acceptance: vec!["ok".into()],
        },
        Priority::P2,
        10,
    );
    task.state = TaskState::Blocked;
    task.blocked_reason = Some(BlockedReason::Stuck {
        detail: "two fails".into(),
    });
    db.insert_task(&task).unwrap();

    let out = unblock_task(&db, "t-blk", "aaf board unblock", "human", 20).unwrap();
    assert!(!out.budget_reset);
    assert!(!out.plan_approved);

    let updated = db.get_task("t-blk").unwrap();
    assert_eq!(updated.state, TaskState::Ready);
    assert!(
        updated
            .context_notes
            .iter()
            .any(|n| n == "unblocked: aaf board unblock"),
        "missing unblocked note: {:?}",
        updated.context_notes
    );
}

#[test]
fn unblock_budget_exceeded_preserves_already_approved_plan() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    let mut task = Task::new_root(
        "t-bud",
        "/repo",
        "Budget blocked task",
        ImmutableSpec {
            description: "desc".into(),
            acceptance: vec!["ok".into()],
        },
        Priority::P1,
        10,
    );
    task.state = TaskState::Blocked;
    task.blocked_reason = Some(BlockedReason::BudgetExceeded {
        detail: "root token budget exceeded".into(),
    });
    task.context_notes
        .push("plan_approved: approved plan [plan_contract_hash=abc]".into());
    db.insert_task(&task).unwrap();
    let out = unblock_task(&db, "t-bud", "aaf board unblock", "human", 20).unwrap();
    assert!(
        out.plan_approved,
        "unblocking a budget_exceeded task with plan_approved note must preserve the plan"
    );
    let events = db.events_for_task("t-bud", 10).unwrap();
    assert!(
        !aaf_engine::plan_debate::state::is_human_unblock_event(&events[0]),
        "preserved plan unblock must not wipe the converged plan epoch"
    );
}

#[test]
fn clear_stale_receipts_preserves_sibling_main_approvals() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let data = tmp.path().join("data");
    let worktrees = tmp.path().join("worktrees");
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(&data).unwrap();
    let db = Db::open(&data.join("aaf.db")).unwrap();
    let task = Task::new_root(
        "t1",
        repo.to_string_lossy().into_owned(),
        "Task 1",
        ImmutableSpec {
            description: "desc".into(),
            acceptance: vec![],
        },
        Priority::P2,
        1,
    );
    let wt = worktree_path(&repo, &worktrees, &task.id).unwrap();
    fs::create_dir_all(&wt).unwrap();
    let main_dir = data.join("approvals").join(aaf_fp::repo_id(&repo));
    let wt_dir = data.join("approvals").join(aaf_fp::repo_id(&wt));
    fs::create_dir_all(&main_dir).unwrap();
    fs::create_dir_all(&wt_dir).unwrap();
    fs::write(wt_dir.join("fp-task.json"), "{}").unwrap();
    fs::write(main_dir.join("fp-task.json"), "{}").unwrap();
    fs::write(main_dir.join("fp-sibling.json"), "{}").unwrap();

    let ctx = Ctx {
        cwd: repo.clone(),
        home: tmp.path().join("home"),
        data_dir: data,
        config_dir: tmp.path().join("config"),
        worktrees_dir: worktrees,
        path_var: std::ffi::OsString::new(),
        run_id: None,
        stdin_is_tty: false,
        aaf_env: Vec::new(),
    };
    clear_stale_receipts(&db, &task, &ctx);

    assert!(!wt_dir.exists());
    assert!(!main_dir.join("fp-task.json").exists());
    assert!(main_dir.join("fp-sibling.json").exists());
}

#[test]
fn approve_if_unchanged_refuses_a_locked_diff_edited_after_preview() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let repo = root.join("repo");
    fs::create_dir_all(repo.join("tests")).unwrap();
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    fs::write(repo.join("tests/it.rs"), "// v1\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "init"]);
    fs::write(repo.join("tests/it.rs"), "// v2\n").unwrap();
    let ctx = Ctx {
        cwd: repo.clone(),
        home: root.join("home"),
        data_dir: root.join("data"),
        config_dir: root.join("config"),
        worktrees_dir: root.join("worktrees"),
        path_var: std::env::var_os("PATH").unwrap_or_default(),
        run_id: None,
        stdin_is_tty: false,
        aaf_env: Vec::new(),
    };

    let (_, preview) = eval_tree_head_lock(&ctx, &repo).unwrap();
    assert!(!preview.locked_changes.is_empty() && !preview.lock_fp.is_empty());
    fs::write(repo.join("tests/it.rs"), "// v3 after preview\n").unwrap();

    let refused = approve_if_unchanged(&ctx, &repo, &preview.lock_fp, "alice");
    assert!(
        matches!(refused, Err(ApproveRefusal::FpChanged)),
        "{refused:?}"
    );
    assert!(
        !ctx.data_dir.join("approvals").exists(),
        "an approval was written for a diff the reviewer never saw"
    );

    // The diff the reviewer did see is signed.
    let (_, now) = eval_tree_head_lock(&ctx, &repo).unwrap();
    let signed = approve_if_unchanged(&ctx, &repo, &now.lock_fp, "alice").unwrap();
    assert_eq!(signed.lock_fp, now.lock_fp);
    assert!(ctx.data_dir.join("approvals").exists());
}
