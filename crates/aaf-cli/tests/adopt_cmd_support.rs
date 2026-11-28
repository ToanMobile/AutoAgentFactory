//! Shared fixture of the `adopt_cmd_*` tests (included with `#[path]`): a bare
//! `origin`, a repository with `main` pushed, and a fake `gh` first on `PATH`.
//! No real GitHub is ever reached: `gh` is a shell script that answers
//! `pr view` from a JSON file, records every call, and keeps the comment.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::new_without_default
)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use aaf_domain::event::{EventKind, FactoryEvent, PrAdoptedPayload};
use aaf_domain::ports::{EventSink, TaskStore};
use aaf_domain::task::{Task, TaskState};
use aaf_engine::db::Db;
use serde_json::{Value, json};
use tempfile::TempDir;

pub const AAF_BIN: &str = env!("CARGO_BIN_EXE_aaf");

pub fn git(dir: &Path, args: &[&str]) -> String {
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
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
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

/// What the fake `gh pr view` answers.
#[derive(Clone)]
pub struct View {
    pub number: u64,
    pub state: &'static str,
    pub head_oid: String,
    pub head_name: String,
    pub base_name: &'static str,
    pub draft: bool,
    pub fork: bool,
    /// `None` is a JSON `null` author.
    pub author: Option<&'static str>,
}

impl View {
    pub fn open(number: u64, head_oid: &str, head_name: &str) -> View {
        View {
            number,
            state: "OPEN",
            head_oid: head_oid.to_string(),
            head_name: head_name.to_string(),
            base_name: "main",
            draft: false,
            fork: false,
            author: Some("alex"),
        }
    }

    fn json(&self) -> Value {
        json!({
            "number": self.number,
            "state": self.state,
            "headRefOid": self.head_oid,
            "headRefName": self.head_name,
            "baseRefName": self.base_name,
            "mergeable": "MERGEABLE",
            "isDraft": self.draft,
            "title": format!("Chat change {}", self.number),
            "url": format!("https://github.com/org/repo/pull/{}", self.number),
            "isCrossRepository": self.fork,
            "mergeCommit": null,
            "author": self.author.map(|login| json!({ "login": login })),
            "body": "Please adopt.\n\nAcceptance:\n- it works",
        })
    }
}

pub struct Run {
    pub code: i32,
    pub json: Value,
    pub stderr: String,
}

pub struct Fixture {
    pub tmp: TempDir,
    pub root: PathBuf,
    pub bare: PathBuf,
    pub repo: PathBuf,
    pub data: PathBuf,
    pub config: PathBuf,
    pub worktrees: PathBuf,
    pub bin: PathBuf,
    /// Where the fake `gh` keeps its state (`view.json`, `calls.log`, `comment.txt`).
    pub gh_state: PathBuf,
}

const GH_SCRIPT: &str = r#"#!/bin/sh
state="$(dirname "$0")/../gh-state"
echo "$*" >> "$state/calls.log"
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  cat "$state/view.json"
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "comment" ]; then
  if [ -e "$state/comment-fails" ]; then
    echo "comment refused" >&2
    exit 1
  fi
  printf '%s\n' "$4" > "$state/comment.txt"
  exit 0
fi
echo "unexpected gh args: $*" >&2
exit 1
"#;

impl Fixture {
    /// `allowlist` is the TOML array body of `[safety].owner_github_allowlist`.
    pub fn new(allowlist: &str) -> Fixture {
        Fixture::build(allowlist, true)
    }

    /// A repository whose base has no remote.
    pub fn without_remote() -> Fixture {
        Fixture::build("\"alex\"", false)
    }

