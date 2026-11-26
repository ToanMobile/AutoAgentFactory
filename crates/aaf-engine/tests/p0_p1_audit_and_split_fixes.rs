#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

use aaf_domain::blocked::{BlockedReason, SubtaskPlan, UnblockAction, UnblockTier};
use aaf_domain::config::UnblockerConfig;
use aaf_domain::ports::{Clock, TaskStore};
use aaf_domain::task::{ImmutableSpec, Priority, Task};
use aaf_engine::db::Db;
use aaf_engine::factory::GitOps;
use aaf_engine::factory::RealGit;
use aaf_engine::gate::runner::is_heavy_with_light;
use aaf_engine::git::base_ref::BaseRef;
use aaf_engine::unblocker::{FailureSignal, Ladder, SubtaskProposer, classify};

struct FixedClock(i64);
impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

struct PanicProposer;
impl SubtaskProposer for PanicProposer {
    fn propose(
        &self,
        _task: &Task,
        _reason: &BlockedReason,
        _digest: &str,
    ) -> aaf_domain::ports::PortResult<Vec<SubtaskPlan>> {
        panic!("T4 proposer must not be invoked for AuditRejected");
    }
}

fn git_ok(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn audit_rejected_skips_t3_and_t4_directly_to_human_escalation() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    let clock = FixedClock(1_700_000_000);
    let task = Task::new_root(
        "t-audit-1",
        "/repo",
        "admin sale flow",
        ImmutableSpec {
            description: "desc".to_string(),
            acceptance: vec!["ok".to_string()],
        },
        Priority::P1,
        clock.now(),
    );
    db.insert_task(&task).unwrap();

    let mut signal = FailureSignal::empty();
    signal.audit_rejected = Some("effective score 87 (min 95); G6 scored 72".to_string());
    assert!(matches!(
        classify(&signal),
        BlockedReason::AuditRejected { .. }
    ));

    let cfg = UnblockerConfig::default();
    let ladder = Ladder::new(&db, &db, &db, &db, &clock, &cfg);
    let decision = ladder
        .next_action("t-audit-1", &signal, Some(&PanicProposer))
        .unwrap();
    assert_eq!(decision.tier, UnblockTier::T5Escalate);
    assert!(matches!(
        decision.reason,
        BlockedReason::AuditRejected { .. }
    ));
    assert!(matches!(
        decision.action,
        UnblockAction::EscalateHuman { .. }
    ));
}

#[test]
fn subtask_inherits_parent_scope_paths() {
    let mut parent = Task::new_root(
        "t-parent",
        "/repo",
        "parent",
        ImmutableSpec {
            description: "d".to_string(),
            acceptance: vec!["a".to_string()],
        },
        Priority::P1,
        10,
    );
    parent.scope_paths = vec![
        "tools/admin-keys.html".to_string(),
        "tests/scripts".to_string(),
    ];
    let child = Task::new_subtask(
        "t-parent-s1",
        &parent,
        "child",
        ImmutableSpec {
            description: "cd".to_string(),
            acceptance: vec!["ca".to_string()],
        },
        11,
    )
    .unwrap();
    assert_eq!(child.scope_paths, parent.scope_paths);
}

#[test]
fn light_commands_exempts_wrapper_script_but_never_direct_heavy_tool() {
    let light = vec!["test_scoped".to_string()];
    assert!(is_heavy_with_light(
        "test_scoped",
        "bash scripts/aaf_scoped_test.sh",
        &[]
    ));
    assert!(!is_heavy_with_light(
        "test_scoped",
        "bash scripts/aaf_scoped_test.sh",
        &light
    ));
    assert!(is_heavy_with_light(
        "test_scoped",
        "cd PhoneConnect && ./gradlew testDebugUnitTest",
        &light
    ));
    assert!(is_heavy_with_light(
        "build",
        "bash scripts/build.sh",
        &light
    ));
}

