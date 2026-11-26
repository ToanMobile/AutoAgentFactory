#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Lane-2 hardening tests:
//! 1. `.git/info/exclude` writes denied for `FileWrite` and shell/interpreters (`SECURITY.md §9`)
//! 2. `store_guard` rejects non-metadata external-excluded files while tolerating `.DS_Store`
//! 3. `main_branch_guard` interactive Zero-Touch `main`/`master`/`develop` protection
//! 4. `--host antigravity` and `--host opencode` adapters and JSON allow/deny outputs

mod common;

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use aaf_domain::gate::{
    GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION, Receipt,
    StoreComplianceReport, StoreStatus,
};
use aaf_hook::host::{HookHost, decide_for_host, parse_cli_host, run_for_host};
use aaf_hook::{Decision, HookEnv, decide};
use common::{Fixture, rule, run_corpus, run_file_corpus};
use serde_json::{Value, json};

const ALLOW: Option<&str> = None;
const GIT: Option<&str> = Some("git_guard");
const INTERP: Option<&str> = Some("interp_guard");
const MAIN: Option<&str> = Some("main_branch_guard");

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?} failed: {out:?}");
}

fn with_env<T>(fx: &Fixture, work: impl FnOnce(&HookEnv) -> T) -> T {
    let root = fx.root.clone();
    let lookup = move |_: &Path| Some(root.clone());
    let config = |_: &Path| fx.git_config.borrow().clone();
    let env = HookEnv {
        repo_root: &lookup,
        git_config: &config,
        data_dir: Some(fx.data.clone()),
        config_dir: Some(fx.config.clone()),
        home: Some(fx.home.clone()),
        cwd: None,
        in_factory_run: fx.in_factory_run,
        trusted_aaf_bins: fx.trusted_aaf_bins.clone(),
    };
    work(&env)
}

fn run_hook_bin(
    args: &[&str],
    payload: &Value,
    envs: &[(&str, &str)],
    data_dir: &Path,
    home: &Path,
) -> (i32, Value, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aaf-hook"));
    cmd.args(args)
        .env_clear()
            .env("AAF_NO_BUILTIN_DEFAULTS", "1")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("AAF_DATA_DIR", data_dir)
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let code = output.status.code().unwrap_or(-1);
    let stdout_val: Value = serde_json::from_slice(&output.stdout).unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (code, stdout_val, stderr)
}

#[test]
fn git_info_exclude_writes_are_denied_across_file_shell_and_interpreters() {
    let fx = Fixture::new();
    fx.write(".git/info/exclude", "# existing\n");

    run_file_corpus(
        &fx,
        &[
            ("Write", ".git/info/exclude", GIT),
            ("Edit", ".git/info/exclude", GIT),
            ("MultiEdit", ".git/info/exclude.lock", GIT),
            ("Write", "{root}/.git/info/exclude", GIT),
            ("Read", ".git/info/exclude", ALLOW),
        ],
    );

    run_corpus(
        &fx,
        &[
            ("echo secret_patch.kt >> .git/info/exclude", GIT),
            ("sh -c 'echo x >> .git/info/exclude'", GIT),
            ("printf 'secret.kt\\n' > .git/info/exclude.lock", GIT),
            ("tee -a .git/info/exclude <<< 'secret.kt'", GIT),
            ("cp /tmp/list .git/info/exclude", GIT),
            (
                "python3 -c \"open('.git/info/exclude','a').write('secret_patch.kt\\n')\"",
                INTERP,
            ),
            (
                "python3 -c \"import os; open(os.path.join('.git','info','exclude'),'w').write('x')\"",
                INTERP,
            ),
            ("cat .git/info/exclude", ALLOW),
        ],
    );
}

fn clear_release_receipt(repo: &Path) -> Receipt {
    Receipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        repo_root: repo.to_string_lossy().into_owned(),
        tree_fp: aaf_fp::tree_fp(repo, aaf_fp::GIT_TIMEOUT).unwrap(),
        head_sha: aaf_fp::head_sha(repo),
        base_sha: None,
        mode: GateMode::Release,
        status: GateStatus::Clear,
        reasons: Vec::new(),
        config_hash: "test".to_string(),
        commands: Vec::new(),
        static_findings: Vec::new(),
        suppressions: 0,
        head_lock: HeadLockReport {
            status: LockStatus::Clear,
            lock_fp: String::new(),
            locked_changes: Vec::new(),
            approval: None,
        },
        store_compliance: StoreComplianceReport {
            status: StoreStatus::Clear,
            critical: 0,
            warnings: 0,
            unresolved: Vec::new(),
            findings: Vec::new(),
        },
        created_at: 1_700_000_000,
    }
}

