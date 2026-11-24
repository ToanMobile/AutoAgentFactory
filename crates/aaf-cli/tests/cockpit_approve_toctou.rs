#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
//! `aaf cockpit` approve TOCTOU: the head-lock approval is bound to the locked
//! diff the reviewer was shown. Split out of `v32_cockpit_cli.rs` (600-line cap).

#[path = "../src/ctx.rs"]
mod ctx;
#[path = "../src/output.rs"]
mod output;

#[path = "../src/cmd"]
mod cmd {
    pub mod board {
        pub mod store;
    }
    pub mod cockpit;
    pub mod cockpit_actions;
    pub mod handover;
    pub mod scope;
    pub mod task_ops;
}

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use aaf_domain::config::RepoConfig;
use aaf_domain::gate::LockStatus;
use aaf_domain::ports::TaskStore;
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::gate::{GIT_TIMEOUT, base, head_lock};
use aaf_engine::git::worktree;
use tempfile::TempDir;

use crate::cmd::cockpit::run_cockpit;
use crate::ctx::Ctx;
use crate::output::Exit;

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=f@example.invalid",
        ])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn fixture_ctx() -> (TempDir, Ctx) {
    let tmp = TempDir::new().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let repo = root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("README.md"), "cockpit fixture\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    let ctx = Ctx {
        cwd: repo,
        home: root.join("home"),
        data_dir: root.join("data"),
        config_dir: root.join("config"),
        worktrees_dir: root.join("worktrees"),
        path_var: std::env::var_os("PATH").unwrap_or_default(),
        run_id: None,
        stdin_is_tty: true,
        aaf_env: Vec::new(),
    };
    fs::create_dir_all(&ctx.data_dir).unwrap();
    (tmp, ctx)
}

fn spec(desc: &str) -> ImmutableSpec {
    ImmutableSpec {
        description: desc.to_string(),
        acceptance: vec!["passes verification".to_string()],
    }
}

#[test]
fn cockpit_approve_refuses_when_locked_file_changes_during_confirm() {
    use aaf_engine::db::RoDbCache;

    struct MutatingConfirm(std::path::PathBuf, &'static [u8], bool);
    impl std::io::Read for MutatingConfirm {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.2 {
                return Ok(0);
            }
            self.2 = true;
            fs::write(&self.0, self.1)?;
            let n = buf.len().min(2);
            buf[..n].copy_from_slice(&b"y\n"[..n]);
            Ok(n)
        }
    }

    let (_tmp, ctx) = fixture_ctx();
    let repo = ctx.cwd.clone();
    fs::create_dir_all(repo.join("tests")).unwrap();
    fs::write(repo.join("tests/it.rs"), "// v1\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "add test"]);
    fs::write(repo.join("tests/it.rs"), "// v2\n").unwrap();
    {
        let db = Db::open(&ctx.data_dir.join("aaf.db")).unwrap();
        let mut task = Task::new_root(
            "t-toctou",
            repo.to_string_lossy(),
            "Approve locked change",
            spec("toctou"),
            Priority::P1,
            1_700_000_000,
        );
        task.state = TaskState::Blocked;
        db.insert_task(&task).unwrap();
    }
    let ro = RoDbCache::new(ctx.data_dir.join("aaf.db"));
    let mut rdr =
        std::io::BufReader::new(MutatingConfirm(repo.join("tests/it.rs"), b"// v3\n", false));
    let mut out = Vec::new();
    cmd::cockpit_actions::action_approve(&ctx, &ro, "t-toctou", Some("alice"), &mut rdr, &mut out)
        .unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("lock_fp mismatch"), "{text}");

    let cfg = RepoConfig::default();
    let b = base::resolve(&repo, &cfg.gate.base_ref, GIT_TIMEOUT).unwrap();
    let eval = head_lock::evaluate(&ctx.data_dir, &repo, &cfg, &b.diff_target);
    assert_eq!(eval.status, LockStatus::Blocked);
}