#[test]
fn real_git_detects_base_drift_and_cross_branch_contamination() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let wt_dir = tmp.path().join("worktrees");
    let locks = tmp.path().join("locks");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&locks).unwrap();

    git_ok(&repo, &["init", "-b", "main"]);
    git_ok(&repo, &["config", "user.email", "t@example.com"]);
    git_ok(&repo, &["config", "user.name", "Tester"]);
    std::fs::create_dir_all(repo.join("scripts")).unwrap();
    std::fs::write(
        repo.join("scripts/aaf_scoped_test.sh"),
        "#!/bin/sh\nexit 0\n",
    )
    .unwrap();
    std::fs::write(repo.join("README.md"), "init\n").unwrap();
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-m", "init"]);

    let git = RealGit::new(
        repo.clone(),
        wt_dir,
        locks,
        BaseRef {
            remote: None,
            branch: "main".to_string(),
        },
    );

    let wt_parent = git.task_worktree("t-parent").unwrap();
    std::fs::write(wt_parent.path.join("parent_only.txt"), "from parent\n").unwrap();
    git.commit_all(&wt_parent, "parent work").unwrap();

    let wt_child = git.task_worktree("t-child").unwrap();
    assert!(
        git.foreign_aaf_branch_ancestors(&wt_child)
            .unwrap()
            .is_empty()
    );
    assert!(
        git.base_changed_paths_since_merge_base(&wt_child)
            .unwrap()
            .is_empty()
    );

    // Base updates gate script -> drift check detects it.
    std::fs::write(
        repo.join("scripts/aaf_scoped_test.sh"),
        "#!/bin/sh\necho fast\n",
    )
    .unwrap();
    git_ok(&repo, &["add", "scripts/aaf_scoped_test.sh"]);
    git_ok(&repo, &["commit", "-m", "update scoped test"]);
    let drifted = git.base_changed_paths_since_merge_base(&wt_child).unwrap();
    assert_eq!(drifted, vec!["scripts/aaf_scoped_test.sh".to_string()]);

    // Child merges unmerged parent branch -> contamination guard catches it.
    git_ok(&wt_child.path, &["merge", "--ff-only", "aaf/t-parent"]);
    let foreign = git.foreign_aaf_branch_ancestors(&wt_child).unwrap();
    assert_eq!(foreign, vec!["aaf/t-parent".to_string()]);
}

#[test]
fn rebase_hydrates_newly_added_copy_files_from_base() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let wt_dir = tmp.path().join("worktrees");
    let locks = tmp.path().join("locks");
    std::fs::create_dir_all(repo.join("PhoneConnect")).unwrap();
    std::fs::create_dir_all(repo.join(".autofactory")).unwrap();
    std::fs::create_dir_all(&locks).unwrap();

    git_ok(&repo, &["init", "-b", "main"]);
    git_ok(&repo, &["config", "user.email", "t@example.com"]);
    git_ok(&repo, &["config", "user.name", "Tester"]);
    std::fs::write(repo.join(".gitignore"), "PhoneConnect/local.properties\n").unwrap();
    std::fs::write(
        repo.join(".autofactory/config.toml"),
        "[project]\nname = \"demo\"\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("PhoneConnect/local.properties"),
        "sdk.dir=/Volumes/Data/AndroidSDK\n",
    )
    .unwrap();
    git_ok(&repo, &["add", ".gitignore", ".autofactory/config.toml"]);
    git_ok(&repo, &["commit", "-m", "init"]);

    let git = RealGit::new(
        repo.clone(),
        wt_dir,
        locks,
        BaseRef {
            remote: None,
            branch: "main".to_string(),
        },
    );
    let wt = git.task_worktree("t-old").unwrap();
    assert!(!wt.path.join("PhoneConnect/local.properties").exists());

    // Base later adds [worktree].copy_files -> rebase brings local.properties into old worktree.
    std::fs::write(
        repo.join(".autofactory/config.toml"),
        "[project]\nname = \"demo\"\n\n[worktree]\ncopy_files = [\"PhoneConnect/local.properties\"]\n",
    )
    .unwrap();
    git_ok(&repo, &["add", ".autofactory/config.toml"]);
    git_ok(&repo, &["commit", "-m", "add copy_files"]);

    let conflicts = git.rebase(&wt).unwrap();
    assert!(conflicts.is_empty());
    assert_eq!(
        std::fs::read_to_string(wt.path.join("PhoneConnect/local.properties")).unwrap(),
        "sdk.dir=/Volumes/Data/AndroidSDK\n"
    );
}
