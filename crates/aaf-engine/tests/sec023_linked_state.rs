#![allow(clippy::unwrap_used, clippy::expect_used)]
//! SEC-023 edges: the main checkout's `HEAD`/`index` are closed only when the
//! run's worktree is a linked worktree of the git dir (or below one), never
//! for a normal checkout, a subdirectory of one, or a `--separate-git-dir`.

use std::path::{Path, PathBuf};

use aaf_domain::ports::SandboxPolicy;
use aaf_engine::executors::sandbox::{SandboxPaths, seatbelt_profile};

const ID: &str =
    "-c user.name=t -c user.email=t@e.invalid -c commit.gpgsign=false -c core.hooksPath=/dev/null";
/// Part of the rule that closes the main checkout's state.
const STATE_RULE: &str = "/([^/]*HEAD|index|AUTO_MERGE|";

fn git(repo: &Path, args: &[&str]) -> String {
    let mut full: Vec<&str> = ID.split(' ').collect();
    full.extend_from_slice(args);
    aaf_fp::git(repo, &full, &[], aaf_fp::GIT_TIMEOUT).unwrap()
}

/// `<root>/main` (one commit, `sub/a.txt`) and its linked worktree `<root>/wt`.
fn linked(root: &Path, init: &[&str]) -> (PathBuf, PathBuf) {
    let (main, wt) = (root.join("main"), root.join("wt"));
    std::fs::create_dir_all(main.join("sub")).unwrap();
    git(&main, init);
    std::fs::write(main.join("sub/a.txt"), "a").unwrap();
    git(&main, &["add", "sub/a.txt"]);
    git(&main, &["commit", "-q", "-m", "first"]);
    let w = wt.to_str().unwrap();
    git(&main, &["worktree", "add", "-q", "-b", "t", w]);
    (main, wt)
}

fn has_state_rule(cwd: &Path) -> bool {
    let args = ["rev-parse", "--path-format=absolute", "--git-common-dir"];
    let git_dir = git(cwd, &args);
    let extra = SandboxPaths {
        worktree: Some(cwd.to_string_lossy().into_owned()),
        git_dirs: vec![git_dir],
        ..SandboxPaths::default()
    };
    seatbelt_profile(&SandboxPolicy::default(), &extra)
        .unwrap()
        .contains(STATE_RULE)
}

#[test]
fn state_rule_only_for_linked_worktrees_and_their_subdirectories() {
    let dir = tempfile::tempdir().unwrap();
    let (main, wt) = linked(&dir.path().canonicalize().unwrap(), &["init", "-q"]);
    assert!(has_state_rule(&wt) && has_state_rule(&wt.join("sub")));
    assert!(!has_state_rule(&main) && !has_state_rule(&main.join("sub")));
}

#[test]
fn separate_git_dir_checkout_gets_no_state_rule() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let store = root.join("store.git");
    let init = ["init", "-q", "--separate-git-dir", store.to_str().unwrap()];
    let (main, wt) = linked(&root, &init);
    assert!(main.join(".git").is_file());
    assert!(!has_state_rule(&main) && has_state_rule(&wt));
}

/// Under Seatbelt, a subdirectory of the normal checkout still commits and
/// one of the linked worktree still cannot move the main checkout's `HEAD`.
#[cfg(target_os = "macos")]
#[test]
fn subdirectories_keep_the_right_side_of_the_rule() {
    use aaf_domain::ports::{AgentRequest, AgentRunner};
    use aaf_engine::executors::{ExecEnv, generic::GenericRunner};
    if !Path::new("/usr/bin/sandbox-exec").exists() {
        return;
    }
    let run = |cwd: &Path, script: &str| {
        let template = ["/bin/sh", "-c", script, "sh", "{prompt}"].map(String::from);
        let runner = GenericRunner::new(template.to_vec(), ExecEnv::default()).unwrap();
        let request = AgentRequest {
            run_id: format!("sec023-edge-{}", uuid::Uuid::new_v4()),
            prompt: "unused".to_string(),
            cwd: cwd.to_string_lossy().into_owned(),
            account: None,
            timeout_secs: 60,
            sandbox: SandboxPolicy {
                enabled: true,
                ..SandboxPolicy::default()
            },
            json_schema: None,
            max_turns: None,
        };
        runner.run(&request).unwrap().log_tail
    };
    let dir = tempfile::tempdir().unwrap();
    let (main, wt) = linked(&dir.path().canonicalize().unwrap(), &["init", "-q"]);
    let script = format!("echo b > b.txt && git add b.txt && git {ID} commit -q -m p && echo ok");
    let log = run(&main.join("sub"), &script);
    assert!(log.contains("ok"), "{log}");
    assert_eq!(git(&main, &["log", "-1", "--format=%s"]), "p");

    let head = main.join(".git/HEAD");
    let before = std::fs::read(&head).unwrap();
    let evil = format!("(echo x > '{}') 2>/dev/null || echo denied", head.display());
    let log = run(&wt.join("sub"), &evil);
    assert!(log.contains("denied"), "{log}");
    assert_eq!(std::fs::read(&head).unwrap(), before);
}
