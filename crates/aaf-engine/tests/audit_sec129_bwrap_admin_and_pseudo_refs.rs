#![allow(clippy::unwrap_used, clippy::expect_used)]
//! SEC-129 (Linux Bubblewrap): sibling linked worktree admin dirs under
//! `<common>/worktrees/*` and pseudo-refs (`ORIG_HEAD`, `FETCH_HEAD`, ...) in
//! `<common>` must be read-only in `bwrap` argv while keeping the active
//! worktree's own admin dir (`<common>/worktrees/wt_a/index`) writable and
//! preserving the RFC-126 reflog mount layout.

use std::fs;
use std::path::{Path, PathBuf};

use aaf_engine::executors::linux_sandbox::build_bwrap_args;
use aaf_engine::executors::sandbox::SandboxPaths;
use tempfile::TempDir;

const GIT_FLAGS: [&str; 10] = [
    "-c",
    "user.name=aaf-test",
    "-c",
    "user.email=aaf-test@example.invalid",
    "-c",
    "commit.gpgsign=false",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.logAllRefUpdates=true",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MountMode {
    Ro,
    Rw,
}

fn git(repo: &Path, args: &[&str]) -> String {
    let mut full = GIT_FLAGS.to_vec();
    full.extend_from_slice(args);
    aaf_fp::git(repo, &full, &[], aaf_fp::GIT_TIMEOUT).unwrap()
}

fn path_str(path: &Path) -> String {
    path.to_str()
        .expect("tempdir path must be valid UTF-8")
        .to_string()
}

/// Computes the effective mount mode (`Ro` or `Rw`) for `target` from `bwrap`
/// argv order before `--`.
///
/// Bubblewrap applies `--ro-bind` / `--bind` mounts sequentially: each mount
/// whose destination equals `target` or is an ancestor directory of `target`
/// (`target.starts_with(dest)`, component-wise) supersedes earlier mounts (a
/// later mount on the same path wins, a child mount overlays its parent, and a
/// later parent overmount would shadow an earlier child mount).
fn effective_mount_mode(argv: &[String], target: &Path) -> Option<MountMode> {
    let mut mode = None;
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--" => break,
            "--ro-bind" | "--bind" if i + 2 < argv.len() => {
                let dest = Path::new(&argv[i + 2]);
                if target.starts_with(dest) {
                    mode = Some(if argv[i] == "--ro-bind" {
                        MountMode::Ro
                    } else {
                        MountMode::Rw
                    });
                }
                i += 3;
            }
            "--dev" | "--proc" | "--tmpfs" | "--chdir" => {
                i += 2;
            }
            _ => {
                i += 1;
            }
        }
    }
    mode
}

fn has_self_bind(argv: &[String], flag: &str, path: &str) -> bool {
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--" => break,
            "--ro-bind" | "--bind" if i + 2 < argv.len() => {
                if argv[i] == flag && argv[i + 1] == path && argv[i + 2] == path {
                    return true;
                }
                i += 3;
            }
            "--dev" | "--proc" | "--tmpfs" | "--chdir" => {
                i += 2;
            }
            _ => {
                i += 1;
            }
        }
    }
    false
}

struct Fixture {
    _tmp: TempDir,
    common: PathBuf,
    argv: Vec<String>,
}

