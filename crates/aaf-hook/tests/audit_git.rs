#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Bypasses an independent audit found in `git_guard`, each next to the
//! calls that must stay allowed.

mod common;

use common::{Fixture, run_corpus};

const GIT: Option<&str> = Some("git_guard");
const ALLOW: Option<&str> = None;

#[test]
fn the_last_dry_run_flag_wins() {
    run_corpus(
        &Fixture::new(),
        &[
            ("git push --dry-run --no-dry-run --force origin main", GIT),
            ("git push -n --no-dry-run -f origin main", GIT),
            ("git push --dry-run --no-dry --force origin main", GIT),
            ("git push --dry-run --no-dry-run origin :main", GIT),
            // allowed
            ("git push --no-dry-run --dry-run --force origin main", ALLOW),
            ("git push --no-dry-run -n -f origin main", ALLOW),
            ("git push --no-dry-run origin feature/x", ALLOW),
        ],
    );
}

#[test]
fn an_option_value_is_never_read_as_a_flag() {
    run_corpus(
        &Fixture::new(),
        &[
            // `--dry-run` is the push option's value here, not a flag.
            ("git push --push-option --dry-run --force origin main", GIT),
            ("git push --push-opt --dry-run --force origin main", GIT),
            ("git push --receive-p --dry-run -f origin main", GIT),
            ("git push --exec --dry-run -f origin main", GIT),
            // A global option's value is not the subcommand.
            ("git --attr-source HEAD push --force origin main", GIT),
            ("git --attr-source HEAD reset --hard", GIT),
            // allowed
            ("git push --push-option=ci.skip origin feature/x", ALLOW),
            ("git push --push-option ci.skip origin feature/x", ALLOW),
            ("git --attr-source HEAD status", ALLOW),
        ],
    );
}

#[test]
fn with_repo_every_positional_is_a_refspec() {
    run_corpus(
        &Fixture::new(),
        &[
            ("git push --repo=origin +HEAD:main", GIT),
            ("git push --repo origin :main", GIT),
            ("git push --repo=origin :refs/heads/master", GIT),
            ("git push --repo origin --delete release/1.0", GIT),
            ("git push --repo=origin --force-with-lease main", GIT),
            // allowed
            ("git push --repo=origin feature/x", ALLOW),
            ("git push --repo origin HEAD:feature/x", ALLOW),
            ("git push --repo=origin :feature/old", ALLOW),
            ("git push --repo=origin --force-with-lease feature/x", ALLOW),
        ],
    );
}

#[test]
fn an_alias_cannot_smuggle_a_denied_command() {
    let fixture = Fixture::new();
    fixture.write(".env", "x");
    run_corpus(
        &fixture,
        &[
            ("git -c alias.f='push --force' f origin main", GIT),
            // Defined is enough: a later word could invoke it.
            ("git -c alias.f='push --force' status", GIT),
            ("git -c ALIAS.f='reset --hard' f", GIT),
            ("git -c alias.c='commit --no-verify' c -m x", GIT),
            // The dangerous part arrives as an argument of the alias.
            ("git -c alias.p=push p --force origin main", GIT),
            ("git -c alias.p=push p origin :main", GIT),
            ("git -c alias.a=b -c alias.b=push a -f origin main", GIT),
            ("git -C /tmp/x -c alias.r=reset r --hard", GIT),
            // Shell aliases go through the shell analyser.
            ("git -c 'alias.x=!git push --force' x", GIT),
            ("git -c 'alias.x=!rm .env' x", GIT),
            ("git -c 'alias.x=!git push' x --force origin main", GIT),
            (
                "git -c 'alias.p=!f() { git push \"$@\"; }; f' p --force origin main",
                GIT,
            ),
            // What cannot be analysed is refused.
            ("git -c alias.f='push \"--force' f", GIT),
            ("git -c alias.f='$CMD --force' f", GIT),
            ("git --config-env=alias.f=EVIL f origin main", GIT),
            ("git --config-env alias.f=EVIL f origin main", GIT),
            (
                "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=alias.f GIT_CONFIG_VALUE_0='push --force' git f",
                GIT,
            ),
            ("$GIT -c alias.f='push --force' f origin main", GIT),
            // Stored aliases.
            ("git config alias.f 'push --force'", GIT),
            ("git config --global alias.x '!git reset --hard'", GIT),
            ("git config set alias.f 'push -f'", GIT),
            ("git config --add alias.f 'branch -D main'", GIT),
            (
                "git config alias.f 'push --force' && git f origin main",
                GIT,
            ),
            ("git config alias.f 'push --force'; git f origin main", GIT),
            // allowed
            ("git -c alias.st=status st", ALLOW),
            ("git -c alias.l='log --oneline -n 5' l", ALLOW),
            ("git -c alias.p=push p origin feature/x", ALLOW),
            ("git -c alias.p=push p --dry-run --force origin main", ALLOW),
            ("git -c 'alias.x=!ls -la' x", ALLOW),
            ("git -c 'alias.x=!git status' x --short", ALLOW),
            ("git config alias.st status", ALLOW),
            ("git config alias.lg 'log --graph --oneline'", ALLOW),
            ("git config alias.f", ALLOW),
            ("git config --get alias.f", ALLOW),
            ("git config --unset alias.f", ALLOW),
            ("git -c user.name=alias.f commit -m x", ALLOW),
        ],
    );
}

#[test]
fn a_double_dash_does_not_hide_the_branch() {
    run_corpus(
        &Fixture::new(),
        &[
            ("git branch -D -- main", GIT),
            ("git branch --delete -- release/1.0", GIT),
            ("git update-ref -d -- refs/heads/master", GIT),
            // allowed
            ("git branch -D -- feature/old", ALLOW),
            ("git update-ref -d -- refs/heads/feature/old", ALLOW),
        ],
    );
}

#[test]
fn git_clean_follows_the_last_dry_run_flag() {
    run_corpus(
        &Fixture::new(),
        &[
            ("git clean -n -x --no-dry-run -f", Some("delete_guard")),
            (
                "git clean --dry-run --no-dry-run -fdx",
                Some("delete_guard"),
            ),
            // allowed
            ("git clean --no-dry-run -n -x", ALLOW),
            ("git clean -fdx --dry-run", ALLOW),
        ],
    );
}

#[test]
fn git_merge_and_cherry_pick_from_aaf_task_branches_are_denied() {
    run_corpus(
        &Fixture::new(),
        &[
            ("git merge --ff-only aaf/t-6ae3e474", GIT),
            ("git merge refs/heads/aaf/t-6ae3e474-s1", GIT),
            ("git merge origin/aaf/t-6ae3e474", GIT),
            ("git cherry-pick aaf/t-6ae3e474", GIT),
            ("git cherry-pick refs/remotes/origin/aaf/t-6ae3e474-s1", GIT),
            ("git cherry-pick main..aaf/t-6ae3e474", GIT),
            ("git cherry-pick main...aaf/t-6ae3e474", GIT),
            // allowed
            ("git merge main", ALLOW),
            ("git merge origin/main", ALLOW),
        ],
    );
}
