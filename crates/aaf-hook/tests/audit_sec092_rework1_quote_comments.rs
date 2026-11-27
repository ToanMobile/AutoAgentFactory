#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for SEC-092 rework 1:
//! Full-line comments containing an unmatched quote (`don't`, `it's`) must not
//! desynchronize quote tracking in `interp_guard` and allow following dangerous
//! code to slip through.

mod common;

use common::{Fixture, run_corpus};

const ALLOW: Option<&str> = None;
const INTERP: Option<&str> = Some("interp_guard");

#[test]
fn odd_quote_in_comment_line_does_not_bypass_interp_guard() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            (
                "perl -e \"# don't\nsystem \\\"git push --force\\\"\"",
                INTERP,
            ),
            ("ruby -e \"# it's\nsystem 'git push --force'\"", INTERP),
            (
                "python3 -c \"# don't\nimport os; os.system('git push --force')\"",
                INTERP,
            ),
            (
                "node -e \"// don't\nrequire('child_process').execSync('git push --force')\"",
                INTERP,
            ),
        ],
    );
}

#[test]
fn benign_scripts_with_odd_quote_comments_stay_allowed() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("perl -e \"# don't panic\" -e 'print 1'", ALLOW),
            ("python3 -c \"# it's fine\nprint(1)\"", ALLOW),
        ],
    );
}