#[test]
fn cockpit_approve_refuses_when_base_ref_moves_during_confirm() {
    use aaf_engine::db::RoDbCache;

    struct MovingBaseConfirm(std::path::PathBuf, String, bool);
    impl std::io::Read for MovingBaseConfirm {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.2 {
                return Ok(0);
            }
            self.2 = true;
            git(&self.0, &["merge", "--ff-only", &self.1]);
            let n = buf.len().min(2);
            buf[..n].copy_from_slice(&b"y\n"[..n]);
            Ok(n)
        }
    }

    let (_tmp, ctx) = fixture_ctx();
    let repo = ctx.cwd.clone();
    fs::create_dir_all(repo.join("tests")).unwrap();
    fs::write(repo.join("tests/a.rs"), "// a0\n").unwrap();
    fs::write(repo.join("tests/b.rs"), "// b0\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "add locked tests"]);

    let wt = worktree::create(&repo, &ctx.worktrees_dir, "t-base", "main").unwrap();
    fs::write(wt.path.join("tests/b.rs"), "// b1\n").unwrap();
    git(&wt.path, &["add", "-A"]);
    git(&wt.path, &["commit", "-q", "-m", "edit b"]);
    let c1 = git(&wt.path, &["rev-parse", "HEAD"]);
    fs::write(wt.path.join("tests/a.rs"), "// a1\n").unwrap();
    git(&wt.path, &["add", "-A"]);
    git(&wt.path, &["commit", "-q", "-m", "edit a"]);

    {
        let db = Db::open(&ctx.data_dir.join("aaf.db")).unwrap();
        let mut task = Task::new_root(
            "t-base",
            repo.to_string_lossy(),
            "Approve with moving base",
            spec("base move toctou"),
            Priority::P1,
            1_700_000_000,
        );
        task.state = TaskState::Blocked;
        db.insert_task(&task).unwrap();
    }

    let ro = RoDbCache::new(ctx.data_dir.join("aaf.db"));
    let mut rdr = std::io::BufReader::new(MovingBaseConfirm(repo, c1, false));
    let mut out = Vec::new();
    cmd::cockpit_actions::action_approve(&ctx, &ro, "t-base", Some("alice"), &mut rdr, &mut out)
        .unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("lock_fp mismatch"), "{text}");

    let cfg = RepoConfig::default();
    let b = base::resolve(&wt.path, &cfg.gate.base_ref, GIT_TIMEOUT).unwrap();
    let eval = head_lock::evaluate(&ctx.data_dir, &wt.path, &cfg, &b.diff_target);
    assert_eq!(eval.status, LockStatus::Blocked);
}

/// Yields `lines` one by one, running `hook` right before line `at` is read.
struct HookReader<F: FnMut()> {
    lines: Vec<Vec<u8>>,
    idx: usize,
    pos: usize,
    at: usize,
    hook: Option<F>,
}

impl<F: FnMut()> std::io::Read for HookReader<F> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::io::BufRead;
        let n = {
            let avail = self.fill_buf()?;
            let n = avail.len().min(buf.len());
            buf[..n].copy_from_slice(&avail[..n]);
            n
        };
        self.consume(n);
        Ok(n)
    }
}

impl<F: FnMut()> std::io::BufRead for HookReader<F> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        if self.idx == self.at
            && let Some(mut hook) = self.hook.take()
        {
            hook();
        }
        Ok(self.lines.get(self.idx).map_or(&[][..], |l| &l[self.pos..]))
    }

    fn consume(&mut self, amt: usize) {
        self.pos += amt;
        if self
            .lines
            .get(self.idx)
            .is_some_and(|l| self.pos >= l.len())
        {
            (self.idx, self.pos) = (self.idx + 1, 0);
        }
    }
}

#[test]
fn a13_cockpit_approve_refuses_when_locked_diff_changes_after_preview() {
    let (_tmp, ctx) = fixture_ctx();
    let repo = ctx.cwd.clone();
    fs::create_dir_all(repo.join("tests")).unwrap();
    fs::write(repo.join("tests/it.rs"), "// v1\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "add locked test"]);
    fs::write(repo.join("tests/it.rs"), "// v2\n").unwrap();
    {
        let db = Db::open(&ctx.data_dir.join("aaf.db")).unwrap();
        let task = Task::new_root(
            "t-toctou",
            repo.to_string_lossy(),
            "Approve locked change",
            spec("Approve test edit"),
            Priority::P1,
            1_700_000_000,
        );
        db.insert_task(&task).unwrap();
    }
    let it = repo.join("tests/it.rs");
    let mut reader = HookReader {
        lines: ["approve t-toctou bob\n", "y\n", "q\n"]
            .iter()
            .map(|l| l.as_bytes().to_vec())
            .collect(),
        idx: 0,
        pos: 0,
        at: 1,
        hook: Some(|| fs::write(&it, "// v3 sneaked in after preview\n").unwrap()),
    };
    let mut out_buf: Vec<u8> = Vec::new();
    let res = run_cockpit(&ctx, None, false, &mut reader, &mut out_buf).unwrap();
    let out = String::from_utf8(out_buf).unwrap();
    assert_eq!(res.exit, Exit::Ok);
    assert!(out.contains("Confirm approve t-toctou [y/N]: "), "{out}");
    let cfg = RepoConfig::default();
    let base_info = base::resolve(&repo, &cfg.gate.base_ref, GIT_TIMEOUT).unwrap();
    assert_eq!(
        head_lock::evaluate(&ctx.data_dir, &repo, &cfg, &base_info.diff_target).status,
        LockStatus::Blocked,
        "approval signed a locked diff the reviewer never saw: {out}"
    );
    assert!(
        out.contains("locked diff changed since preview; run approve again"),
        "{out}"
    );
    assert!(!out.contains("approved t-toctou"), "{out}");
}
