//! F4-A: the rebase before a gate and the merges of an integration tree run
//! no repository hook. A hook committed in the tree (repo-relative
//! `core.hooksPath`) is agent-controlled: rewriting a locked test file back
//! to its base text in the working tree would let the gate judge a tree
//! other than the commit that lands.

use std::fs;

use super::git_real::{Fixture, git};
use crate::factory::ports::GitOps;
use crate::throttle::tests::exec_support::write_executable;

const EDITED_TEST: &str = "// checks weakened\n";

/// Commits executable `.githooks/<name>` hooks on main that put the locked
/// test file back to its base text, and points `core.hooksPath` at them.
fn hooked_fixture(names: &[&str]) -> Fixture {
    let fx = Fixture::new();
    let hooks = fx.repo.join(".githooks");
    fs::create_dir_all(&hooks).unwrap();
    for name in names {
        let hook = hooks.join(name);
        write_executable(
            &hook,
            "#!/bin/sh\nprintf '// checks\\n' > tests/app_test.rs\n",
        );
    }
    git(&fx.repo, &["add", "-A"]);
    git(&fx.repo, &["commit", "--quiet", "-m", "hooks"]);
    git(&fx.repo, &["config", "core.hooksPath", ".githooks"]);
    fx
}

/// Moves main on with an unrelated commit, so a rebase rewrites the branch.
fn move_base(fx: &Fixture) {
    fs::write(fx.repo.join("other.txt"), "base moved\n").unwrap();
    git(&fx.repo, &["add", "-A"]);
    git(&fx.repo, &["commit", "--quiet", "-m", "base moves"]);
}

#[test]
fn the_rebase_before_a_gate_runs_no_hook_of_the_tree() {
    let fx = hooked_fixture(&["post-rewrite", "post-checkout"]);
    let tree = fx.task("t1", "tests/app_test.rs", EDITED_TEST);
    move_base(&fx);

    // With no hook running, the rebase of an unrelated change is clean.
    let conflicts = fx.ops.rebase(&tree).unwrap();

    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert_eq!(git(&tree.path, &["status", "--porcelain"]), "");
    // What the gate reads is what the branch commits.
    assert_eq!(
        fs::read_to_string(tree.path.join("tests/app_test.rs")).unwrap(),
        EDITED_TEST
    );
    assert_eq!(
        git(&tree.path, &["show", "HEAD:tests/app_test.rs"]),
        EDITED_TEST.trim_end()
    );
}

#[test]
fn a_tree_left_dirty_by_the_rebase_is_not_handed_to_the_gate() {
    let fx = Fixture::new();
    let tree = fx.task("t1", "app.txt", "two\n");
    move_base(&fx);
    // Something the rebase leaves in the working tree and the branch does
    // not commit.
    fs::write(tree.path.join("stray.txt"), "not committed\n").unwrap();

    let error = fx.ops.rebase(&tree).unwrap_err();

    assert!(error.to_string().contains("uncommitted changes"), "{error}");
}

#[test]
fn the_merges_of_an_integration_tree_run_no_hook_of_the_tree() {
    let fx = hooked_fixture(&["post-merge", "post-checkout"]);
    fx.task("t1", "tests/app_test.rs", EDITED_TEST);

    let integration = fx.ops.integrate(&["t1".to_string()]).unwrap();

    assert_eq!(integration.merged, vec!["t1"]);
    let path = &integration.tree.path;
    assert_eq!(git(path, &["status", "--porcelain"]), "");
    assert_eq!(
        fs::read_to_string(path.join("tests/app_test.rs")).unwrap(),
        EDITED_TEST
    );
    fx.ops.discard(&integration).unwrap();
}

/// R3-L5-2: `commit_all` runs the repository's commit hooks. A post-commit
/// hook that puts the locked test back in the working tree after the
/// weakened one was committed must not hand the gate a tree other than the
/// commit: what the hook wrote is committed too, so the gate judges HEAD.
#[test]
fn a_tree_left_dirty_by_a_commit_hook_is_not_handed_to_the_gate() {
    let fx = hooked_fixture(&["post-commit"]);
    let tree = fx.ops.task_worktree("t1").unwrap();
    fs::write(tree.path.join("tests/app_test.rs"), EDITED_TEST).unwrap();

    fx.ops.commit_all(&tree, "aaf: t1").unwrap();

    assert_eq!(git(&tree.path, &["status", "--porcelain"]), "");
    assert_eq!(
        git(&tree.path, &["show", "HEAD:tests/app_test.rs"]),
        fs::read_to_string(tree.path.join("tests/app_test.rs"))
            .unwrap()
            .trim_end()
    );
    assert_eq!(git(&tree.path, &["rev-list", "--count", "main..HEAD"]), "2");
}
