#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Oracle tests for C6: opt-in `aaf gc --merged-branches [--base <ref>] [--dry-run]`.
//!
//! Invokes the real `aaf` binary (`CARGO_BIN_EXE_aaf`) and asserts non-vacuous
//! exit codes (`0`) and `"status": "ok"` on every negative/safety check.

#[path = "cli_support.rs"]
mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::json;
use support::{Sandbox, make_executable, one_object, write};

fn git_in(dir: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=f@example.invalid",
        ])
        .args(args)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn git_ok(dir: &Path, home: &Path, args: &[&str]) -> String {
    let out = git_in(dir, home, args);
    assert!(
        out.status.success(),
        "git -C {} {:?} failed: {}",
        dir.display(),
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn branch_exists(sb: &Sandbox, branch: &str) -> bool {
    let full = format!("refs/heads/{branch}");
    git_in(
        &sb.repo,
        &sb.home,
        &["show-ref", "--verify", "--quiet", &full],
    )
    .status
    .success()
}

fn add_worktree(sb: &Sandbox, branch: &str, dir_name: &str, start: &str) -> PathBuf {
    let wt = sb.repo.parent().unwrap().join(dir_name);
    let wt_str = wt.to_str().unwrap();
    git_ok(
        &sb.repo,
        &sb.home,
        &["worktree", "add", "-b", branch, wt_str, start],
    );
    fs::canonicalize(&wt).unwrap()
}

fn commit_file_in(dir: &Path, home: &Path, rel: &str, body: &str, msg: &str) -> String {
    write(&dir.join(rel), body);
    git_ok(dir, home, &["add", "-A"]);
    git_ok(dir, home, &["commit", "-q", "-m", msg]);
    git_ok(dir, home, &["rev-parse", "HEAD"])
}

fn merge_no_ff(sb: &Sandbox, branch: &str) -> String {
    git_ok(
        &sb.repo,
        &sb.home,
        &[
            "merge",
            "--no-ff",
            "-q",
            "-m",
            &format!("merge {branch}"),
            branch,
        ],
    );
    git_ok(&sb.repo, &sb.home, &["rev-parse", "HEAD"])
}

#[test]
fn c6_1_merged_branch_with_clean_worktree_is_removed_only_when_opted_in() {
    let sb = Sandbox::new();
    let wt = add_worktree(&sb, "fix/done", "wt-fix-done", "main");
    commit_file_in(&wt, &sb.home, "fix.txt", "done\n", "real fix commit");
    merge_no_ff(&sb, "fix/done");

    // Default `aaf gc` (without `--merged-branches`) must NOT touch `fix/done`.
    let (code_def, out_def) = sb.json(&["gc"]);
    assert_eq!(code_def, 0, "{out_def}");
    assert_eq!(out_def["status"], json!("ok"));
    assert!(
        wt.is_dir(),
        "default gc must leave fix/done worktree intact"
    );
    assert!(
        branch_exists(&sb, "fix/done"),
        "default gc must keep fix/done"
    );

    // Opt-in `aaf gc --merged-branches` removes both clean linked worktree and branch.
    let (code, out) = sb.json(&["gc", "--merged-branches"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["ok"], json!(true));
    assert_eq!(out["status"], json!("ok"));
    assert_eq!(out["merged_branches_base"], json!("refs/heads/main"));
    assert_eq!(out["merged_branches_deleted"], json!(["fix/done"]));
    assert_eq!(
        out["merged_worktrees_removed"].as_array().unwrap().len(),
        1,
        "{out}"
    );
    assert!(!wt.exists(), "clean merged worktree must be removed");
    assert!(
        !branch_exists(&sb, "fix/done"),
        "merged branch must be deleted"
    );
}

#[test]
fn c6_2_zero_commit_and_renamed_empty_branches_are_never_deleted() {
    let sb = Sandbox::new();

    // 1. Fresh 0-commit branch created at current `main` (branch_oid == base_oid).
    git_ok(&sb.repo, &sb.home, &["branch", "fix/brand-new", "main"]);

    // 2. Empty branch created at `main`, renamed via `git branch -m`, AND `main`
    //    advances so `branch_oid != base_oid` while still an ancestor (reflog guard).
    git_ok(&sb.repo, &sb.home, &["branch", "fix/temp-empty", "main"]);
    git_ok(
        &sb.repo,
        &sb.home,
        &["branch", "-m", "fix/temp-empty", "fix/renamed-empty"],
    );
    commit_file_in(
        &sb.repo,
        &sb.home,
        "main_next.txt",
        "advance\n",
        "advance main",
    );

    // Also create a second brand-new branch at the new `main` tip.
    git_ok(&sb.repo, &sb.home, &["branch", "lane/fresh-tip", "main"]);

    let (code, out) = sb.json(&["gc", "--merged-branches"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["ok"], json!(true));
    assert_eq!(out["status"], json!("ok"));
    assert_eq!(out["merged_branches_deleted"], json!([]), "{out}");

    assert!(
        branch_exists(&sb, "fix/brand-new"),
        "0-commit branch must remain intact"
    );
    assert!(
        branch_exists(&sb, "fix/renamed-empty"),
        "renamed 0-commit branch must remain intact even after main advances"
    );
    assert!(
        branch_exists(&sb, "lane/fresh-tip"),
        "0-commit branch at main tip must remain intact"
    );
}

#[test]
fn c6_3_dirty_merged_worktree_and_branch_survive_even_with_force() {
    let sb = Sandbox::new();
    let wt = add_worktree(&sb, "audit/dirty", "wt-audit-dirty", "main");
    commit_file_in(&wt, &sb.home, "audit.txt", "checked\n", "audit commit");
    merge_no_ff(&sb, "audit/dirty");

    // Untracked file makes the worktree dirty.
    write(&wt.join("untracked_notes.txt"), "do not lose me\n");

    let (code, out) = sb.json(&["gc", "--merged-branches", "--force"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["ok"], json!(true));
    assert_eq!(out["status"], json!("ok"));
    assert_eq!(out["merged_branches_deleted"], json!([]), "{out}");
    assert_eq!(out["merged_worktrees_removed"], json!([]), "{out}");
    let warnings = out["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap_or("").contains("dirty")),
        "expected dirty warning in {warnings:?}"
    );
    assert!(
        wt.join("untracked_notes.txt").is_file(),
        "dirty worktree must never be removed even with --force"
    );
    assert!(
        branch_exists(&sb, "audit/dirty"),
        "branch of dirty worktree must also be kept"
    );
}

#[test]
fn c6_4_protected_branches_non_allowed_prefixes_and_active_rebase_survive() {
    let sb = Sandbox::new();

    // Protected and non-allowed-prefix merged branches.
    for (branch, dir_name, file) in [
        ("public-v2", "wt-public-v2", "pub.txt"),
        ("release/v1", "wt-release-v1", "rel.txt"),
        ("feat/keep-me", "wt-feat", "feat.txt"),
        ("aaf/keep-task", "wt-aaf", "aaf.txt"),
    ] {
        let wt = add_worktree(&sb, branch, dir_name, "main");
        commit_file_in(&wt, &sb.home, file, "v\n", &format!("commit {branch}"));
        merge_no_ff(&sb, branch);
    }

    // Active interactive rebase (detached HEAD, clean status) on an already-merged branch.
    let wt_rebase = add_worktree(&sb, "lane/rebasing", "wt-rebase", "main");
    commit_file_in(&wt_rebase, &sb.home, "r1.txt", "one\n", "rebase commit 1");
    commit_file_in(&wt_rebase, &sb.home, "r2.txt", "two\n", "rebase commit 2");
    merge_no_ff(&sb, "lane/rebasing");

    // Start `git rebase -i` paused on `edit` so HEAD is detached and `git status` is clean.
    let seq_editor = sb.bin.join("fake-seq-editor");
    write(
        &seq_editor,
        "#!/bin/sh\nsed '1s/^pick /edit /' \"$1\" > \"$1.tmp\" && mv \"$1.tmp\" \"$1\"\n",
    );
    make_executable(&seq_editor);
    let rebase_out = Command::new("git")
        .arg("-C")
        .arg(&wt_rebase)
        .args([
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=f@example.invalid",
            "rebase",
            "-i",
            "HEAD~2",
        ])
        .env("HOME", &sb.home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_SEQUENCE_EDITOR", &seq_editor)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        rebase_out.status.success(),
        "git rebase -i setup failed: {}",
        String::from_utf8_lossy(&rebase_out.stderr)
    );
    // Confirm the test fixture actually has detached HEAD + clean status + rebase-merge.
    let status = git_ok(&wt_rebase, &sb.home, &["status", "--porcelain"]);
    assert!(
        status.is_empty(),
        "rebase worktree should be clean: {status:?}"
    );
    let head_sym = git_in(&wt_rebase, &sb.home, &["symbolic-ref", "-q", "HEAD"]);
    assert!(
        !head_sym.status.success(),
        "rebase worktree should be detached"
    );

    let (code, out) = sb.json(&["gc", "--merged-branches", "--force"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["ok"], json!(true));
    assert_eq!(out["status"], json!("ok"));
    assert_eq!(out["merged_branches_deleted"], json!([]), "{out}");
    assert_eq!(out["merged_worktrees_removed"], json!([]), "{out}");

    for kept in [
        "main",
        "public-v2",
        "release/v1",
        "feat/keep-me",
        "aaf/keep-task",
        "lane/rebasing",
    ] {
        assert!(
            branch_exists(&sb, kept),
            "branch `{kept}` must remain intact"
        );
    }
    assert!(wt_rebase.is_dir(), "rebasing worktree must remain intact");
}

#[test]
fn c6_5_dry_run_previews_without_mutating_disk_or_refs() {
    let sb = Sandbox::new();
    let wt = add_worktree(&sb, "fix/preview", "wt-fix-preview", "main");
    commit_file_in(&wt, &sb.home, "p.txt", "preview\n", "preview commit");
    merge_no_ff(&sb, "fix/preview");

    let (code, out) = sb.json(&["gc", "--merged-branches", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["ok"], json!(true));
    assert_eq!(out["status"], json!("ok"));
    assert_eq!(out["dry_run"], json!(true));
    assert_eq!(out["merged_branches_base"], json!("refs/heads/main"));
    assert_eq!(out["merged_branches_deleted"], json!(["fix/preview"]));
    assert_eq!(
        out["merged_worktrees_removed"].as_array().unwrap().len(),
        1,
        "{out}"
    );

    assert!(wt.is_dir(), "--dry-run must not remove worktree on disk");
    assert!(
        branch_exists(&sb, "fix/preview"),
        "--dry-run must not delete branch"
    );
}

#[test]
fn c6_6_locked_worktree_cwd_guard_and_detached_head_base_resolution() {
    let sb = Sandbox::new();

    // Locked worktree + worktree containing caller cwd.
    let wt_locked = add_worktree(&sb, "fix/locked", "wt-locked", "main");
    commit_file_in(&wt_locked, &sb.home, "l.txt", "lock\n", "locked commit");
    merge_no_ff(&sb, "fix/locked");
    git_ok(
        &sb.repo,
        &sb.home,
        &[
            "worktree",
            "lock",
            "--reason",
            "keep",
            wt_locked.to_str().unwrap(),
        ],
    );

    let wt_cwd = add_worktree(&sb, "fix/in-cwd", "wt-in-cwd", "main");
    commit_file_in(&wt_cwd, &sb.home, "c.txt", "cwd\n", "cwd commit");
    merge_no_ff(&sb, "fix/in-cwd");
    let sub = wt_cwd.join("nested");
    fs::create_dir_all(&sub).unwrap();

    // Run `aaf gc --merged-branches` FROM inside `wt_cwd/nested`.
    let out_cwd = sb
        .command(&["gc", "--merged-branches", "--json"])
        .current_dir(&sub)
        .output()
        .unwrap();
    assert_eq!(out_cwd.status.code(), Some(0));
    let body = one_object(&out_cwd);
    assert_eq!(body["status"], json!("ok"));
    assert!(wt_locked.is_dir() && branch_exists(&sb, "fix/locked"));
    assert!(wt_cwd.is_dir() && branch_exists(&sb, "fix/in-cwd"));

    // Detached HEAD with no default base (`main` renamed to `custom-trunk`):
    // must refuse to guess and warn asking for `--base <ref>`.
    let wt_done = add_worktree(&sb, "audit/ok", "wt-audit-ok", "main");
    commit_file_in(&wt_done, &sb.home, "ok.txt", "ok\n", "audit ok");
    merge_no_ff(&sb, "audit/ok");
    git_ok(
        &sb.repo,
        &sb.home,
        &["branch", "-m", "main", "custom-trunk"],
    );
    let trunk_sha = git_ok(&sb.repo, &sb.home, &["rev-parse", "custom-trunk"]);
    git_ok(
        &sb.repo,
        &sb.home,
        &["checkout", "--detach", "-q", &trunk_sha],
    );

    let (code_no_base, out_no_base) = sb.json(&["gc", "--merged-branches"]);
    assert_eq!(code_no_base, 0, "{out_no_base}");
    assert_eq!(out_no_base["status"], json!("ok"));
    assert!(
        out_no_base["merged_branches_base"].is_null(),
        "{out_no_base}"
    );
    assert!(
        out_no_base["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap_or("").contains("--base")),
        "expected --base guidance warning: {out_no_base}"
    );
    assert!(wt_done.is_dir() && branch_exists(&sb, "audit/ok"));

    // Passing explicit `--base custom-trunk` from inside `wt_cwd/nested` cleans
    // `audit/ok` while still keeping `fix/in-cwd` (cwd) and `fix/locked` (locked).
    let out_with_base_raw = sb
        .command(&[
            "gc",
            "--merged-branches",
            "--base",
            "custom-trunk",
            "--json",
        ])
        .current_dir(&sub)
        .output()
        .unwrap();
    assert_eq!(out_with_base_raw.status.code(), Some(0));
    let out_with_base = one_object(&out_with_base_raw);
    assert_eq!(out_with_base["status"], json!("ok"));
    assert_eq!(
        out_with_base["merged_branches_base"],
        json!("refs/heads/custom-trunk")
    );
    assert_eq!(
        out_with_base["merged_branches_deleted"],
        json!(["audit/ok"])
    );
    assert!(!wt_done.exists() && !branch_exists(&sb, "audit/ok"));
    assert!(wt_cwd.is_dir() && branch_exists(&sb, "fix/in-cwd"));
    assert!(wt_locked.is_dir() && branch_exists(&sb, "fix/locked"));
}

#[test]
fn c6_7_squash_merged_pr_verified_via_gh_and_sha_base_skips_gh() {
    let sb = Sandbox::new();
    let wt = add_worktree(&sb, "fix/squashed", "wt-squashed", "main");
    let head_oid = commit_file_in(&wt, &sb.home, "sq.txt", "squashed\n", "branch commit");

    // Simulate a squash merge commit on `main` (not sharing commit history with `fix/squashed`).
    let merge_oid = commit_file_in(&sb.repo, &sb.home, "sq.txt", "squashed\n", "squash PR #42");

    // Stub `gh` returning the merged PR matching `headRefName` and `headRefOid`
    // and logging every invocation so we can verify SHA `--base` skips `gh`.
    let gh = sb.bin.join("gh");
    let gh_log = sb.home.join("gh.log");
    let payload = json!([{
        "headRefName": "fix/squashed",
        "headRefOid": head_oid,
        "baseRefName": "main",
        "mergeCommit": { "oid": merge_oid },
        "isCrossRepository": false
    }])
    .to_string();
    write(
        &gh,
        &format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\nprintf '%s\\n' '{}'\n",
            gh_log.display(),
            payload.replace('\'', "'\\''")
        ),
    );
    make_executable(&gh);

    // Passing a raw commit SHA as `--base` must NOT call `gh pr list --base <sha>`.
    let (sha_code, sha_out) = sb.json(&["gc", "--merged-branches", "--base", &merge_oid]);
    assert_eq!(sha_code, 0, "{sha_out}");
    assert_eq!(sha_out["merged_branches_deleted"], json!([]));
    assert!(
        !gh_log.exists(),
        "raw SHA --base must not invoke gh pr list"
    );

    // Dry-run previews cleanup without mutating disk or git refs.
    let (dry_code, dry_out) = sb.json(&["gc", "--merged-branches", "--dry-run"]);
    assert_eq!(dry_code, 0, "{dry_out}");
    assert_eq!(dry_out["merged_branches_deleted"], json!(["fix/squashed"]));
    assert!(wt.is_dir() && branch_exists(&sb, "fix/squashed"));

    // Real run removes both clean worktree and squash-merged branch.
    let (code, out) = sb.json(&["gc", "--merged-branches"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["status"], json!("ok"));
    assert_eq!(out["merged_branches_deleted"], json!(["fix/squashed"]));
    assert!(!wt.exists() && !branch_exists(&sb, "fix/squashed"));
}
