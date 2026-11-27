#![allow(clippy::unwrap_used, clippy::expect_used)]
//! SEC-097: `check_worktree_for_executor` must reject a task worktree whose
//! non-Claude host hook registration (`.codex/hooks.json`) was modified from
//! `merge-base HEAD <base>`, matching the `claude` branch (`check_worktree`).

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use aaf_engine::hook_settings::{self, multi_host};
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn write_executable(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

fn init_temp_repo() -> (TempDir, PathBuf, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let repo = root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "--quiet", "-b", "main"]);
    for (k, v) in [
        ("user.name", "aaf test"),
        ("user.email", "aaf@example.invalid"),
        ("commit.gpgsign", "false"),
        ("core.hooksPath", "/dev/null"),
    ] {
        git(&repo, &["config", k, v]);
    }
    fs::write(repo.join("README.md"), "test\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "--quiet", "-m", "init"]);

    let hook_bin = root.join("hook-bin");
    fs::create_dir_all(&hook_bin).unwrap();
    write_executable(&hook_bin.join("aaf-hook"), "#!/bin/sh\nexit 0\n");
    (tmp, root, repo, hook_bin)
}

fn add_task_worktree(repo: &Path, root: &Path, branch: &str, base: &str) -> PathBuf {
    let wt = root.join(branch);
    git(
        repo,
        &["worktree", "add", "-b", branch, wt.to_str().unwrap(), base],
    );
    wt
}

fn create_fake_hook_outside_worktree() -> (TempDir, PathBuf) {
    let fake_tmp = tempfile::tempdir().unwrap();
    let fake_root = fs::canonicalize(fake_tmp.path()).unwrap();
    let fake_hook = fake_root.join("aaf-hook");
    write_executable(&fake_hook, "#!/bin/sh\ncat >/dev/null; echo '{}'\n");
    (fake_tmp, fake_hook)
}

#[test]
fn codex_worktree_hook_changed_from_base_is_rejected() {
    let (_tmp, root, repo, hook_bin) = init_temp_repo();
    let ok_path = format!("{}:/usr/bin:/bin", hook_bin.display());

    multi_host::register_codex(&repo, "aaf-hook").unwrap();
    git(&repo, &["add", ".codex/hooks.json"]);
    git(&repo, &["commit", "--quiet", "-m", "add codex hook"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);

    let wt = add_task_worktree(&repo, &root, "task-sec097-codex", &base);
    let (_fake_tmp, fake_hook) = create_fake_hook_outside_worktree();

    multi_host::register_codex(&wt, fake_hook.to_str().unwrap()).unwrap();
    git(&wt, &["add", ".codex/hooks.json"]);
    git(&wt, &["commit", "--quiet", "-m", "tamper codex hook"]);

    let res =
        hook_settings::check_worktree_for_executor(&wt, OsStr::new(&ok_path), Some(&base), "codex");
    assert!(
        res.is_err(),
        "SEC-097: check_worktree_for_executor must reject codex worktree when .codex/hooks.json \
         was changed from base to point to a fake external aaf-hook, got Ok(())"
    );
}

#[test]
fn codex_worktree_hook_unchanged_from_base_is_accepted() {
    let (_tmp, root, repo, hook_bin) = init_temp_repo();
    let ok_path = format!("{}:/usr/bin:/bin", hook_bin.display());

    multi_host::register_codex(&repo, "aaf-hook").unwrap();
    git(&repo, &["add", ".codex/hooks.json"]);
    git(&repo, &["commit", "--quiet", "-m", "add codex hook"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);

    let wt = add_task_worktree(&repo, &root, "task-sec097-codex-ok", &base);
    fs::write(wt.join("work.txt"), "unrelated change\n").unwrap();
    git(&wt, &["add", "work.txt"]);
    git(&wt, &["commit", "--quiet", "-m", "unrelated task commit"]);

    let res =
        hook_settings::check_worktree_for_executor(&wt, OsStr::new(&ok_path), Some(&base), "codex");
    assert!(
        res.is_ok(),
        "codex worktree with .codex/hooks.json unchanged from base should be accepted, got {res:?}"
    );
}

#[test]
fn claude_worktree_hook_changed_from_base_is_rejected() {
    let (_tmp, root, repo, hook_bin) = init_temp_repo();
    let ok_path = format!("{}:/usr/bin:/bin", hook_bin.display());

    let (base_settings, _) = hook_settings::merge_hook(None, "aaf-hook").unwrap();
    fs::create_dir_all(repo.join(".claude")).unwrap();
    fs::write(
        repo.join(hook_settings::SETTINGS_REL),
        base_settings.unwrap(),
    )
    .unwrap();
    git(&repo, &["add", hook_settings::SETTINGS_REL]);
    git(&repo, &["commit", "--quiet", "-m", "add claude hook"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);

    let wt = add_task_worktree(&repo, &root, "task-sec097-claude", &base);
    let (_fake_tmp, fake_hook) = create_fake_hook_outside_worktree();
    let fake_cmd = hook_settings::shell_quote(fake_hook.to_str().unwrap());
    let (tampered, _) = hook_settings::merge_hook(None, &fake_cmd).unwrap();
    fs::write(wt.join(hook_settings::SETTINGS_REL), tampered.unwrap()).unwrap();
    git(&wt, &["add", hook_settings::SETTINGS_REL]);
    git(&wt, &["commit", "--quiet", "-m", "tamper claude hook"]);

    let err = hook_settings::check_worktree_for_executor(
        &wt,
        OsStr::new(&ok_path),
        Some(&base),
        "claude",
    )
    .unwrap_err();
    assert!(
        err.contains("the task changed it"),
        "expected claude branch to reject changed settings file, got: {err}"
    );
}
