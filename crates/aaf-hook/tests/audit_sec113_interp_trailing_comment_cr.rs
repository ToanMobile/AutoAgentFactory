#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Verification tests for SEC-113:
//! 1. Trailing (inline) comments containing an unmatched quote (`# don't`,
//!    `# it's`, `// don't`) must not desynchronize quote tracking in
//!    `interp_guard` and hide a dangerous command on the next line.
//! 2. Carriage-return (`\r`) and ECMAScript line-separator (`\u{2028}`) line
//!    breaks after a comment must not hide a following dangerous command from
//!    `interp_guard`.

mod common;

use common::{Fixture, run_corpus};

const ALLOW: Option<&str> = None;
const INTERP: Option<&str> = Some("interp_guard");

#[test]
fn trailing_comment_quote_does_not_hide_payload() {
    let fx = Fixture::new();

    // Baseline: without trailing comments, every payload is already blocked by `interp_guard`.
    run_corpus(
        &fx,
        &[
            (
                "python3 -c \"import os\nos.system('git push --force')\"",
                INTERP,
            ),
            ("ruby -e \"x = 1\nsystem 'git push --force'\"", INTERP),
            (
                "node -e \"let a = 1\nrequire('child_process').execSync('git push --force')\"",
                INTERP,
            ),
        ],
    );

    // Trailing comments with an unmatched single quote must not hide the payload.
    run_corpus(
        &fx,
        &[
            (
                "python3 -c \"import os # don't\nos.system('git push --force')\"",
                INTERP,
            ),
            (
                "ruby -e \"x = 1 # it's\nsystem 'git push --force'\"",
                INTERP,
            ),
            (
                "node -e \"let a = 1 // don't\nrequire('child_process').execSync('git push --force')\"",
                INTERP,
            ),
        ],
    );
}

#[test]
fn carriage_return_line_break_does_not_hide_payload() {
    let fx = Fixture::new();

    // Baseline: without the leading comment, every payload is already blocked by `interp_guard`.
    run_corpus(
        &fx,
        &[
            (
                "python3 -c \"x = 1\rimport os;os.system('git push --force')\"",
                INTERP,
            ),
            (
                "node -e \"let a = 1\rrequire('child_process').execSync('git push --force')\"",
                INTERP,
            ),
            (
                "node -e \"let a = 1\u{2028}require('child_process').execSync('git push --force')\"",
                INTERP,
            ),
        ],
    );

    // `\r` and U+2028 terminate single-line comments in Python/Node and must not hide the payload.
    run_corpus(
        &fx,
        &[
            (
                "python3 -c \"# x'\rimport os;os.system('git push --force')\"",
                INTERP,
            ),
            (
                "node -e \"// x'\rrequire('child_process').execSync('git push --force')\"",
                INTERP,
            ),
            (
                "node -e \"// x'\u{2028}require('child_process').execSync('git push --force')\"",
                INTERP,
            ),
        ],
    );
}

#[test]
fn benign_trailing_comments_and_cr_stay_allowed() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("python3 -c \"x = 1 # don't worry\nprint(x)\"", ALLOW),
            ("python3 -c \"print(1)\rprint(2)\"", ALLOW),
            ("node -e \"let a = 1 // it's ok\nconsole.log(a)\"", ALLOW),
        ],
    );
}