#[test]
fn store_guard_refuses_non_metadata_external_excluded_files_and_tolerates_ds_store() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let (repo, data, home) = (base.join("app"), base.join("data"), base.join("home"));
    for p in [&repo, &data, &home] {
        fs::create_dir_all(p).unwrap();
    }
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("Main.kt"), "fun main() {}\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);

    let key = aaf_fp::load_or_create_key(&data).unwrap();
    aaf_fp::write_receipt(&data, &key, &clear_release_receipt(&repo)).unwrap();

    let submit = || {
        let lookup = |d: &Path| aaf_fp::repo_root(d).ok();
        let env = HookEnv {
            repo_root: &lookup,
            git_config: &|_| Some(String::new()),
            data_dir: Some(data.clone()),
            config_dir: None,
            home: Some(home.clone()),
            cwd: None,
            in_factory_run: false,
            trusted_aaf_bins: Vec::new(),
        };
        let payload = json!({"tool_name": "Bash", "tool_input": {"command": "fastlane deliver"}, "cwd": repo});
        decide(payload.to_string().as_bytes(), &env)
    };

    // Harmless OS metadata files under `.git/info/exclude` are tolerated.
    fs::write(
        repo.join(".git/info/exclude"),
        ".DS_Store\nThumbs.db\nDesktop.ini\n._*\nsecret_patch.kt\n",
    )
    .unwrap();
    for meta in [".DS_Store", "Thumbs.db", "Desktop.ini", "._Main.kt"] {
        fs::write(repo.join(meta), "meta").unwrap();
    }
    assert_eq!(submit(), Decision::Allow);

    // A non-metadata external-excluded file MUST refuse store submission.
    fs::write(repo.join("secret_patch.kt"), "fun backdoor() {}\n").unwrap();
    match submit() {
        Decision::Deny(denial) => {
            assert_eq!(denial.rule, "store_submit_guard");
            assert!(
                denial.reason.contains("secret_patch.kt"),
                "{}",
                denial.reason
            );
        }
        Decision::Allow => panic!("expected store_guard to refuse secret_patch.kt"),
    }
}

