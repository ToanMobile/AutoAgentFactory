use std::path::Path;

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::config::AccountSelection;
use aaf_engine::accounts::BindingSource;
use aaf_engine::factory::BindingView;
use aaf_engine::throttle::{Busy, Progress};

use super::progress::line;
use super::{WORK_STATES, has_work_in_db, report, sandbox_choice, waiting_line};
use crate::output::Exit;

#[test]
fn a_run_stopped_by_a_login_or_network_failure_says_so_and_exits_2() {
    let why = "t1: the agent CLI failed: its login or credentials were refused: \
               Not logged in. The run stopped";
    let summary = aaf_engine::factory::RunSummary {
        agent_runs: 1,
        requeued: vec!["t1".to_string()],
        halted: Some(why.to_string()),
        ..Default::default()
    };
    let out = report(&summary, &[], &binding(None)).unwrap();
    assert_eq!(out.status, "halted");
    assert_eq!(out.exit, Exit::Policy);
    assert!(
        out.text.ends_with(&format!("\nstopped: {why}")),
        "{}",
        out.text
    );
    assert_eq!(out.fields["halted"], serde_json::json!(why));

    // The neighbour: the same run without the stop is an ordinary result.
    let summary = aaf_engine::factory::RunSummary {
        halted: None,
        ..summary
    };
    let out = report(&summary, &[], &binding(None)).unwrap();
    assert_eq!(out.status, "ok");
    assert_eq!(out.exit, Exit::Ok);
}

fn binding(cooldown_until: Option<i64>) -> BindingView {
    BindingView {
        selection: AccountSelection::Pinned,
        account: Some(AccountProfile {
            id: "claude_code-0123456789ab".to_string(),
            provider: ProviderKind::ClaudeCode,
            config_dir: "/home/u/.claude-work".to_string(),
            name: "work".to_string(),
        }),
        source: Some(BindingSource::History),
        cooldown_until,
        settings_account: None,
        settings_account_known: false,
    }
}

#[test]
fn a_run_that_waits_for_the_repositorys_account_says_which_and_until_when() {
    assert_eq!(
        waiting_line(1, &binding(Some(1_900_000_000))).as_deref(),
        Some("waiting for account work (selection=pinned): in cooldown until 1900000000")
    );
    // Nothing waits, or the wait is not a cooldown: no line.
    assert_eq!(waiting_line(0, &binding(Some(1_900_000_000))), None);
    assert_eq!(waiting_line(1, &binding(None)), None);
}

#[test]
fn macos_sandboxes_unless_told_not_to() {
    assert_eq!(sandbox_choice(false, Ok(true)), Ok(true));
    assert_eq!(sandbox_choice(true, Ok(true)), Ok(false));
}

#[test]
fn without_seatbelt_running_unconfined_must_be_asked_for() {
    assert!(sandbox_choice(false, Ok(false)).is_err());
    assert_eq!(sandbox_choice(true, Ok(false)), Ok(false));
    let err = sandbox_choice(false, Err("bwrap smoke check failed".into())).unwrap_err();
    assert!(err.contains("bwrap smoke check failed"), "{err}");
    assert_eq!(
        sandbox_choice(true, Err("bwrap smoke check failed".into())),
        Ok(false)
    );
}

#[test]
fn a_wait_of_the_gate_is_one_line_that_names_the_tree_and_the_reason() {
    let tree = Path::new("/data/worktrees/repo-1/batch-0123456789ab");
    let slot = Progress::WaitingForSlot {
        label: "test_scoped",
        reason: Busy::SlotsTaken,
        waited_secs: 12,
    };
    assert_eq!(
        line(tree, &slot),
        "gate batch-0123456789ab: test_scoped: waiting for a heavy slot \
         (every heavy slot is taken), 12s"
    );
    let load = Progress::WaitingForLoad {
        label: "build",
        load_percent: 400,
        max_load_percent: 150,
        cpu_busy_percent: None,
        max_cpu_percent: None,
        waited_secs: 30,
        gives_up_after_secs: 1800,
    };
    assert_eq!(
        line(Path::new("/wt/t1"), &load),
        "gate t1: build: waiting for CPU load 400% to drop to 150%, 30s \
         (runs anyway after 1800s)"
    );
}

#[test]
fn a_kill_that_gave_up_names_the_group_that_may_still_run() {
    let unkilled = Progress::Unkilled {
        label: "gradlew",
        pgid: 4242,
    };
    assert_eq!(
        crate::cmd::gate::progress_line(&unkilled),
        "gradlew: process group 4242 still had members after repeated SIGKILL; \
         they may still be running (`ps -o pid,command -g 4242`)"
    );
}

