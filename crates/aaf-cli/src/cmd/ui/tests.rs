#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Instant;

use aaf_engine::throttle::Pressure;

use super::*;

fn test_backend(tmp: &Path) -> CliUiBackend {
    let ctx = Ctx {
        cwd: tmp.to_path_buf(),
        home: tmp.join("home"),
        data_dir: tmp.join("data"),
        config_dir: tmp.join("config"),
        worktrees_dir: tmp.join("worktrees"),
        path_var: std::ffi::OsString::new(),
        run_id: None,
        stdin_is_tty: false,
        aaf_env: Vec::new(),
    };
    CliUiBackend {
        ro_db: SharedRoDb::for_ctx(&ctx),
        ctx: ctx.clone(),
        scope: "all".into(),
        repo_root: None,
        config_base: tmp.to_path_buf(),
        read_only: false,
        allow_approve: false,
        supervisor: supervisor::UiSupervisor::new(ctx.clone(), None, false),
        server_started_at: 0,
        caches: report_cache::UiCaches::default(),
    }
}

#[test]
fn status_cache_returns_cached_value_until_invalidated() {
    let tmp = tempfile::tempdir().unwrap();
    let backend = test_backend(tmp.path());
    let first = backend.status_json().unwrap();
    assert_eq!(first["scope"], json!("all"));
    assert!(backend.caches.status.lock().unwrap().entry.is_some());
    assert!(backend.caches.pressure.lock().unwrap().is_some());
    *backend.caches.pressure.lock().unwrap() = Some((Instant::now(), Some(Pressure::Warn)));
    assert_eq!(backend.caches.cached_pressure(), Some(Pressure::Warn));
    let sentinel = Some((Instant::now(), json!({ "sentinel": 1 })));
    backend.caches.status.lock().unwrap().entry = sentinel;
    assert_eq!(backend.status_json().unwrap()["sentinel"], json!(1));
    backend.invalidate_status_cache();
    assert!(backend.caches.status.lock().unwrap().entry.is_none());
    let refreshed = backend.status_json().unwrap();
    assert_eq!(refreshed["scope"], json!("all"));
    assert!(refreshed.get("sentinel").is_none());
    assert_eq!(refreshed["heavy_slots"]["memory_pressure"], json!("warn"));
}

#[test]
fn invalidation_during_inflight_refresh_skips_stale_cache_write() {
    let tmp = tempfile::tempdir().unwrap();
    let backend = test_backend(tmp.path());
    let start_gen = backend.caches.cached_status_or_gen().unwrap_err();
    backend.invalidate_status_cache();
    backend
        .caches
        .store_status_if_current(start_gen, &json!({ "stale": true }));
    assert!(backend.caches.cached_status_or_gen().is_err());
    let cur_gen = backend.caches.cached_status_or_gen().unwrap_err();
    assert_ne!(cur_gen, start_gen);
    backend
        .caches
        .store_status_if_current(cur_gen, &json!({ "fresh": true }));
    assert_eq!(
        backend.caches.cached_status_or_gen().ok(),
        Some(json!({ "fresh": true }))
    );
}

#[test]
fn service_token_is_created_once_with_0600_and_reused() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("cfg");
    let a = service::load_or_create_token(&dir).unwrap();
    assert!(service::valid_token(&a));
    let b = service::load_or_create_token(&dir).unwrap();
    assert_eq!(a, b, "same token across restarts");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(service::token_path(&dir))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(dir_mode & 0o077, 0);
    }
}

#[test]
fn service_malformed_token_is_replaced_and_symlink_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(service::token_path(tmp.path()), "short\n").unwrap();
    let t = service::load_or_create_token(tmp.path()).unwrap();
    assert!(service::valid_token(&t));
    assert_eq!(
        std::fs::read_to_string(service::token_path(tmp.path())).unwrap(),
        t
    );
    #[cfg(unix)]
    {
        let target = tmp.path().join("target.txt");
        std::fs::write(&target, "0123456789abcdef0123456789abcdef").unwrap();
        let sym_dir = tmp.path().join("sym");
        std::fs::create_dir_all(&sym_dir).unwrap();
        std::os::unix::fs::symlink(&target, service::token_path(&sym_dir)).unwrap();
        let err = service::load_or_create_token(&sym_dir).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }
}

