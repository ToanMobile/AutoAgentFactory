#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
fn read_only_list_and_show_do_not_create_db() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let ctx = Ctx {
        cwd: tmp.path().to_path_buf(),
        home: tmp.path().join("home"),
        data_dir: data_dir.clone(),
        config_dir: tmp.path().join("config"),
        worktrees_dir: tmp.path().join("worktrees"),
        path_var: std::ffi::OsString::new(),
        run_id: None,
        stdin_is_tty: false,
        aaf_env: Vec::new(),
    };

    let listed = list(
        &ListArgs {
            state: None,
            all: true,
        },
        &ctx,
    )
    .unwrap();
    assert_eq!(listed.status, "ok");
    assert!(!data_dir.join("aaf.db").exists());

    let err = show("t-missing", &ctx).unwrap_err();
    assert!(
        err.message.to_ascii_lowercase().contains("not found"),
        "{}",
        err.message
    );
    assert!(!data_dir.join("aaf.db").exists());
}
