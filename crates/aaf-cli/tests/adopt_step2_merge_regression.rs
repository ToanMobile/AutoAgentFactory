#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression for `aaf merge` after the adopt primitives landed: none of the
//! new flags (`strict_merged_head`, `keep_non_aaf_local`, `no_receipt_reuse`,
//! `run_commands_despite_head_lock`) is on for the human's command, so a
//! merged PR at another head is still `ALREADY_MERGED` and a merged chat branch
//! is still cleaned up locally.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

const AAF_BIN: &str = env!("CARGO_BIN_EXE_aaf");

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn ref_exists(repo: &Path, refname: &str) -> bool {
    Command::new("git")
        .args(["show-ref", "--verify", "--quiet", refname])
        .current_dir(repo)
        .status()
        .unwrap()
        .success()
}

struct Fixture {
    _tmp: TempDir,
    root: PathBuf,
    bare: PathBuf,
    repo: PathBuf,
    data: PathBuf,
    config: PathBuf,
    worktrees: PathBuf,
    bin_dir: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let bare = root.join("origin.git");
        let repo = root.join("repo");
        let data = root.join("data");
        let config = root.join("config");
        let worktrees = root.join("worktrees");
        let bin_dir = root.join("bin");
        for dir in [&bare, &repo, &data, &config, &worktrees, &bin_dir] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(
            config.join("config.toml"),
            "[machine]\nmax_load_percent = 0\n",
        )
        .unwrap();
        git(&bare, &["init", "--bare", "-b", "main"]);
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
        fs::create_dir_all(repo.join(".autofactory")).unwrap();
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(
            repo.join(".autofactory/config.toml"),
            "[commands]\ntest_scoped = \"true\"\n[gate]\nhead_lock_globs = []\n",
        )
        .unwrap();
        fs::write(repo.join("src/lib.rs"), "pub fn v() -> i32 { 1 }\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-m", "initial"]);
        git(&repo, &["push", "-u", "origin", "main"]);
        Self {
            _tmp: tmp,
            root,
            bare,
            repo,
            data,
            config,
            worktrees,
            bin_dir,
        }
    }

    /// A pushed chat branch (not `aaf/`) for PR `pr`; returns its head.
    fn chat_pr(&self, branch: &str, pr: u64) -> String {
        git(&self.repo, &["checkout", "-b", branch]);
        fs::write(self.repo.join("src/lib.rs"), "pub fn v() -> i32 { 2 }\n").unwrap();
        git(&self.repo, &["add", "-A"]);
        git(&self.repo, &["commit", "-m", "chat change"]);
        let head = git(&self.repo, &["rev-parse", "HEAD"]);
        git(&self.repo, &["push", "-u", "origin", branch]);
        git(
            &self.bare,
            &["update-ref", &format!("refs/pull/{pr}/head"), &head],
        );
        git(&self.repo, &["checkout", "main"]);
        head
    }

    fn write_gh_stub(&self, pr: u64, state: &str, view_head: &str, branch: &str, merge_oid: &str) {
        let view = format!(
            r#"{{"number":{pr},"state":"{state}","headRefOid":"{view_head}","headRefName":"{branch}","baseRefName":"main","mergeable":"MERGEABLE","isDraft":false,"title":"PR #{pr}","url":"https://github.com/org/repo/pull/{pr}","isCrossRepository":false,"mergeCommit":{{"oid":"{merge_oid}"}}}}"#
        );
        let script = format!(
            r#"#!/bin/sh
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  printf '%s\n' '{view}'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "checks" ]; then
  printf '[]\n'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "merge" ]; then
  git --git-dir='{bare}' update-ref refs/heads/main '{merge_oid}'
  exit 0
fi
echo "unexpected gh args: $*" >&2
exit 1
"#,
            bare = self.bare.display(),
        );
        let gh = self.bin_dir.join("gh");
        fs::write(&gh, script).unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn run_aaf(&self, args: &[&str]) -> (i32, Value, String) {
        let sys_path = std::env::var("PATH").unwrap_or_default();
        let out = Command::new(AAF_BIN)
            .args(args)
            .current_dir(&self.repo)
            .env("PATH", format!("{}:{sys_path}", self.bin_dir.display()))
            .env("HOME", self.root.join("home"))
            .env("AAF_DATA_DIR", &self.data)
            .env("AAF_CONFIG_DIR", &self.config)
            .env("AAF_WORKTREE_DIR", &self.worktrees)
            .env_remove("AAF_RUN_ID")
            .output()
            .unwrap();
        let code = out.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let json: Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|e| panic!("bad JSON ({code}): {e}\n{stdout}\n{stderr}"));
        (code, json, stderr)
    }
}

#[test]
fn aaf_merge_still_removes_a_merged_chat_branch_locally_and_remotely() {
    let fx = Fixture::new();
    let head = fx.chat_pr("feat/chat", 21);
    fx.write_gh_stub(21, "OPEN", &head, "feat/chat", &head);
    assert!(ref_exists(&fx.repo, "refs/heads/feat/chat"));

    let (code, json, stderr) = fx.run_aaf(&["merge", "21", "--expect-head", &head, "--json"]);
    assert_eq!(code, 0, "json={json} stderr={stderr}");
    assert_eq!(json["status"], "MERGED");
    assert!(
        !ref_exists(&fx.repo, "refs/heads/feat/chat"),
        "the human's `aaf merge` keeps deleting the merged local branch"
    );
    assert!(!ref_exists(&fx.bare, "refs/heads/feat/chat"));
}

#[test]
fn aaf_merge_still_reports_already_merged_for_another_head() {
    let fx = Fixture::new();
    let head = fx.chat_pr("feat/chat", 22);
    // The PR is merged, but GitHub's head is not the one `--expect-head` names.
    let other = "c".repeat(40);
    git(&fx.bare, &["update-ref", "refs/heads/main", &head]);
    fx.write_gh_stub(22, "MERGED", &other, "feat/chat", &head);

    let (code, json, stderr) = fx.run_aaf(&["merge", "22", "--expect-head", &head, "--json"]);
    assert_eq!(code, 0, "json={json} stderr={stderr}");
    assert_eq!(
        json["status"], "ALREADY_MERGED",
        "only the adopt lane's `strict_merged_head` turns this into merged_unpinned: {json}"
    );
}