    fn build(allowlist: &str, with_remote: bool) -> Fixture {
        let tmp = TempDir::new().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let bare = root.join("origin.git");
        let repo = root.join("repo");
        let data = root.join("data");
        let config = root.join("config");
        let worktrees = root.join("worktrees");
        let bin = root.join("bin");
        let gh_state = root.join("gh-state");
        for dir in [&bare, &repo, &config, &bin, &gh_state, &root.join("home")] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(
            config.join("config.toml"),
            format!(
                "[machine]\nmax_load_percent = 0\n[safety]\nowner_github_allowlist = [{allowlist}]\n"
            ),
        )
        .unwrap();
        let gh = bin.join("gh");
        fs::write(&gh, GH_SCRIPT).unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();

        git(&bare, &["init", "--bare", "-b", "main"]);
        git(&repo, &["init", "-b", "main"]);
        if with_remote {
            git(&repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
        }
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
        if with_remote {
            git(&repo, &["push", "-u", "origin", "main"]);
        }
        Fixture {
            tmp,
            root,
            bare,
            repo,
            data,
            config,
            worktrees,
            bin,
            gh_state,
        }
    }

    /// A pushed chat branch with one change, served as `refs/pull/<pr>/head`.
    /// Returns its head.
    pub fn chat_pr(&self, branch: &str, pr: u64) -> String {
        git(&self.repo, &["checkout", "-b", branch, "main"]);
        fs::write(self.repo.join("src/lib.rs"), "pub fn v() -> i32 { 2 }\n").unwrap();
        git(&self.repo, &["add", "-A"]);
        git(&self.repo, &["commit", "-m", "chat change"]);
        let head = git(&self.repo, &["rev-parse", "HEAD"]);
        git(&self.repo, &["push", "origin", branch]);
        self.serve_pull_ref(pr, &head);
        git(&self.repo, &["checkout", "main"]);
        head
    }

    /// A chat branch that changes a file and changes it back, while `main`
    /// moves on: the head is not an ancestor of `main` and merges to nothing.
    pub fn self_cancelling_pr(&self, branch: &str, pr: u64) -> String {
        git(&self.repo, &["checkout", "-b", branch, "main"]);
        fs::write(self.repo.join("src/lib.rs"), "pub fn v() -> i32 { 2 }\n").unwrap();
        git(&self.repo, &["commit", "-am", "change"]);
        fs::write(self.repo.join("src/lib.rs"), "pub fn v() -> i32 { 1 }\n").unwrap();
        git(&self.repo, &["commit", "-am", "change it back"]);
        let head = git(&self.repo, &["rev-parse", "HEAD"]);
        git(&self.repo, &["push", "origin", branch]);
        self.serve_pull_ref(pr, &head);
        git(&self.repo, &["checkout", "main"]);
        fs::write(self.repo.join("README.md"), "base moved\n").unwrap();
        git(&self.repo, &["add", "-A"]);
        git(&self.repo, &["commit", "-m", "base moves on"]);
        git(&self.repo, &["push", "origin", "main"]);
        head
    }

    pub fn serve_pull_ref(&self, pr: u64, oid: &str) {
        git(
            &self.bare,
            &["update-ref", &format!("refs/pull/{pr}/head"), oid],
        );
    }

    pub fn show_pr(&self, view: &View) {
        fs::write(self.gh_state.join("view.json"), view.json().to_string()).unwrap();
    }

    pub fn comment_fails(&self) {
        fs::write(self.gh_state.join("comment-fails"), "").unwrap();
    }

    pub fn gh_calls(&self) -> Vec<String> {
        fs::read_to_string(self.gh_state.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    pub fn comment(&self) -> Option<String> {
        fs::read_to_string(self.gh_state.join("comment.txt")).ok()
    }

    pub fn run_in(&self, cwd: &Path, args: &[&str], run_id: Option<&str>) -> Run {
        let sys_path = std::env::var("PATH").unwrap_or_default();
        let mut command = Command::new(AAF_BIN);
        command
            .arg("--json")
            .args(args)
            .current_dir(cwd)
            .env("PATH", format!("{}:{sys_path}", self.bin.display()))
            .env("HOME", self.root.join("home"))
            .env("AAF_DATA_DIR", &self.data)
            .env("AAF_CONFIG_DIR", &self.config)
            .env("AAF_WORKTREE_DIR", &self.worktrees)
            .env_remove("AAF_RUN_ID");
        if let Some(id) = run_id {
            command.env("AAF_RUN_ID", id);
        }
        let out = command.output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        Run {
            code: out.status.code().unwrap_or(-1),
            json: serde_json::from_str(stdout.trim()).unwrap_or(Value::Null),
            stderr: format!("{stderr}\nstdout: {stdout}"),
        }
    }

    /// The human (non-JSON) output of `aaf board <args>`.
    pub fn board_text(&self, args: &[&str]) -> String {
        let sys_path = std::env::var("PATH").unwrap_or_default();
        let out = Command::new(AAF_BIN)
            .arg("board")
            .args(args)
            .current_dir(&self.repo)
            .env("PATH", format!("{}:{sys_path}", self.bin.display()))
            .env("HOME", self.root.join("home"))
            .env("AAF_DATA_DIR", &self.data)
            .env("AAF_CONFIG_DIR", &self.config)
            .env("AAF_WORKTREE_DIR", &self.worktrees)
            .env_remove("AAF_RUN_ID")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    pub fn adopt(&self, number: u64) -> Run {
        let n = number.to_string();
        self.run_in(&self.repo, &["board", "adopt", &n], None)
    }

    /// `aaf --json board <args>` in the main checkout.
    pub fn board(&self, args: &[&str]) -> Run {
        let mut all = vec!["board"];
        all.extend_from_slice(args);
        self.run_in(&self.repo, &all, None)
    }

    pub fn db(&self) -> Db {
        fs::create_dir_all(&self.data).unwrap();
        Db::open(&self.data.join("aaf.db")).unwrap()
    }

    pub fn db_exists(&self) -> bool {
        self.data.join("aaf.db").is_file()
    }

    /// Every task in the store (none when the store was never created).
    pub fn tasks(&self) -> Vec<Task> {
        if !self.db_exists() {
            return Vec::new();
        }
        self.db().list_tasks(None, None).unwrap()
    }

    pub fn events(&self, task_id: &str) -> Vec<FactoryEvent> {
        let mut events = self.db().events_for_task(task_id, 100).unwrap();
        events.reverse();
        events
    }

    pub fn aaf_refs(&self) -> Vec<String> {
        let out = git(
            &self.repo,
            &["for-each-ref", "--format=%(refname)", "refs/heads/aaf/"],
        );
        out.lines().map(str::to_string).collect()
    }

    /// Every directory under the worktrees dir that holds a checkout.
    pub fn worktree_dirs(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(repos) = fs::read_dir(&self.worktrees) else {
            return found;
        };
        for repo in repos.flatten() {
            if let Ok(tasks) = fs::read_dir(repo.path()) {
                found.extend(tasks.flatten().map(|t| t.path()));
            }
        }
        found
    }

    /// Nothing was created: no task, no `aaf/*` ref, no worktree.
    pub fn assert_nothing_created(&self, what: &str) {
        assert!(self.tasks().is_empty(), "{what}: a task was created");
        assert!(self.aaf_refs().is_empty(), "{what}: an aaf/* ref was made");
        assert!(
            self.worktree_dirs().is_empty(),
            "{what}: a worktree was made: {:?}",
            self.worktree_dirs()
        );
        let registered = git(&self.repo, &["worktree", "list", "--porcelain"]);
        assert_eq!(
            registered.matches("worktree ").count(),
            1,
            "{what}: a worktree is registered:\n{registered}"
        );
    }

    /// Cancels an adopting task the way the lane does, with its reason.
    pub fn lane_cancels(&self, id: &str, code: &str, reason: &str) {
        let db = self.db();
        assert!(
            db.compare_and_set_state(id, TaskState::Adopting, TaskState::Cancelled, None, 50)
                .unwrap()
        );
        db.emit(
            &FactoryEvent::new(
                EventKind::AdoptAborted,
                json!({ "code": code, "reason": reason, "pr_number": 7 }).to_string(),
                51,
            )
            .for_task(id),
        )
        .unwrap();
    }

    pub fn pr_adopted(&self, id: &str) -> PrAdoptedPayload {
        let event = self
            .events(id)
            .into_iter()
            .find(|e| e.kind == EventKind::PrAdopted)
            .expect("a pr_adopted event");
        serde_json::from_str(&event.payload_json).expect("a PrAdoptedPayload")
    }
}