#[test]
fn main_branch_guard_protects_primary_checkout_and_allows_worktrees_and_overrides() {
    // 1. Primary checkout on main/master/develop with `.autofactory/config.toml` blocks interactively.
    for branch in ["main", "master", "develop"] {
        let fx = Fixture::new().interactive();
        fx.checkout(branch);
        fx.write(".autofactory/config.toml", "[project]\nname = \"demo\"\n");

        assert_eq!(rule(&fx.file_tool("Write", "src/lib.rs")), MAIN, "{branch}");
        assert_eq!(rule(&fx.file_tool("Edit", "README.md")), MAIN, "{branch}");
        assert_eq!(rule(&fx.file_tool("Read", "src/lib.rs")), ALLOW, "{branch}");
        assert_eq!(
            rule(&fx.file_tool("Write", "/tmp/outside-scratch.txt")),
            ALLOW,
            "{branch}"
        );
        assert_eq!(rule(&fx.bash("git commit -m 'direct'")), MAIN, "{branch}");
        assert_eq!(rule(&fx.bash("git status")), ALLOW, "{branch}");

        // Also blocks via multi-host FileWrite path.
        let (dec, _) = with_env(&fx, |env| {
            decide_for_host(
                json!({
                    "tool_name": "write_to_file",
                    "tool_input": {"TargetFile": "src/app.rs"},
                    "cwd": fx.root,
                })
                .to_string()
                .as_bytes(),
                env,
                HookHost::Antigravity,
            )
        });
        assert_eq!(rule(&dec), MAIN, "{branch}");
    }

    // 2. Without `.autofactory/config.toml` (non-aaf repo), edits on main stay allowed.
    let non_aaf = Fixture::new().interactive();
    non_aaf.checkout("main");
    assert_eq!(rule(&non_aaf.file_tool("Write", "src/lib.rs")), ALLOW);
    assert_eq!(rule(&non_aaf.bash("git commit -m 'ok'")), ALLOW);

    // 3. On a feature branch, edits and commits are allowed.
    let feature = Fixture::new().interactive();
    feature.checkout("feature/safe");
    feature.write(".autofactory/config.toml", "[project]\nname = \"demo\"\n");
    assert_eq!(rule(&feature.file_tool("Write", "src/lib.rs")), ALLOW);
    assert_eq!(rule(&feature.bash("git commit -m 'ok'")), ALLOW);

    // 4. Headless worker (`in_factory_run: true`) is never blocked by main_branch_guard.
    let worker = Fixture::new();
    worker.checkout("main");
    worker.write(".autofactory/config.toml", "[project]\nname = \"demo\"\n");
    assert_eq!(rule(&worker.file_tool("Write", "src/lib.rs")), ALLOW);
    assert_eq!(rule(&worker.bash("git commit -m 'ok'")), ALLOW);

    // 5. Real git repo + linked worktree + binary env checks (`AAF_ALLOW_MAIN_EDIT=1`, `AAF_RUN_ID`).
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let repo = base.join("primary");
    let wt = base.join("wt-t1");
    let data = base.join("data");
    let home = base.join("home");
    for p in [&repo, &data, &home] {
        fs::create_dir_all(p).unwrap();
    }
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(repo.join(".autofactory")).unwrap();
    fs::write(
        repo.join(".autofactory/config.toml"),
        "[project]\nname = \"p\"\n",
    )
    .unwrap();
    fs::write(repo.join("lib.rs"), "pub fn a() {}\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "aaf/t1",
            wt.to_str().unwrap(),
        ],
    );
    assert!(
        wt.join(".git").is_file(),
        "linked worktree must have .git file"
    );

    let edit_primary = json!({"tool_name": "Write", "tool_input": {"file_path": repo.join("lib.rs")}, "cwd": repo});
    let commit_primary = json!({"tool_name": "Bash", "tool_input": {"command": "git commit -m 'direct'"}, "cwd": repo});
    let edit_wt =
        json!({"tool_name": "Write", "tool_input": {"file_path": wt.join("lib.rs")}, "cwd": wt});

    // Interactive on primary `main` -> DENIED (code 2).
    let (code, _, stderr) = run_hook_bin(&[], &edit_primary, &[], &data, &home);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("main_branch_guard"), "{stderr}");
    let (code, _, stderr) = run_hook_bin(&[], &commit_primary, &[], &data, &home);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("main_branch_guard"), "{stderr}");

    // Linked worktree `aaf/t1` -> ALLOWED (code 0).
    let (code, _, stderr) = run_hook_bin(&[], &edit_wt, &[], &data, &home);
    assert_eq!(code, 0, "{stderr}");

    // Break-glass `AAF_ALLOW_MAIN_EDIT=1` -> ALLOWED (code 0).
    let (code, _, stderr) = run_hook_bin(
        &[],
        &edit_primary,
        &[("AAF_ALLOW_MAIN_EDIT", "1")],
        &data,
        &home,
    );
    assert_eq!(code, 0, "{stderr}");
    let (code, _, stderr) = run_hook_bin(
        &[],
        &commit_primary,
        &[("AAF_ALLOW_MAIN_EDIT", "1")],
        &data,
        &home,
    );
    assert_eq!(code, 0, "{stderr}");

    // Headless worker `AAF_RUN_ID=run-1` -> ALLOWED (code 0).
    let (code, _, stderr) =
        run_hook_bin(&[], &edit_primary, &[("AAF_RUN_ID", "run-1")], &data, &home);
    assert_eq!(code, 0, "{stderr}");
}

