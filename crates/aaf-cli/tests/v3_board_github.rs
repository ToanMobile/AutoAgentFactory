#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Pillar 4 CLI: `aaf board import-issues` and `aaf board sync-github [--dry-run]`.

#[path = "factory_support.rs"]
mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use aaf_domain::ports::TaskStore;
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use serde_json::json;
use support::Fixture;

fn write_exec(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn board_github_commands_validate_guards_and_dry_run_sync() {
    let fx = Fixture::new();
    let id = fx.add("Prepare Super-Board card");

    // Inside agent run, both commands refuse with exit 2.
    let inside_import = fx.aaf_in(
        &fx.repo,
        &["board", "import-issues"],
        &[("AAF_RUN_ID", "r1")],
    );
    assert_eq!(inside_import.code, 2, "{}", inside_import.stderr);
    assert_eq!(inside_import.json["status"], json!("refused"));

    let inside_sync = fx.aaf_in(
        &fx.repo,
        &["board", "sync-github", "--dry-run"],
        &[("AAF_RUN_ID", "r1")],
    );
    assert_eq!(inside_sync.code, 2, "{}", inside_sync.stderr);
    assert_eq!(inside_sync.json["status"], json!("refused"));

    // Fail-closed when owner_github_allowlist is empty (no gh call attempted).
    let no_allow = fx.aaf(&["board", "import-issues"]);
    assert_eq!(no_allow.code, 2, "{}", no_allow.stderr);
    assert_eq!(no_allow.json["status"], json!("refused"));

    // Live sync refuses when [github].enabled is false (default).
    let disabled_sync = fx.aaf(&["board", "sync-github"]);
    assert_eq!(disabled_sync.code, 2, "{}", disabled_sync.stderr);
    assert_eq!(disabled_sync.json["status"], json!("refused"));

    // Dry-run sync still succeeds even when [github].enabled is false.
    let sync = fx.aaf(&["board", "sync-github", "--dry-run"]);
    assert_eq!(sync.code, 0, "{}", sync.stderr);
    assert_eq!(sync.json["mode"], json!("issue_label"));
    assert_eq!(sync.json["dry_run"], json!(true));
    assert_eq!(sync.json["unchanged"], json!(0));
    assert_eq!(sync.json["errors"], json!(0));
    assert_eq!(sync.json["truncated_project_items"], json!(false));
    let items = sync.json["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["task_id"], json!(id));
    assert_eq!(items[0]["column"], json!("Ready"));
    assert_eq!(items[0]["unchanged"], json!(false));
}

#[test]
fn board_github_refuses_whitespace_allowlist_and_disabled_import() {
    let fx = Fixture::new();
    let tier1 = fx.root().join("home/.config/autofactory/config.toml");

    // Whitespace-only allowlist must be refused (exit 2).
    fs::write(
        &tier1,
        "[machine]\nmax_load_percent = 0\n\n[safety]\nowner_github_allowlist = [\"   \", \"\\t\"]\n",
    )
    .unwrap();
    let ws_import = fx.aaf(&["board", "import-issues"]);
    assert_eq!(ws_import.code, 2, "{}", ws_import.stderr);
    assert_eq!(ws_import.json["status"], json!("refused"));

    // Valid allowlist still refuses import while [github].enabled is false.
    fs::write(
        &tier1,
        "[machine]\nmax_load_percent = 0\n\n[safety]\nowner_github_allowlist = [\"owner\"]\n",
    )
    .unwrap();
    let disabled_import = fx.aaf(&["board", "import-issues"]);
    assert_eq!(disabled_import.code, 2, "{}", disabled_import.stderr);
    assert_eq!(disabled_import.json["status"], json!("refused"));
}

#[test]
fn board_github_import_issues_label_and_dry_run_flags() {
    let fx = Fixture::new();

    // Inside AAF_RUN_ID, --label --dry-run is still refused (exit 2).
    let inside = fx.aaf_in(
        &fx.repo,
        &["board", "import-issues", "--label", "bug", "--dry-run"],
        &[("AAF_RUN_ID", "r1")],
    );
    assert_eq!(inside.code, 2, "{}", inside.stderr);
    assert_eq!(inside.json["status"], json!("refused"));

    // Empty allowlist is still refused (exit 2) even with --label and --dry-run.
    let no_allow = fx.aaf(&["board", "import-issues", "--label", "bug", "--dry-run"]);
    assert_eq!(no_allow.code, 2, "{}", no_allow.stderr);
    assert_eq!(no_allow.json["status"], json!("refused"));

    // Configure allowlist (leaving [github].enabled = false) and verify dry-run preview
    // resolves `gh` outside the repo/worktree via `aaf_fp::program("gh")`, forwards
    // `--label bug`, and writes zero tasks to SQLite.
    let tier1 = fx.root().join("home/.config/autofactory/config.toml");
    fs::write(
        &tier1,
        "[machine]\nmax_load_percent = 0\n\n[safety]\nowner_github_allowlist = [\"owner\"]\n",
    )
    .unwrap();

    let poison_bin = fx.repo.join("poison-bin");
    write_exec(
        &poison_bin.join("gh"),
        "#!/bin/sh\n: > \"${0%/*}/poisoned\"\nexit 99\n",
    );
    let fake_bin = fx.root().join("fake-bin");
    write_exec(
        &fake_bin.join("gh"),
        "#!/bin/sh\n\
         printf '%s\\n' \"$*\" > \"${0%/*}/gh.args\"\n\
         printf '%s' '[{\"number\":42,\"title\":\"Dry run bug\",\"body\":\"desc\",\"author\":{\"login\":\"owner\"},\"labels\":[{\"name\":\"bug\"}],\"comments\":[]},{\"number\":43,\"title\":\"Wrong label\",\"body\":\"desc\",\"author\":{\"login\":\"owner\"},\"labels\":[{\"name\":\"docs\"}],\"comments\":[]}]'\n",
    );
    let path = format!(
        "{}:{}:{}",
        poison_bin.display(),
        fake_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let dry = fx.aaf_in(
        &fx.repo,
        &["board", "import-issues", "--label", "bug", "--dry-run"],
        &[("PATH", &path)],
    );
    assert_eq!(dry.code, 0, "{}", dry.stderr);
    assert_eq!(dry.json["dry_run"], json!(true));
    assert_eq!(dry.json["fetched"], json!(1));
    assert_eq!(dry.json["created"].as_array().unwrap().len(), 1);
    assert!(!poison_bin.join("poisoned").exists());
    let recorded_args = fs::read_to_string(fake_bin.join("gh.args")).unwrap();
    assert!(recorded_args.contains("--label bug"), "{recorded_args}");
    // Dry-run import must be strictly read-only and never create AAF_DATA_DIR or aaf.db.
    assert!(
        !fx.data.exists(),
        "import-issues --dry-run created data dir"
    );
    assert!(!fx.data.join("aaf.db").exists());

    // Dry-run sync-github on a fresh workspace also creates neither data dir nor aaf.db.
    let dry_sync = fx.aaf(&["board", "sync-github", "--dry-run"]);
    assert_eq!(dry_sync.code, 0, "{}", dry_sync.stderr);
    assert_eq!(dry_sync.json["dry_run"], json!(true));
    assert_eq!(dry_sync.json["items"].as_array().unwrap().len(), 0);
    assert!(!fx.data.exists(), "sync-github --dry-run created data dir");

    let list = fx.aaf(&["board", "list"]);
    assert_eq!(list.code, 0, "{}", list.stderr);
    assert_eq!(list.json["count"], json!(0));
}

#[test]
fn board_github_sync_partial_error_exits_2_and_preserves_json_fields() {
    let fx = Fixture::new();
    let cfg_dir = fx.repo.join(".autofactory");
    fs::create_dir_all(&cfg_dir).unwrap();
    fs::write(
        cfg_dir.join("config.toml"),
        "[github]\nenabled = true\nproject_owner = \"ToanMobile\"\nproject_number = 4\nrepo = \"\"\n",
    )
    .unwrap();

    let repo_s = fx.repo.to_string_lossy().into_owned();
    {
        let db = fx.db();
        for (id, issue, state) in [
            ("t-ok88", 88_u64, TaskState::Ready),
            ("t-err89", 89_u64, TaskState::Gating),
        ] {
            let mut task = Task::new_root(
                id,
                &repo_s,
                format!("Issue #{issue}"),
                ImmutableSpec {
                    description: "d".to_string(),
                    acceptance: vec!["a".to_string()],
                },
                Priority::P1,
                100,
            );
            task.state = state;
            task.fingerprint = Some(format!("gh-issue:#{issue}"));
            db.insert_task(&task).unwrap();
        }
    }

    let fake_bin = fx.root().join("fake-gh");
    write_exec(
        &fake_bin.join("gh"),
        "#!/bin/sh\n\
         case \"$1 $2\" in\n\
           'project view') printf '%s' '{\"id\":\"PVT_4\"}' ;;\n\
           'project field-list') printf '%s' '{\"fields\":[{\"id\":\"PVTSSF_status\",\"name\":\"Status\",\"options\":[{\"id\":\"opt_r\",\"name\":\"Ready\"},{\"id\":\"opt_q\",\"name\":\"QA\"}]}]}' ;;\n\
           'project item-list') printf '%s' '{\"items\":[{\"id\":\"PVTI_88\",\"status\":\"Ready\",\"content\":{\"type\":\"Issue\",\"number\":88}}],\"totalCount\":1}' ;;\n\
           *) echo \"unexpected gh args: $*\" >&2; exit 1 ;;\n\
         esac\n",
    );
    let path = format!(
        "{}:{}",
        fake_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let res = fx.aaf_in(&fx.repo, &["board", "sync-github"], &[("PATH", &path)]);
    assert_eq!(res.code, 2, "{}", res.stderr);
    assert_eq!(res.json["ok"], json!(false));
    assert_eq!(res.json["status"], json!("partial_error"));
    assert_eq!(res.json["mode"], json!("project_v2"));
    assert_eq!(res.json["unchanged"], json!(1));
    assert_eq!(res.json["errors"], json!(1));
    let items = res.json["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{}", res.json);
    let err_item = items
        .iter()
        .find(|i| i["task_id"] == json!("t-err89"))
        .unwrap();
    assert!(err_item["error"].as_str().is_some());
}
