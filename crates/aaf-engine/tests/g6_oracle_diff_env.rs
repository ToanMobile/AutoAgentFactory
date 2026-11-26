#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests verifying that the G6 red->green oracle injects
//! `AAF_CHANGED_FILES_FILE` / `AAF_DELETED_FILES_FILE` into diff-aware
//! `test_scoped` scripts and filters removed tests on the base control pass.

use std::fs;
use std::path::Path;
use std::process::Command;

use aaf_domain::config::MachineConfig;
use aaf_domain::gate::{GateMode, GateStatus};
use aaf_domain::ports::Clock;
use aaf_engine::config::{ConfigInputs, ConfigOverlay, REPO_CONFIG_REL, load};
use aaf_engine::gate::run_gate_throttled;
use tempfile::TempDir;

#[path = "throttle_support.rs"]
mod throttle_support;
use throttle_support::calm_throttle;

struct FixedClock(i64);
impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

fn git_out(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn oracle_injects_changed_files_env_for_diff_aware_scoped_runner_and_filters_control() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let data = TempDir::new().unwrap();
    git_out(repo, &["init", "-b", "main"]);
    fs::create_dir_all(repo.join(".autofactory")).unwrap();
    fs::create_dir_all(repo.join("tools")).unwrap();
    fs::create_dir_all(repo.join("tests/scripts")).unwrap();

    fs::write(repo.join("tools/feature.sh"), "echo old\n").unwrap();
    fs::write(repo.join("tests/scripts/test-base.sh"), "exit 0\n").unwrap();
    // Diff-aware scoped test script (mirrors GeelyEx2/scripts/aaf_scoped_test.sh):
    // - If AAF_CHANGED_FILES_FILE is missing/empty, falls back to an unrelated
    //   passing check (exit 0) — which used to trigger false-positive vacuous test.
    // - When AAF_CHANGED_FILES_FILE lists `tests/scripts/test-*.sh`, runs each
    //   listed script directly (failing if a deleted file were still listed!).
    fs::write(
        repo.join("scoped.sh"),
        r#"#!/bin/sh
set -eu
if [ -z "${AAF_CHANGED_FILES_FILE:-}" ] || [ ! -r "$AAF_CHANGED_FILES_FILE" ]; then
  echo "fallback: unrelated passing suite"
  exit 0
fi
ran=0
while IFS= read -r p || [ -n "$p" ]; do
  case "$p" in
    tests/scripts/test-*.sh)
      sh "$p"
      ran=1
      ;;
    tools/*)
      sh tests/scripts/test-base.sh
      ran=1
      ;;
  esac
done < "$AAF_CHANGED_FILES_FILE"
if [ "$ran" -eq 0 ]; then
  echo "fallback: nothing mapped"
fi
exit 0
"#,
    )
    .unwrap();
    fs::write(
        repo.join(REPO_CONFIG_REL),
        "[project]\nname = \"scoped-oracle\"\nprofile = \"backend\"\n\n\
         [commands]\nbuild = \"true\"\ntest_scoped = \"sh scoped.sh\"\n\
         test_full = \"sh scoped.sh\"\nlint = \"true\"\n\n\
         [gate]\nbase_ref = \"main\"\nred_green_oracle = \"block\"\n",
    )
    .unwrap();
    git_out(repo, &["add", "."]);
    git_out(repo, &["commit", "-m", "base"]);

    // Feature branch: fix tools/feature.sh + add a new test script proving it.
    git_out(repo, &["checkout", "-b", "feat/fix"]);
    fs::write(repo.join("tools/feature.sh"), "echo fixed\n").unwrap();
    fs::write(
        repo.join("tests/scripts/test-new-fix.sh"),
        "out=$(sh tools/feature.sh)\n[ \"$out\" = \"fixed\" ] || { echo \"FAIL: got $out\"; exit 1; }\n",
    )
    .unwrap();
    git_out(repo, &["add", "."]);
    git_out(repo, &["commit", "-m", "fix feature with new test"]);

    let no_env = |_: &str| None;
    let eff = load(&ConfigInputs {
        global_config: None,
        repo_root: repo.to_path_buf(),
        env: &no_env,
        cli: ConfigOverlay::default(),
    })
    .unwrap();
    let machine = MachineConfig::default();
    let throttle = calm_throttle(&machine);
    let receipt = run_gate_throttled(
        data.path(),
        repo,
        &eff.repo,
        GateMode::Scoped,
        &FixedClock(1_000_000),
        &throttle,
    )
    .unwrap();

    assert_eq!(
        receipt.status,
        GateStatus::Clear,
        "diff-aware test_scoped must prove red->green and pass control, got reasons: {:?}",
        receipt.reasons
    );
    assert!(
        receipt
            .reasons
            .iter()
            .any(|r| r.contains("oracle: red_green=proven")),
        "expected proven oracle verdict, got: {:?}",
        receipt.reasons
    );
    assert!(
        !receipt
            .reasons
            .iter()
            .any(|r| r.to_ascii_lowercase().contains("vacuous")),
        "must not emit false-positive vacuous warning: {:?}",
        receipt.reasons
    );
}