#[test]
fn service_bookmark_plist_and_port_in_use_behave_safely() {
    assert_eq!(
        service::bookmark_url(DEFAULT_PORT, "ab"),
        format!("http://127.0.0.1:{DEFAULT_PORT}/#token=ab")
    );
    let xml = service::plist_xml(
        Path::new("/Users/x/.cargo/bin/aaf"),
        Path::new("/Users/x"),
        "/usr/bin:/bin&x:/usr/bin:",
        &[("AAF_CONFIG_DIR", "/tmp/cfg".into())],
        false,
    );
    assert!(xml.contains("<string>/Users/x/.cargo/bin/aaf</string>"));
    assert!(xml.contains("<string>--service</string>"));
    assert!(!xml.contains("--allow-actions"));
    assert!(xml.contains(service::SERVICE_LABEL));
    assert!(xml.contains("<string>/usr/bin:/bin&amp;x</string>"));
    assert!(xml.contains("<key>AAF_CONFIG_DIR</key>"));
    assert!(!xml.contains("token") && !xml.contains("--key") && !xml.contains("AAF_UI_TOKEN"));

    let xml_act = service::plist_xml(
        Path::new("/Users/x/.cargo/bin/aaf"),
        Path::new("/Users/x"),
        "/usr/bin",
        &[],
        true,
    );
    assert!(xml_act.contains("<string>--allow-actions</string>"));
    assert!(!xml_act.contains("--allow-approve"));

    let tmp = tempfile::tempdir().unwrap();
    let none = service::port_in_use_message(DEFAULT_PORT, false, tmp.path());
    assert!(none.contains("--port") && !none.contains("--service-url"));
    service::load_or_create_token(tmp.path()).unwrap();
    let with_svc = service::port_in_use_message(DEFAULT_PORT, false, tmp.path());
    assert!(with_svc.contains("--service-url") && with_svc.contains("4319"));
    assert!(service::port_in_use_message(DEFAULT_PORT, true, tmp.path()).contains("launchd"));
}

#[test]
fn service_token_migrates_legacy_config_dir_into_data_dir_and_deletes_old() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let legacy = service::token_path(&cfg_dir);
    let orig = "0123456789abcdef0123456789abcdef";
    std::fs::write(&legacy, orig).unwrap();

    let tok = service::load_or_migrate_token(&data_dir, &cfg_dir).unwrap();
    assert_eq!(tok, orig);
    assert!(service::token_path(&data_dir).is_file());
    assert!(
        !legacy.exists(),
        "legacy config_dir token must be removed after migration"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(service::token_path(&data_dir))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn seed_service_and_supervisor_forward_worktree_dir_and_seed_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let default_wt = home.join(aaf_engine::executors::DEFAULT_WORKTREES_REL);
    let custom_wt = tmp.path().join("custom-worktrees");

    // 1. Default worktrees_dir and no AAF_SEED_CACHE -> neither appears in non_default_service_env.
    let ctx_default = Ctx {
        cwd: tmp.path().to_path_buf(),
        home: home.clone(),
        data_dir: home.join(".local/share/autofactory"),
        config_dir: home.join(".config/autofactory"),
        worktrees_dir: default_wt,
        path_var: std::ffi::OsString::from("/usr/bin"),
        run_id: None,
        stdin_is_tty: false,
        aaf_env: Vec::new(),
    };
    let env_def = service::non_default_service_env_with_toolchain(&ctx_default, &[]);
    assert!(
        env_def.is_empty(),
        "expected empty default env, got {env_def:?}"
    );

    let sdk_dir = tmp.path().join("android-sdk");
    let jdk_dir = tmp.path().join("openjdk@17/Contents/Home");
    std::fs::create_dir_all(&sdk_dir).unwrap();
    std::fs::create_dir_all(&jdk_dir).unwrap();
    let env_sdk = service::non_default_service_env_with_toolchain(
        &ctx_default,
        &[
            ("ANDROID_HOME".to_string(), sdk_dir.display().to_string()),
            ("JAVA_HOME".to_string(), jdk_dir.display().to_string()),
            ("ANDROID_SDK_ROOT".to_string(), "relative/path".to_string()),
            (
                "GRADLE_USER_HOME".to_string(),
                "http://user:pass@proxy:3128".to_string(),
            ),
            ("ANTHROPIC_API_KEY".to_string(), "sk-secret".to_string()),
        ],
    );
    assert!(
        env_sdk.contains(&("ANDROID_HOME", sdk_dir.display().to_string()))
            && env_sdk.contains(&("JAVA_HOME", jdk_dir.display().to_string())),
        "valid toolchain dirs must be forwarded into plist env: {env_sdk:?}"
    );
    assert!(
        !env_sdk.iter().any(|(k, _)| matches!(
            *k,
            "ANDROID_SDK_ROOT" | "GRADLE_USER_HOME" | "ANTHROPIC_API_KEY"
        )),
        "relative paths, proxy userinfo, and secrets must be ignored: {env_sdk:?}"
    );

    // 2. Custom worktrees_dir + AAF_SEED_CACHE=0 -> both forwarded by service and supervisor.
    let ctx_custom = Ctx {
        worktrees_dir: custom_wt.clone(),
        aaf_env: vec![("AAF_SEED_CACHE".to_string(), "0".to_string())],
        ..ctx_default
    };
    let svc_env = service::non_default_service_env_with_toolchain(&ctx_custom, &[]);
    assert!(
        svc_env.contains(&("AAF_WORKTREE_DIR", custom_wt.display().to_string())),
        "service must forward non-default AAF_WORKTREE_DIR: {svc_env:?}"
    );
    assert!(
        svc_env.contains(&("AAF_SEED_CACHE", "0".to_string())),
        "service must forward AAF_SEED_CACHE: {svc_env:?}"
    );

    let sup_env = supervisor::runner_env(&ctx_custom);
    assert!(
        sup_env
            .iter()
            .any(|(k, v)| *k == "AAF_WORKTREE_DIR" && v == custom_wt.as_os_str()),
        "supervisor must forward AAF_WORKTREE_DIR: {sup_env:?}"
    );
    assert!(
        sup_env
            .iter()
            .any(|(k, v)| *k == "AAF_SEED_CACHE" && v == "0"),
        "supervisor must forward AAF_SEED_CACHE: {sup_env:?}"
    );
}