fn setup_fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let main_repo = root.join("main");
    let wt_a = root.join("wt_a");
    let wt_b = root.join("wt_b");

    fs::create_dir(&main_repo).unwrap();
    git(&main_repo, &["init", "-q", "-b", "main"]);
    fs::write(main_repo.join("README.md"), "init\n").unwrap();
    git(&main_repo, &["add", "README.md"]);
    git(&main_repo, &["commit", "-q", "-m", "initial"]);

    git(
        &main_repo,
        &["worktree", "add", "-q", "-b", "aaf/T1", &path_str(&wt_a)],
    );
    git(
        &main_repo,
        &["worktree", "add", "-q", "-b", "aaf/T2", &path_str(&wt_b)],
    );

    let common = PathBuf::from(git(
        &wt_a,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ))
    .canonicalize()
    .unwrap();

    let oid = format!("{}\n", git(&main_repo, &["rev-parse", "HEAD"]).trim());
    fs::write(common.join("ORIG_HEAD"), &oid).unwrap();
    fs::write(common.join("FETCH_HEAD"), &oid).unwrap();
    fs::write(common.join("worktrees/wt_b/ORIG_HEAD"), &oid).unwrap();

    assert!(common.join("worktrees/wt_a/index").is_file());
    assert!(common.join("worktrees/wt_b/HEAD").is_file());
    assert!(common.join("worktrees/wt_b/index").is_file());
    assert!(common.join("worktrees/wt_b/ORIG_HEAD").is_file());
    assert!(common.join("logs/refs/heads/aaf/T1").is_file());

    // Mirrors `executors::confined_paths::build_sandbox_paths` (private module):
    // `worktree` is the task's cwd (`wt_a`) and `git_dirs` contains only `--git-common-dir`.
    let paths = SandboxPaths {
        worktree: Some(path_str(&wt_a)),
        git_dirs: vec![path_str(&common)],
        ..SandboxPaths::default()
    };
    let argv = build_bwrap_args(&paths, "/bin/true", &[]).unwrap();

    Fixture {
        _tmp: tmp,
        common,
        argv,
    }
}

#[test]
fn sibling_worktree_admin_dir_is_read_only() {
    let fx = setup_fixture();
    for rel in [
        "worktrees/wt_b/HEAD",
        "worktrees/wt_b/index",
        "worktrees/wt_b/ORIG_HEAD",
    ] {
        let target = fx.common.join(rel);
        assert_eq!(
            effective_mount_mode(&fx.argv, &target),
            Some(MountMode::Ro),
            "sibling worktree admin file {rel} ({}) must be effectively read-only in bwrap argv: {:?}",
            target.display(),
            fx.argv
        );
    }
}

#[test]
fn common_pseudo_refs_are_read_only() {
    let fx = setup_fixture();
    for rel in ["ORIG_HEAD", "FETCH_HEAD"] {
        let target = fx.common.join(rel);
        assert_eq!(
            effective_mount_mode(&fx.argv, &target),
            Some(MountMode::Ro),
            "common git dir pseudo-ref {rel} ({}) must be effectively read-only in bwrap argv: {:?}",
            target.display(),
            fx.argv
        );
    }
}

#[test]
fn own_worktree_admin_stays_writable() {
    let fx = setup_fixture();
    let own_index = fx.common.join("worktrees/wt_a/index");
    assert_eq!(
        effective_mount_mode(&fx.argv, &own_index),
        Some(MountMode::Rw),
        "active worktree index ({}) must remain writable in bwrap argv: {:?}",
        own_index.display(),
        fx.argv
    );
}

#[test]
fn rfc126_reflog_layout_unchanged() {
    let fx = setup_fixture();
    let common_logs = fx.common.join("logs");
    let own_branch_reflog = fx.common.join("logs/refs/heads/aaf/T1");

    assert!(
        has_self_bind(&fx.argv, "--ro-bind", &path_str(&common_logs)),
        "expected --ro-bind for {} in bwrap argv: {:?}",
        common_logs.display(),
        fx.argv
    );
    assert!(
        has_self_bind(&fx.argv, "--bind", &path_str(&own_branch_reflog)),
        "expected --bind for {} in bwrap argv: {:?}",
        own_branch_reflog.display(),
        fx.argv
    );
    assert_eq!(
        effective_mount_mode(&fx.argv, &common_logs),
        Some(MountMode::Ro)
    );
    assert_eq!(
        effective_mount_mode(&fx.argv, &own_branch_reflog),
        Some(MountMode::Rw)
    );
}
