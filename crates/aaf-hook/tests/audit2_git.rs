#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Round-2 audit of `git_guard`: option values glued to their flag and git
//! configuration the hook cannot read, each next to the calls that must stay
//! allowed.

mod common;

use common::{Fixture, run_corpus};

const GIT: Option<&str> = Some("git_guard");
const DELETE: Option<&str> = Some("delete_guard");
const OPAQUE: Option<&str> = Some("unparsable_command");
const ALLOW: Option<&str> = None;

#[test]
fn a_value_glued_to_a_global_option_is_still_its_value() {
    let fixture = Fixture::new();
    fixture.write(".claude/settings.json", "{}");
    fixture.write(".claude/notes.txt", "x");
    run_corpus(
        &fixture,
        &[
            ("git -calias.f='push --force' f origin main", GIT),
            ("git -calias.f='push --force' status", GIT),
            ("git -cALIAS.r='reset --hard' r", GIT),
            ("git -calias.p=push p --force origin main", GIT),
            ("git -calias.p=push p origin :main", GIT),
            ("git -c alias.a=b -calias.b=push a -f origin main", GIT),
            ("git -calias.a=b -c alias.b=push a -f origin main", GIT),
            ("git -C/tmp/x -calias.r=reset r --hard", GIT),
            ("git '-calias.x=!git push --force' x", GIT),
            ("$GIT -calias.f='push --force' f origin main", GIT),
            // Already covered in glued form; must stay covered.
            ("git -ccore.hooksPath=/dev/null commit -m x", GIT),
            ("git -C/tmp/x push --force origin main", GIT),
            // `-C<dir>` moves the pathspecs of `git rm` like `-C <dir>`.
            ("git -C.claude rm settings.json", DELETE),
            // allowed
            ("git -calias.st=status st", ALLOW),
            ("git -calias.p=push p origin feature/x", ALLOW),
            ("git -calias.p=push p --dry-run --force origin main", ALLOW),
            ("git -cuser.name=x commit -m y", ALLOW),
            ("git -C/tmp/x status", ALLOW),
            ("git -C.claude rm notes.txt", ALLOW),
        ],
    );
}

#[test]
fn configuration_from_an_included_file_is_refused() {
    run_corpus(
        &Fixture::new(),
        &[
            (
                "git -c include.path=/tmp/evil.gitconfig push origin main",
                GIT,
            ),
            (
                "git -cinclude.path=/tmp/evil.gitconfig push origin main",
                GIT,
            ),
            ("git -c Include.Path=/tmp/evil.gitconfig f origin main", GIT),
            ("git -c includeIf.gitdir:/.path=/tmp/evil status", GIT),
            ("git -c 'includeIf.onbranch:main.path=/tmp/e' push", GIT),
            ("git --config-env=include.path=EVIL push origin main", GIT),
            ("git --config-env include.path=EVIL push origin main", GIT),
            ("git --config-env=includeIf.gitdir:/.path=EVIL push", GIT),
            (
                "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=include.path GIT_CONFIG_VALUE_0=/tmp/e git push",
                GIT,
            ),
            (
                "GIT_CONFIG_PARAMETERS=\"'include.path=/tmp/e'\" git push",
                GIT,
            ),
            // A key the hook cannot read could be any of the refused ones.
            (
                "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=$K GIT_CONFIG_VALUE_0=x git f",
                GIT,
            ),
            (
                "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=$(cat k) GIT_CONFIG_VALUE_0=x git f",
                GIT,
            ),
            ("GIT_CONFIG_PARAMETERS=$P git f origin main", GIT),
            ("git -c $KV push origin main", GIT),
            ("git -c \"$(cat kv)\" push origin main", GIT),
            ("git --config-env=$K=EVIL f origin main", GIT),
            // Stored for a later command.
            ("git config include.path /tmp/evil.gitconfig", GIT),
            ("git config --global --add include.path /tmp/e", GIT),
            ("git config set includeIf.gitdir:/.path /tmp/e", GIT),
            ("git -c include.path=/tmp/e push origin main 'oops", OPAQUE),
            // allowed
            ("git -c user.name=x -c user.email=y@z commit -m m", ALLOW),
            ("git -c core.pager=cat log -n 3", ALLOW),
            ("git -c user.name=$NAME commit -m m", ALLOW),
            ("git -c color.ui=false status", ALLOW),
            ("git --config-env=http.extraheader=AUTH fetch", ALLOW),
            (
                "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=user.name GIT_CONFIG_VALUE_0=x git commit -m m",
                ALLOW,
            ),
            ("git config include.path", ALLOW),
            ("git config --get include.path", ALLOW),
            ("git log --grep include.path", ALLOW),
            // Not git: `-c` is some other program's option.
            ("$SHELL -c \"$CMD\"", ALLOW),
        ],
    );
}

#[test]
fn a_redirected_config_file_is_refused() {
    run_corpus(
        &Fixture::new(),
        &[
            (
                "GIT_CONFIG_GLOBAL=/tmp/evil.gitconfig git push origin main",
                GIT,
            ),
            (
                "GIT_CONFIG_SYSTEM=/tmp/evil.gitconfig git push origin main",
                GIT,
            ),
            ("GIT_CONFIG=/tmp/evil.gitconfig git config alias.f", GIT),
            ("GIT_CONFIG_GLOBAL=$EVIL git f origin main", GIT),
            ("GIT_CONFIG_GLOBAL=$(mktemp) git f origin main", GIT),
            (
                "env GIT_CONFIG_GLOBAL=/tmp/evil.gitconfig git push origin main",
                GIT,
            ),
            (
                "env -i GIT_CONFIG_SYSTEM=/tmp/evil.gitconfig git push origin main",
                GIT,
            ),
            (
                "/usr/bin/env GIT_CONFIG_GLOBAL=/tmp/evil.gitconfig git f",
                GIT,
            ),
            (
                "export GIT_CONFIG_GLOBAL=/tmp/evil.gitconfig; git push origin main",
                GIT,
            ),
            (
                "export GIT_CONFIG_SYSTEM=/tmp/evil.gitconfig && git f origin main",
                GIT,
            ),
            (
                "GIT_CONFIG_GLOBAL=/tmp/evil.gitconfig; export GIT_CONFIG_GLOBAL; git f",
                GIT,
            ),
            (
                "declare -x GIT_CONFIG_GLOBAL=/tmp/evil.gitconfig; git f",
                GIT,
            ),
            (
                "sh -c 'GIT_CONFIG_GLOBAL=/tmp/evil.gitconfig git f origin main'",
                GIT,
            ),
            // `~/.gitconfig` and `$XDG_CONFIG_HOME/git/config` move with these.
            ("HOME=/tmp/evil git f origin main", GIT),
            ("XDG_CONFIG_HOME=/tmp/evil git push origin main", GIT),
            ("env HOME=/tmp/evil git push origin main", GIT),
            // allowed
            ("GIT_CONFIG_GLOBAL=/dev/null git status", ALLOW),
            (
                "GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null git log -n 1",
                ALLOW,
            ),
            ("GIT_CONFIG_NOSYSTEM=1 git status", ALLOW),
            ("GIT_AUTHOR_NAME=x git commit -m m", ALLOW),
            ("GIT_TERMINAL_PROMPT=0 git fetch origin", ALLOW),
            ("HOME=/tmp/h npm test", ALLOW),
            ("echo GIT_CONFIG_GLOBAL=/tmp/x", ALLOW),
        ],
    );
}
