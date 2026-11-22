#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests from audit round 4 (lane/audit4-fix).

mod common;

use common::{Fixture, run_corpus, run_file_corpus};

const ALLOW: Option<&str> = None;
const HUMAN: Option<&str> = Some("human_action_guard");
const GIT: Option<&str> = Some("git_guard");
const HEAVY: Option<&str> = Some("heavy_cmd_guard");

#[test]
fn env_clearing_wrapper_around_nested_shell_still_blocks_aaf() {
    let f = Fixture::new();
    run_corpus(
        &f,
        &[
            ("env -i sh -c 'aaf run'", HUMAN),
            ("env -i bash -c 'aaf run --unsandboxed'", HUMAN),
            ("env - sh -c 'aaf run'", HUMAN),
            ("exec -c sh -c 'aaf run'", HUMAN),
            ("sudo sh -c 'aaf run'", HUMAN),
            ("env -i /bin/sh -c 'aaf gate'", HUMAN),
            ("env -i eval 'aaf run'", HUMAN),
            ("git -c alias.f='!env -i sh -c \"aaf run\"' f", GIT),
            ("aaf run", ALLOW),
            ("env -i sh -c 'true'", ALLOW),
        ],
    );
}

#[test]
fn git_config_double_dash_and_abbreviations_still_block_program_keys() {
    let f = Fixture::new();
    run_corpus(
        &f,
        &[
            ("git config -- core.fsmonitor /tmp/evil --list", GIT),
            ("git config -- core.fsmonitor /tmp/evil -l", GIT),
            ("git config -- core.fsmonitor /tmp/evil --get", GIT),
            ("git config -- core.fsmonitor /tmp/evil --unset", GIT),
            ("git config --add -- core.fsmonitor /tmp/evil --list", GIT),
            ("git config --ed", GIT),
            ("git config --rename-sec foo core", GIT),
            // Plain reads and unsets before `--` stay allowed.
            ("git config --list", ALLOW),
            ("git config --get core.fsmonitor", ALLOW),
            ("git config --unset core.fsmonitor", ALLOW),
            ("git config core.fsmonitor false", ALLOW),
        ],
    );
}

#[test]
fn glued_env_p_and_script_c_do_not_bypass_guards() {
    let f = Fixture::new();
    run_corpus(
        &f,
        &[
            ("env -P/tmp/evil aaf slot -- ./gradlew build", HEAVY),
            ("env -iP /tmp/evil aaf slot -- ./gradlew build", HEAVY),
            ("env -iP/tmp/evil aaf slot -- ./gradlew build", HEAVY),
            ("env -P/tmp/evilS aaf slot -- ./gradlew build", HEAVY),
            ("script -c'./gradlew build' /dev/null", HEAVY),
            ("script -qc'./gradlew build' /dev/null", HEAVY),
            // Plain slotted build stays allowed.
            ("aaf slot -- ./gradlew build", ALLOW),
        ],
    );
}

#[test]
fn case_variants_of_protected_branches_are_still_protected() {
    let f = Fixture::new();
    run_corpus(
        &f,
        &[
            ("git branch -D Main", GIT),
            ("git branch -d MAIN", GIT),
            ("git branch -D Release/1.0", GIT),
            ("git push origin :Main", GIT),
            ("git push --delete origin Master", GIT),
            ("git push --force-with-lease origin Main", GIT),
            ("git push --force-with-lease origin Release/2.0", GIT),
            ("git update-ref -d refs/heads/Main", GIT),
            // Feature branch remains allowed.
            ("git branch -D feature/Main", ALLOW),
            ("git push --force-with-lease origin feature/x", ALLOW),
        ],
    );
}

#[test]
fn worktree_admin_commondir_and_gitdir_files_are_protected() {
    let f = Fixture::new();
    run_corpus(
        &f,
        &[
            ("echo /tmp/evil > .git/worktrees/w/commondir", GIT),
            ("echo /tmp/evil > .git/worktrees/w/gitdir", GIT),
            ("cp /tmp/evil .git/worktrees/w/commondir", GIT),
            ("cp /tmp/evil .git/modules/sub/commondir", GIT),
            // Reading them remains allowed.
            ("cat .git/worktrees/w/commondir", ALLOW),
            ("cat .git/worktrees/w/gitdir", ALLOW),
        ],
    );
    run_file_corpus(
        &f,
        &[
            ("Write", ".git/worktrees/w/commondir", GIT),
            ("Edit", ".git/worktrees/w/gitdir", GIT),
            ("Read", ".git/worktrees/w/commondir", ALLOW),
        ],
    );
}