#[test]
fn antigravity_and_opencode_host_adapters_parse_and_render_allow_deny() {
    assert_eq!(
        HookHost::parse_arg("antigravity"),
        Some(HookHost::Antigravity)
    );
    assert_eq!(HookHost::parse_arg("opencode"), Some(HookHost::Opencode));
    assert_eq!(
        parse_cli_host(&["--host".into(), "antigravity".into()]),
        Ok(HookHost::Antigravity)
    );
    assert_eq!(
        parse_cli_host(&["--host=opencode".into()]),
        Ok(HookHost::Opencode)
    );

    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    fx.write("src/ok.rs", "fn main() {}\n");

    // Antigravity: BeforeTool & Claude-style PreToolUse payloads.
    let agy_deny = with_env(&fx, |env| {
        run_for_host(
            json!({
                "hook_event_name": "BeforeTool",
                "tool_name": "run_command",
                "tool_input": {"CommandLine": "git push --force"},
                "cwd": fx.root,
            })
            .to_string()
            .as_bytes(),
            env,
            HookHost::Antigravity,
        )
    });
    assert_eq!(agy_deny.code, 2);
    let agy_json: Value = serde_json::from_str(&agy_deny.stdout).unwrap();
    assert_eq!(agy_json["decision"], "deny");
    assert_eq!(
        agy_json["hookSpecificOutput"]["hookEventName"],
        "BeforeTool"
    );
    assert_eq!(agy_json["hookSpecificOutput"]["permissionDecision"], "deny");

    for payload in [
        json!({"tool_name": "Bash", "tool_input": {"command": "git status"}, "cwd": fx.root}),
        json!({"tool_name": "write_to_file", "tool_input": {"TargetFile": "src/ok.rs"}, "cwd": fx.root}),
        json!({"tool_name": "replace_file_content", "tool_input": {"TargetFile": "src/ok.rs"}, "cwd": fx.root}),
        json!({"tool_name": "multi_replace_file_content", "tool_input": {"TargetFile": "src/ok.rs"}, "cwd": fx.root}),
        json!({"tool_name": "view_file", "tool_input": {"AbsolutePath": "src/ok.rs"}, "cwd": fx.root}),
    ] {
        let out = with_env(&fx, |env| {
            run_for_host(payload.to_string().as_bytes(), env, HookHost::Antigravity)
        });
        assert_eq!(out.code, 0, "{payload} -> {}", out.stderr);
        assert_eq!(out.stdout, "{}");
    }

    let (dec, resolved) = with_env(&fx, |env| {
        decide_for_host(
            json!({
                "tool_name": "replace_file_content",
                "tool_input": {"TargetFile": ".git/info/exclude"},
                "cwd": fx.root,
            })
            .to_string()
            .as_bytes(),
            env,
            HookHost::Auto,
        )
    });
    assert_eq!(resolved, HookHost::Antigravity);
    assert_eq!(rule(&dec), GIT);

    // OpenCode: `tool.execute.before` and `{tool, args, cwd}` payloads.
    let oc_deny = with_env(&fx, |env| {
        run_for_host(
            json!({"hook_event_name": "tool.execute.before", "tool": "bash", "args": {"command": "git push --force"}, "cwd": fx.root})
                .to_string()
                .as_bytes(),
            env,
            HookHost::Auto,
        )
    });
    assert_eq!(oc_deny.code, 2);
    let oc_json: Value = serde_json::from_str(&oc_deny.stdout).unwrap();
    assert_eq!(oc_json["decision"], "deny");
    assert_eq!(oc_json["hookSpecificOutput"]["permissionDecision"], "deny");

    let (dec, resolved) = with_env(&fx, |env| {
        decide_for_host(
            json!({"tool": "write", "args": {"filePath": ".git/info/exclude"}, "cwd": fx.root})
                .to_string()
                .as_bytes(),
            env,
            HookHost::Auto,
        )
    });
    assert_eq!(resolved, HookHost::Opencode);
    assert_eq!(rule(&dec), GIT);

    for (payload, expected) in [
        (
            json!({"tool": "patch", "args": {"patch": "*** Begin Patch\n*** Update File: .env\n*** End Patch"}, "cwd": fx.root}),
            Some("delete_guard"),
        ),
        (
            json!({"tool": "read", "args": {"filePath": ".env"}, "cwd": fx.root}),
            Some("secret_guard"),
        ),
    ] {
        let (dec, _) = with_env(&fx, |env| {
            decide_for_host(payload.to_string().as_bytes(), env, HookHost::Opencode)
        });
        assert_eq!(rule(&dec), expected, "{payload}");
    }

    let oc_allow = with_env(&fx, |env| {
        run_for_host(
            json!({"hook_event_name": "tool.execute.before", "tool": "edit", "args": {"filePath": "src/ok.rs"}, "cwd": fx.root})
                .to_string()
                .as_bytes(),
            env,
            HookHost::Opencode,
        )
    });
    assert_eq!(oc_allow.code, 0);
    assert_eq!(oc_allow.stdout, "{}");
}
