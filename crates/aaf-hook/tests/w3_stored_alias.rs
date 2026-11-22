#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Wave 3: a git subcommand that is not a builtin is looked up as an alias
//! in the stored configuration. In-process, with the lookup faked.

mod common;

use common::{Fixture, rule, run_corpus};

const GIT: Option<&str> = Some("git_guard");

#[test]
fn stored_aliases_are_expanded_and_judged() {
    let fixture = Fixture::new();
    fixture.aliases(&[
        ("fp", "push --force"),
        ("st", "status --short"),
        ("lease", "push --force-with-lease"),
        ("nv", "commit --no-verify"),
        ("sh", "!f() { git push \"$@\"; }; f"),
        ("loop", "loop"),
        ("first", "status"),
        ("first", "reset --hard"),
    ]);
    run_corpus(
        &fixture,
        &[
            ("git fp", GIT),
            ("git fp origin feature/x", GIT),
            ("/usr/bin/git fp", GIT),
            ("env git fp", GIT),
            ("git st", None),
            // The arguments belong to the expansion.
            ("git lease origin feature/x", None),
            ("git lease origin main", GIT),
            ("git nv -m msg", GIT),
            ("git sh origin feature/x", None),
            ("git sh --force origin feature/x", GIT),
            // An alias that never ends cannot be analysed.
            ("git loop", GIT),
            // The last definition is the one git uses.
            ("git first", GIT),
            ("git unknown-thing", None),
            // An alias defined on the command line still wins the name.
            ("git -c alias.fp=status fp", None),
        ],
    );
}

#[test]
fn a_failed_lookup_refuses_the_unknown_subcommand_only() {
    let fixture = Fixture::new();
    *fixture.git_config.borrow_mut() = None;
    run_corpus(
        &fixture,
        &[
            ("git fp origin main", GIT),
            ("git lfs pull", GIT),
            ("git status", None),
            ("git push origin feature/x", None),
            ("git commit -m msg", None),
        ],
    );
}

#[test]
fn a_repository_the_hook_does_not_look_in_is_refused() {
    let fixture = Fixture::new();
    run_corpus(
        &fixture,
        &[
            ("git --git-dir=/elsewhere/.git fp", GIT),
            ("GIT_DIR=/elsewhere/.git git fp", GIT),
            ("git --git-dir=/elsewhere/.git status", None),
            ("GIT_DIR=/elsewhere/.git git log", None),
        ],
    );
}

#[test]
fn builtin_subcommands_never_read_the_configuration() {
    let fixture = Fixture::new();
    for command in [
        "git status --short",
        "git push origin feature/x",
        "git -C sub commit -m msg",
        "git log --oneline -n 5 && git diff --stat",
        "cargo test && ls -la",
    ] {
        assert_eq!(rule(&fixture.bash(command)), None, "{command}");
    }
    assert_eq!(fixture.config_lookups.get(), 0);

    assert_eq!(rule(&fixture.bash("git st")), None);
    assert_eq!(fixture.config_lookups.get(), 1, "one lookup per alias");
}