#[test]
fn assigned_non_claude_account_routes_run_executor_to_gemini_or_grok() {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let bin = tmp.path().join("bin");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(home.join(".gemini")).unwrap();
    std::fs::create_dir_all(home.join(".grok")).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(repo.join(".claude")).unwrap();
    for tool in ["agy", "grok"] {
        let p = bin.join(tool);
        std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let ctx = crate::ctx::Ctx {
        cwd: repo.clone(),
        home: home.clone(),
        data_dir: tmp.path().join("data"),
        worktrees_dir: tmp.path().join("wt"),
        config_dir: tmp.path().join("cfg"),
        path_var: bin.as_os_str().to_os_string(),
        run_id: None,
        stdin_is_tty: false,
        aaf_env: Vec::new(),
    };

    // 1. Assign Grok -> switches executor from claude to grok, clears model & effort,
    //    and clears Claude-inherited pipeline writer stages.
    std::fs::write(
        repo.join(".claude/settings.local.json"),
        r#"{"account":"grok"}"#,
    )
    .unwrap();
    let mut cfg = ctx.load_config(&repo).unwrap();
    assert_eq!(cfg.repo.run.executor, "grok");
    cfg.repo.run.executor = "claude".into();
    cfg.repo.run.model = "claude-sonnet-4-6".into();
    cfg.repo.run.effort = "max".into();
    cfg.repo.pipeline.implementer.executor = "claude".into();
    cfg.repo.pipeline.implementer.model = "claude-sonnet-4-6".into();
    cfg.repo.pipeline.implementer.effort = "high".into();
    ctx.apply_assigned_account_executor(&repo, &mut cfg);
    assert_eq!(cfg.repo.run.executor, "grok");
    assert!(cfg.repo.run.model.is_empty());
    assert!(cfg.repo.run.effort.is_empty());
    assert!(cfg.repo.pipeline.implementer.executor.is_empty());
    assert!(cfg.repo.pipeline.implementer.model.is_empty());
    assert!(cfg.repo.pipeline.implementer.effort.is_empty());

    // 2. Assign Gemini (case-insensitive) -> switches executor from claude to antigravity
    std::fs::write(
        repo.join(".claude/settings.local.json"),
        r#"{"account":"GEMINI"}"#,
    )
    .unwrap();
    let mut cfg2 = ctx.load_config(&repo).unwrap();
    assert_eq!(cfg2.repo.run.executor, "antigravity");
    cfg2.repo.run.executor = "claude".into();
    cfg2.repo.run.model = "claude-opus-4-6".into();
    cfg2.repo.run.effort = "minimal".into();
    ctx.apply_assigned_account_executor(&repo, &mut cfg2);
    assert_eq!(cfg2.repo.run.executor, "antigravity");
    assert!(cfg2.repo.run.model.is_empty());
    assert!(cfg2.repo.run.effort.is_empty());

    // 3. Explicit custom executor_cmd OR explicit non-Claude [run].executor is NOT overwritten
    let mut cfg3 = ctx.load_config(&repo).unwrap();
    cfg3.repo.run.executor = "claude".into();
    cfg3.repo.run.executor_cmd = vec!["my-wrapper".into()];
    ctx.apply_assigned_account_executor(&repo, &mut cfg3);
    assert_eq!(cfg3.repo.run.executor, "claude");

    let mut cfg4 = ctx.load_config(&repo).unwrap();
    cfg4.repo.run.executor = "codex".into();
    cfg4.repo.run.model = "o3".into();
    ctx.apply_assigned_account_executor(&repo, &mut cfg4);
    assert_eq!(cfg4.repo.run.executor, "codex");
    assert_eq!(cfg4.repo.run.model, "o3");

    // 4. Gemini-only PATH (without agy) routes to "gemini" instead of crashing on "antigravity"
    let bin_gemini = tmp.path().join("bin-gemini");
    std::fs::create_dir_all(&bin_gemini).unwrap();
    let g = bin_gemini.join("gemini");
    std::fs::write(&g, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&g, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut ctx_gemini = ctx.clone();
    ctx_gemini.path_var = bin_gemini.as_os_str().to_os_string();
    let cfg5 = ctx_gemini.load_config(&repo).unwrap();
    assert_eq!(cfg5.repo.run.executor, "gemini");
}

fn insert_task(db: &aaf_engine::db::Db, id: &str, repo: &Path, state: aaf_domain::task::TaskState) {
    use aaf_domain::ports::TaskStore;
    use aaf_domain::task::{ImmutableSpec, Priority, Task};
    let spec = ImmutableSpec {
        description: "spec".into(),
        acceptance: vec!["ok".into()],
    };
    let mut task = Task::new_root(id, repo.to_string_lossy(), id, spec, Priority::P2, 10);
    task.state = state;
    db.insert_task(&task).unwrap();
}

#[test]
fn an_adopting_task_counts_as_outstanding_work_for_its_repository() {
    use aaf_domain::task::TaskState;
    let tmp = tempfile::tempdir().unwrap();
    let db = aaf_engine::db::Db::open(&tmp.path().join("aaf.db")).unwrap();
    let repo = tmp.path().join("repo");
    let other = tmp.path().join("other");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&other).unwrap();

    insert_task(&db, "t-done", &repo, TaskState::Done);
    insert_task(&db, "t-blocked", &repo, TaskState::Blocked);
    assert!(!has_work_in_db(&db, &repo).unwrap(), "nothing to do yet");

    insert_task(&db, "t-ad", &repo, TaskState::Adopting);
    assert!(has_work_in_db(&db, &repo).unwrap(), "Adopting is work");
    assert!(
        !has_work_in_db(&db, &other).unwrap(),
        "another repository's adopted PR is not this repository's work"
    );
    assert!(WORK_STATES.contains(&TaskState::Adopting));
}