#[test]
fn do_delete_done_archives_terminal_tasks_and_enforces_strict_validation() {
    use aaf_domain::ports::TaskStore;
    use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
    use aaf_engine::db::Db;

    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    let mk = |id: &str, repo: &str, state: TaskState| {
        let mut t = Task::new_root(
            id,
            repo,
            format!("Task {id}"),
            ImmutableSpec {
                description: "spec".into(),
                acceptance: vec!["ac".into()],
            },
            Priority::P2,
            100,
        );
        t.state = state;
        t
    };
    db.insert_task(&mk("t-done1", "/repo/a", TaskState::Done))
        .unwrap();
    db.insert_task(&mk("t-canc1", "/repo/a", TaskState::Cancelled))
        .unwrap();
    db.insert_task(&mk("t-ready1", "/repo/a", TaskState::Ready))
        .unwrap();
    db.insert_task(&mk("t-done2", "/repo/b", TaskState::Done))
        .unwrap();

    // Empty note must be refused (no wildcard multi-repo wipe).
    let err_empty = actions::do_delete_done(&db, None, "", "").unwrap_err();
    assert!(err_empty.starts_with("invalid delete_done:"), "{err_empty}");

    // Non-empty task_id must be refused (prevents silent wildcard wipe).
    let err_tid = actions::do_delete_done(&db, None, "t-done1", "").unwrap_err();
    assert!(err_tid.starts_with("invalid delete_done:"), "{err_tid}");

    // Batch > 500 must be refused.
    let huge: Vec<String> = (0..501).map(|i| format!("t-{i}")).collect();
    let huge_json = serde_json::to_string(&huge).unwrap();
    let err_batch = actions::do_delete_done(&db, None, "", &huge_json).unwrap_err();
    assert!(err_batch.contains("exceeds maximum"), "{err_batch}");

    // Invalid task_id in JSON array must be rejected before touching DB.
    let err = actions::do_delete_done(&db, Some("/repo/a"), "", r#"["bad id!"]"#).unwrap_err();
    assert!(err.contains("invalid task id"), "{err}");

    // Explicit list archives only terminal in-scope tasks; hidden from board_json, kept in DB.
    let res = actions::do_delete_done(
        &db,
        Some("/repo/a"),
        "",
        r#"["t-done1","t-canc1","t-ready1","t-done2"]"#,
    )
    .unwrap();
    assert_eq!(res["archived"], true);
    assert_eq!(res["deleted_count"], 2);
    assert_eq!(
        res["deleted_ids"],
        serde_json::json!(["t-canc1", "t-done1"])
    );
    assert_eq!(res["skipped_count"], 2);

    // Hidden from /api/board, preserved in DB for GC/Studio/cost_guard.
    let board = actions::build_board_json(Some(&db), "/repo/a", Some("/repo/a"), None).unwrap();
    let board_ids: Vec<&str> = board["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v["id"].as_str())
        .collect();
    assert_eq!(board_ids, vec!["t-ready1"]);
    assert!(db.get_task("t-done1").is_ok());
    assert!(db.get_task("t-done2").is_ok());
}

#[test]
fn plan_detail_falls_back_to_approved_context_note_after_human_unblock() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("state.db")).unwrap();
    let mut task = aaf_domain::task::Task::new_root(
        "t-plan-fb",
        "/repo/a",
        "Demo plan fallback",
        aaf_domain::task::ImmutableSpec {
            description: "Desc".into(),
            acceptance: vec!["ac".into()],
        },
        aaf_domain::task::Priority::P1,
        100,
    );
    task.context_notes
        .push("plan_approved: # Plan: Fix login\n- Update Login.kt\n- Add LoginTest".into());
    db.insert_task(&task).unwrap();
    db.emit(
        &aaf_domain::event::FactoryEvent::new(
            aaf_domain::event::EventKind::PlanGenerated,
            serde_json::json!({ "plan": "# Plan: Fix login\n- Update Login.kt" }).to_string(),
            101,
        )
        .for_task("t-plan-fb"),
    )
    .unwrap();
    // Human unblock starts a new epoch; latest_plan_info stops here, so UI must fall back to context_notes.
    db.emit(
        &aaf_domain::event::FactoryEvent::new(
            aaf_domain::event::EventKind::TaskStateChanged,
            serde_json::json!({ "by": "human", "to": "ready", "why": "aaf board unblock --all" })
                .to_string(),
            102,
        )
        .for_task("t-plan-fb"),
    )
    .unwrap();

    let detail = extras::plan_detail_json(&db, &task);
    assert_eq!(detail["available"], true);
    assert_eq!(detail["summary"], "Plan: Fix login");
    assert!(
        detail["note_body"]
            .as_str()
            .unwrap()
            .contains("- Update Login.kt")
    );
}
