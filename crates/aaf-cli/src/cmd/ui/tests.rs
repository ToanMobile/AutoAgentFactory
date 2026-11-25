#![allow(clippy::unwrap_used, clippy::expect_used)]

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
        status_cache: Mutex::new(StatusCache::default()),
        status_refresh: Mutex::new(()),
        pressure_cache: Mutex::new(None),
    }
}

#[test]
fn status_cache_returns_cached_value_until_invalidated() {
    let tmp = tempfile::tempdir().unwrap();
    let backend = test_backend(tmp.path());
    let first = backend.status_json().unwrap();
    assert_eq!(first["scope"], json!("all"));
    assert!(backend.status_cache.lock().unwrap().entry.is_some());
    assert!(backend.pressure_cache.lock().unwrap().is_some());
    *backend.pressure_cache.lock().unwrap() = Some((Instant::now(), Some(Pressure::Warn)));
    assert_eq!(backend.cached_pressure(), Some(Pressure::Warn));
    let sentinel = Some((Instant::now(), json!({ "sentinel": 1 })));
    backend.status_cache.lock().unwrap().entry = sentinel;
    assert_eq!(backend.status_json().unwrap()["sentinel"], json!(1));
    backend.invalidate_status_cache();
    assert!(backend.status_cache.lock().unwrap().entry.is_none());
    let refreshed = backend.status_json().unwrap();
    assert_eq!(refreshed["scope"], json!("all"));
    assert!(refreshed.get("sentinel").is_none());
    assert_eq!(refreshed["heavy_slots"]["memory_pressure"], json!("warn"));
}

#[test]
fn invalidation_during_inflight_refresh_skips_stale_cache_write() {
    let tmp = tempfile::tempdir().unwrap();
    let backend = test_backend(tmp.path());
    let start_gen = backend.cached_or_generation().unwrap_err();
    backend.invalidate_status_cache();
    backend.store_status_if_current(start_gen, &json!({ "stale": true }));
    assert!(backend.read_cached_status().is_none());
    let cur_gen = backend.cached_or_generation().unwrap_err();
    assert_ne!(cur_gen, start_gen);
    backend.store_status_if_current(cur_gen, &json!({ "fresh": true }));
    assert_eq!(backend.read_cached_status(), Some(json!({ "fresh": true })));
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
