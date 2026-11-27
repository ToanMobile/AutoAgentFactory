#![allow(clippy::unwrap_used, clippy::expect_used)]
//! RFC-126 (Linux Bubblewrap / SEC-116, SEC-136): reflog directory tree must be
//! read-only (`--ro-bind`) while keeping only the active task's own pre-existing
//! reflog files writable (`--bind`) so an agent cannot swap reflog dirs/files
//! with symlinks and race host git writes or deletes.

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

fn flag_pair_positions(argv: &[String], flag: &str, path: &str) -> Vec<usize> {
    argv.windows(3)
        .enumerate()
        .filter_map(|(idx, w)| (w[0] == flag && w[1] == path && w[2] == path).then_some(idx))
        .collect()
}

fn has_rw_bind_dest(argv: &[String], dest: &str) -> bool {
    argv.windows(3).any(|w| w[0] == "--bind" && w[2] == dest)
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

    assert!(common.join("logs/refs/heads/aaf/T1").is_file());
    assert!(common.join("logs/refs/heads/aaf/T2").is_file());
    assert!(common.join("worktrees/wt_a/logs/HEAD").is_file());
    assert!(common.join("worktrees/wt_b/logs/HEAD").is_file());

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
fn common_logs_is_read_only() {
    let fx = setup_fixture();
    let common_s = path_str(&fx.common);
    let common_logs_s = path_str(&fx.common.join("logs"));

    let common_binds = flag_pair_positions(&fx.argv, "--bind", &common_s);
    let last_common_bind = *common_binds
        .last()
        .expect("expected --bind rw for <common> git dir");

    let logs_ro = flag_pair_positions(&fx.argv, "--ro-bind", &common_logs_s)
        .last()
        .copied()
        .unwrap_or_else(|| {
            panic!(
                "expected --ro-bind for <common>/logs ({common_logs_s}) in bwrap argv: {:?}",
                fx.argv
            )
        });
    assert!(
        logs_ro > last_common_bind,
        "expected --ro-bind <common>/logs ({logs_ro}) after every --bind <common> ({last_common_bind})"
    );
}

#[test]
fn every_worktree_logs_is_read_only() {
    let fx = setup_fixture();
    for name in ["wt_a", "wt_b"] {
        let admin_s = path_str(&fx.common.join("worktrees").join(name));
        let admin_logs_s = path_str(&fx.common.join("worktrees").join(name).join("logs"));

        let admin_bind = flag_pair_positions(&fx.argv, "--bind", &admin_s)
            .last()
            .copied()
            .unwrap_or_else(|| {
                panic!(
                    "expected --bind rw for worktree admin dir {admin_s} in bwrap argv: {:?}",
                    fx.argv
                )
            });
        let logs_ro = flag_pair_positions(&fx.argv, "--ro-bind", &admin_logs_s)
            .last()
            .copied()
            .unwrap_or_else(|| {
                panic!(
                    "expected --ro-bind for {admin_logs_s} in bwrap argv: {:?}",
                    fx.argv
                )
            });
        assert!(
            logs_ro > admin_bind,
            "expected --ro-bind {admin_logs_s} ({logs_ro}) after --bind {admin_s} ({admin_bind})"
        );
    }
}

#[test]
fn own_reflog_files_stay_writable() {
    let fx = setup_fixture();
    let pairs = [
        (
            fx.common.join("logs"),
            fx.common.join("logs/refs/heads/aaf/T1"),
        ),
        (
            fx.common.join("worktrees/wt_a/logs"),
            fx.common.join("worktrees/wt_a/logs/HEAD"),
        ),
    ];

    for (logs_dir, reflog_file) in pairs {
        let logs_dir_s = path_str(&logs_dir);
        let reflog_file_s = path_str(&reflog_file);

        let reflog_bind = flag_pair_positions(&fx.argv, "--bind", &reflog_file_s)
            .last()
            .copied()
            .unwrap_or_else(|| {
                panic!(
                    "expected --bind rw for own reflog file {reflog_file_s} in bwrap argv: {:?}",
                    fx.argv
                )
            });
        let logs_ro = flag_pair_positions(&fx.argv, "--ro-bind", &logs_dir_s)
            .last()
            .copied()
            .unwrap_or_else(|| {
                panic!(
                    "expected --ro-bind for {logs_dir_s} before reopening {reflog_file_s}: {:?}",
                    fx.argv
                )
            });
        assert!(
            reflog_bind > logs_ro,
            "expected --bind {reflog_file_s} ({reflog_bind}) after --ro-bind {logs_dir_s} ({logs_ro})"
        );
    }
}

#[test]
fn sibling_reflog_file_not_reopened() {
    let fx = setup_fixture();
    for sibling in [
        fx.common.join("logs/refs/heads/aaf/T2"),
        fx.common.join("worktrees/wt_b/logs/HEAD"),
    ] {
        let sibling_s = path_str(&sibling);
        assert!(
            !has_rw_bind_dest(&fx.argv, &sibling_s),
            "sibling reflog file must NOT be bound rw: {sibling_s} in {:?}",
            fx.argv
        );
    }
}

#[test]
fn existing_git_protections_unchanged() {
    let fx = setup_fixture();
    let common_s = path_str(&fx.common);
    let last_common_bind = *flag_pair_positions(&fx.argv, "--bind", &common_s)
        .last()
        .expect("expected --bind rw for <common> git dir");

    let config = fx.common.join("config");
    assert!(config.is_file(), "fixture must contain <common>/config");
    let config_s = path_str(&config);
    let config_ro = flag_pair_positions(&fx.argv, "--ro-bind", &config_s)
        .last()
        .copied()
        .unwrap_or_else(|| panic!("expected --ro-bind for {config_s}: {:?}", fx.argv));
    assert!(config_ro > last_common_bind);

    let hooks = fx.common.join("hooks");
    if hooks.exists() {
        let hooks_s = path_str(&hooks);
        let hooks_ro = flag_pair_positions(&fx.argv, "--ro-bind", &hooks_s)
            .last()
            .copied()
            .unwrap_or_else(|| panic!("expected --ro-bind for {hooks_s}: {:?}", fx.argv));
        assert!(hooks_ro > last_common_bind);
    }
}
