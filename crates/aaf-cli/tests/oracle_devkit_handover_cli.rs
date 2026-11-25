#![allow(clippy::unwrap_used, clippy::expect_used)]
//! CLI oracle tests for automatic DevKit handover report generation on
//! `aaf merge` and `aaf run`.

#[path = "factory_support.rs"]
mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use aaf_domain::ports::TaskStore;
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::git::worktree;
use aaf_engine::handover::CSP_META;
use serde_json::{Value, json};
use support::{Fixture, git};
use tempfile::TempDir;

const AAF_BIN: &str = env!("CARGO_BIN_EXE_aaf");

fn git_in(dir: &Path, args: &[&str]) -> String {
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
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

struct MergeFixture {
    _tmp: TempDir,
    root: PathBuf,
    bare: PathBuf,
    repo: PathBuf,
    data: PathBuf,
    config: PathBuf,
    worktrees: PathBuf,
    bin_dir: PathBuf,
}

impl MergeFixture {
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

        git_in(&bare, &["init", "--bare", "-b", "main"]);
        git_in(&repo, &["init", "-b", "main"]);
        git_in(&repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
        fs::create_dir_all(repo.join(".autofactory")).unwrap();
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(
            repo.join(".autofactory/config.toml"),
            "[commands]\ntest_scoped = \"true\"\n[gate]\nhead_lock_globs = []\n",
        )
        .unwrap();
        fs::write(repo.join("src/lib.rs"), "pub fn v() -> i32 { 1 }\n").unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-m", "initial"]);
        git_in(&repo, &["push", "-u", "origin", "main"]);

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

    fn write_gh_stub(&self, pr_number: u64, head: &str, branch: &str) {
        let view_json = format!(
            r#"{{"number":{pr_number},"state":"OPEN","headRefOid":"{head}","headRefName":"{branch}","baseRefName":"main","mergeable":"MERGEABLE","isDraft":false,"title":"PR #{pr_number}","url":"https://github.com/org/repo/pull/{pr_number}","isCrossRepository":false,"mergeCommit":null}}"#
        );
        let script = format!(
            r#"#!/bin/sh
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  printf '%s\n' '{view_json}'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "checks" ]; then
  printf '[]\n'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "merge" ]; then
  git --git-dir='{bare}' update-ref refs/heads/main '{head}'
  exit 0
fi
echo "unexpected gh args: $*" >&2
exit 1
"#,
            bare = self.bare.display(),
        );
        let gh_path = self.bin_dir.join("gh");
        fs::write(&gh_path, script).unwrap();
        fs::set_permissions(&gh_path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn run_aaf(&self, args: &[&str]) -> (i32, Value, String) {
        let sys_path = std::env::var("PATH").unwrap_or_default();
        let path_env = format!("{}:{sys_path}", self.bin_dir.display());
        let out = Command::new(AAF_BIN)
            .args(args)
            .current_dir(&self.repo)
            .env("PATH", path_env)
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
        let val: Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
            panic!("invalid JSON from aaf {args:?} (exit {code}): {e}\nstdout:\n{stdout}\nstderr:\n{stderr}")
        });
        (code, val, stderr)
    }
}

