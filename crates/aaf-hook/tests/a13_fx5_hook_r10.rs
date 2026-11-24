#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for audit-13 round-10 hook findings: a pipe consumer on
//! the line after the heredoc body keeps its directory wrapper (R10-H6-2),
//! a `find -delete` filtered by name asks git nothing (O11-R10-1), and the
//! deny texts an unattended agent reads (O12-R10-5).

mod common;

use std::fs;

use aaf_hook::Decision;
use common::{Fixture, run_corpus};

const ALLOW: Option<&str> = None;
const DELETE: Option<&str> = Some("delete_guard");

fn fixture() -> Fixture {
    let fx = Fixture::new();
    fs::create_dir_all(fx.root.join("src")).unwrap();
    fs::create_dir_all(fx.root.join(".git/hooks")).unwrap();
    fx
}

/// R10-H6-2: `cat <<'EOF' |`, the body, then the consumer on a line of its
/// own: the body runs where the consumer's wrapper moves it, as when the
/// consumer stands on the `|` line.
#[test]
fn r10_h6_2_next_line_consumer_keeps_its_directory_wrapper() {
    run_corpus(
        &fixture(),
        &[
            ("cat <<'EOF' |\nrm -rf cdp\nEOF\nenv -C .. sh", DELETE),
            ("cat <<'EOF' |\nrm -rf cdp\nEOF\nsudo -D .. sh", DELETE),
            (
                "cat <<'EOF' |\nrm -rf cdp\nEOF\nenv --chdir=.. bash -s",
                DELETE,
            ),
            ("cat <<'EOF' |\nrm -rf cdp\nEOF\nenv -C /tmp/.. sh", DELETE),
            // The same consumers on the `|` line.
            ("cat <<'EOF' | env -C .. sh\nrm -rf cdp\nEOF\n", DELETE),
            ("cat <<'EOF' | sudo -D .. sh\nrm -rf cdp\nEOF\n", DELETE),
            // Inside the repository, or no wrapper: allowed as before.
            ("cat <<'EOF' |\nrm -rf cdp\nEOF\nenv -C src sh", ALLOW),
            ("cat <<'EOF' |\nrm -rf cdp\nEOF\nsh", ALLOW),
            ("cat <<'EOF' |\nrm -rf cdp\nEOF\nenv -C .. cat", ALLOW),
        ],
    );
}

/// O11-R10-1: a `find -delete` whose `-name` rules every protected file out
/// is decided without asking git for the repository root; an unfiltered one
/// is still denied.
#[test]
fn o11_r10_1_filtered_find_delete_asks_git_nothing() {
    let fx = fixture();
    for command in [
        "find . -name '*.pyc' -delete",
        "find src -type f -name '*.o' -delete",
    ] {
        fx.root_lookups.set(0);
        assert_eq!(common::rule(&fx.bash(command)), ALLOW, "{command}");
        assert_eq!(fx.root_lookups.get(), 0, "{command}: repo root looked up");
    }
    assert_eq!(common::rule(&fx.bash("find . -delete")), DELETE);
    assert_eq!(
        common::rule(&fx.bash("find . -name '.git' -o -name x -delete")),
        DELETE
    );
}

fn denial(fx: &Fixture, command: &str) -> aaf_hook::Denial {
    match fx.bash(command) {
        Decision::Deny(denial) => denial,
        Decision::Allow => panic!("allowed: {command}"),
    }
}

/// O12-R10-5: no deny text tells an agent that runs unattended to ask the
/// user; the ones for a human's step say how to finish without it.
#[test]
fn o12_r10_5_deny_texts_suit_an_unattended_agent() {
    let fx = fixture();
    fx.write(".env", "TOKEN=x\n");
    let human =
        "this needs a human: finish the task without it and name the step in your final message";
    for (command, rule) in [
        ("echo x > .git/hooks/pre-commit", "git_guard"),
        ("aaf approve t1", "human_action_guard"),
        ("cat .env", "secret_guard"),
        ("fastboot flash boot boot.img", "adb_guard"),
    ] {
        let got = denial(&fx, command);
        assert_eq!(got.rule, rule, "{command}");
        assert!(got.fix.contains(human), "{command}: {}", got.fix);
        assert!(
            !got.message().contains("ask the user"),
            "{command}: {}",
            got.fix
        );
    }
    let named = denial(
        &fx,
        "python3 - <<'EOF'\nimport sys\nt = '.git/hooks/pre-commit'\nopen(sys.argv[1], 'w').write(t)\nEOF\n",
    );
    assert_eq!(named.rule, "interp_guard");
    assert!(
        named.reason.contains(
            "the script's write target is not a literal, so every path it names was checked"
        ),
        "{}",
        named.reason
    );
    assert_eq!(
        named.fix,
        "write the file with the Write/Edit tool or give open() a literal path"
    );
    // A literal target keeps the reason and fix of the path it names.
    let literal = denial(
        &fx,
        "python3 - <<'EOF'\nopen('.git/hooks/pre-commit', 'w').write('x')\nEOF\n",
    );
    assert!(
        !literal.reason.contains("not a literal"),
        "{}",
        literal.reason
    );
    let places = denial(&fx, "cd a; cd b; cd c; cd d; cd e; cd f; cd g; ls");
    assert!(places.reason.contains("too many ways"), "{}", places.reason);
    assert!(places.fix.contains("(cd dir && make)"), "{}", places.fix);
}
