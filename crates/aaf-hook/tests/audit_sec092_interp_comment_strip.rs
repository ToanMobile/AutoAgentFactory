#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for SEC-092:
//! `interp_guard::inspect_payload` must not strip lines starting with `#` or
//! `//` blindly before inspection. In Perl, `//;` is an executable empty-match
//! statement (not a comment), and a second `-e` line starting with `#` can sit
//! inside a multi-line string literal opened by an earlier `-e` block.

mod common;

use common::{Fixture, run_corpus};

const ALLOW: Option<&str> = None;
const INTERP: Option<&str> = Some("interp_guard");

#[test]
fn perl_fake_comment_prefixes_do_not_bypass_interp_guard() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("perl -e '//;system(\"git push --force\")'", INTERP),
            ("perl -e '\"' -e '#\";system(\"git push --force\")'", INTERP),
        ],
    );
}

#[test]
fn benign_comment_only_and_harmless_scripts_stay_allowed() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("perl -e '# just a note' -e 'print 1'", ALLOW),
            ("python3 -c '# note'", ALLOW),
        ],
    );
}