#[test]
fn merge_auto_generates_devkit_handover_html_and_md_with_json_field() {
    let fx = MergeFixture::new();
    let wt = worktree::create(&fx.repo, &fx.worktrees, "t1", "main").unwrap();
    fs::write(wt.path.join("src/lib.rs"), "pub fn v() -> i32 { 7 }\n").unwrap();
    git_in(&wt.path, &["add", "-A"]);
    git_in(&wt.path, &["commit", "-m", "feat t1"]);
    let head = git_in(&wt.path, &["rev-parse", "HEAD"]);
    git_in(&wt.path, &["push", "-u", "origin", &wt.branch]);
    git_in(&fx.bare, &["update-ref", "refs/pull/7/head", &head]);

    let db = Db::open(&fx.data.join("aaf.db")).unwrap();
    let mut task = Task::new_root(
        "t1",
        fx.repo.to_string_lossy(),
        "Bàn giao tính năng merge report",
        ImmutableSpec {
            description: "Tự động xuất báo cáo nghiệm thu sau merge".to_string(),
            acceptance: vec!["Tự tạo HTML và MD".to_string()],
        },
        Priority::P1,
        1_700_000_000,
    );
    task.state = TaskState::Review;
    task.context_notes
        .push("Nguyên nhân: Thiếu bước tự động xuất báo cáo sau aaf merge".to_string());
    db.insert_task(&task).unwrap();
    drop(db);

    fx.write_gh_stub(7, &head, "aaf/t1");

    let (code, body, stderr) = fx.run_aaf(&["merge", "7", "--expect-head", &head, "--json"]);
    assert_eq!(code, 0, "body={body} stderr={stderr}");
    assert_eq!(body["status"], json!("MERGED"));

    let hr = &body["handover_report"];
    assert_eq!(hr["task_id"], json!("t1"), "body={body}");
    assert!(hr["html"].as_str().unwrap().ends_with("handover-t1.html"));
    assert!(hr["markdown"].as_str().unwrap().ends_with("handover-t1.md"));

    let html_path = fx.data.join("reports/handover-t1.html");
    let md_path = fx.data.join("reports/handover-t1.md");
    assert!(html_path.is_file(), "missing {}", html_path.display());
    assert!(md_path.is_file(), "missing {}", md_path.display());

    let html = fs::read_to_string(&html_path).unwrap();
    assert!(html.contains(CSP_META));
    assert!(html.contains("Tóm Tắt 3 Dòng (Chuẩn B10 DevKit)"));
    assert!(html.contains("gate:"));
    assert!(html.contains("1. Đã làm gì / Đã fix gì"));
    assert!(html.contains("2. Chặn bug cũ"));
    assert!(html.contains("3. Nguy cơ bug mới"));
    assert!(html.contains("4. An toàn mã nguồn"));
    assert!(html.contains("5. Còn hở / chưa kiểm"));
    assert!(html.contains("Nguyên nhân:"));
    assert!(html.contains("Thiếu bước tự động xuất báo cáo sau aaf merge"));
    assert!(html.contains("Automated QA Suite"));

    let md = fs::read_to_string(&md_path).unwrap();
    assert!(md.contains("Nguyên nhân:"));
    assert!(md.contains("## Acceptance Report"));
}

#[test]
fn run_auto_generates_devkit_handover_html_and_md_and_surfaces_handover_reports_json() {
    let fx = Fixture::new();
    let dir = fx.repo.join(".autofactory");
    fs::create_dir_all(&dir).unwrap();
    let config = format!(
        "[commands]\ntest_scoped = {}\n\n[run]\nexecutor = \"custom\"\n\
         executor_cmd = [\"/bin/sh\", \"-c\", {}, \"{{prompt}}\"]\nrun_timeout_secs = 60\n",
        json!("test -f done.txt"),
        json!("echo made > done.txt")
    );
    fs::write(dir.join("config.toml"), config).unwrap();
    git(&fx.repo, &["add", "-A"]);
    git(&fx.repo, &["commit", "--quiet", "-m", "configure aaf"]);

    let id = fx.add("make the handover file");
    let out = fx.aaf(&["run", "--unsandboxed"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["status"], json!("ok"));
    assert_eq!(out.json["review"], json!([id]));

    let reports = out.json["handover_reports"]
        .as_array()
        .expect("handover_reports array");
    assert_eq!(reports.len(), 1, "json={}", out.json);
    assert_eq!(reports[0]["task_id"], json!(id));
    assert_eq!(reports[0]["status"], json!("XONG"));

    let html_path = fx.data.join(format!("reports/handover-{id}.html"));
    let md_path = fx.data.join(format!("reports/handover-{id}.md"));
    assert!(html_path.is_file(), "missing {}", html_path.display());
    assert!(md_path.is_file(), "missing {}", md_path.display());

    let html = fs::read_to_string(&html_path).unwrap();
    assert!(html.contains(CSP_META));
    assert!(html.contains("✔ XONG"));
    assert!(html.contains("Tóm Tắt 3 Dòng (Chuẩn B10 DevKit)"));
    assert!(html.contains("Nguyên nhân:"));
    assert!(html.contains("Automated QA Suite"));
    assert!(html.contains("test_scoped"));

    let md = fs::read_to_string(&md_path).unwrap();
    assert!(md.starts_with("XONG\n"), "md={md}");
    assert!(md.contains("gate: PASS/exit 0"));
    assert!(md.contains("## Automated QA Suite [tự chạy]"));
    assert!(md.contains("test_scoped"));
}
