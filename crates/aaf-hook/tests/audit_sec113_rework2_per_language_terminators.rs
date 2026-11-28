#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Rework-2 oracle for SEC-113 (Mẫu C findings F13, F14):
//! 1. A `#`/`//` comment ends where the interpreter ends it, not at every
//!    line break any interpreter knows: Python ends it at `\n`/`\r`, Ruby and
//!    Perl at `\n` only, PHP at `\n`/`\r`/`?>`, and only JavaScript also at
//!    U+2028/U+2029. An odd quote after a character the interpreter keeps in
//!    the comment must not open a string that hides the payload below.
//! 2. A `/*` inside a JavaScript regex literal (`/a\/*/`) is not a block
//!    comment: it must not swallow the rest of the script.
//!
//! The terminators each interpreter keeps were checked with python3 3.15,
//! ruby 4.0, perl 5.34, php and node 26: the line after the comment runs.

mod common;

use common::{Fixture, run_corpus};

const ALLOW: Option<&str> = None;
const INTERP: Option<&str> = Some("interp_guard");

const PY: &str = "import os; os.system('git push --force')";
const JS: &str = "require('child_process').execSync('git push --force')";
const RB: &str = "system 'git push --force'";
const PHP: &str = "system('git push --force');";

/// `interp <<'EOF'` with `body` as the script it reads.
fn doc(interp: &str, body: &str) -> String {
    format!("{interp} <<'EOF'\n{body}\nEOF")
}

fn check(fx: &Fixture, rows: &[(String, Option<&str>)]) {
    let rows: Vec<(&str, Option<&str>)> = rows
        .iter()
        .map(|(command, expected)| (command.as_str(), *expected))
        .collect();
    run_corpus(fx, &rows);
}

#[test]
fn comment_terminators_are_per_language() {
    let fx = Fixture::new();
    check(
        &fx,
        &[
            (doc("python3", &format!("x = 1 # a\u{2028}'\n{PY}")), INTERP),
            (doc("python3", &format!("x = 1 # a\u{2029}'\n{PY}")), INTERP),
            (doc("ruby", &format!("x = 1 # a\r'\n{RB}")), INTERP),
            (doc("ruby", &format!("x = 1 # a\u{2028}'\n{RB}")), INTERP),
            (doc("perl", &format!("my $a = 1; # a\r'\n{RB}")), INTERP),
            (
                doc("php", &format!("<?php $a = 1; # a\u{2028}'\n{PHP}")),
                INTERP,
            ),
            (
                doc("php", &format!("<?php $a = 1; // a\u{2028}'\n{PHP}")),
                INTERP,
            ),
        ],
    );
}

#[test]
fn js_regex_with_slash_star_does_not_swallow_the_script() {
    let fx = Fixture::new();
    check(
        &fx,
        &[(
            doc("node", &format!("const r = /a\\/*/; // don't\n{JS}")),
            INTERP,
        )],
    );
}

#[test]
fn real_terminators_and_benign_scripts_stay_judged_as_before() {
    let fx = Fixture::new();
    check(
        &fx,
        &[
            // JavaScript does end a line comment at U+2028.
            (doc("node", &format!("// x'\u{2028}{JS}")), INTERP),
            // Python does end a comment at `\r`.
            (doc("python3", &format!("# x'\r{PY}")), INTERP),
            // Benign: what follows the separator is still the comment.
            (doc("python3", "x = 1 # a\u{2028}print(1)"), ALLOW),
            (doc("ruby", "x = 1 # a\rputs 1"), ALLOW),
            // Benign: a regex holding `/*`, then a harmless call.
            (
                doc("node", "const r = /a\\/*/; console.log(r.test('a'))"),
                ALLOW,
            ),
        ],
    );
}
