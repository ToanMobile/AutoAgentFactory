#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for SEC-072: detached `-F`/`-K` flags on `perl`/`ruby`
//! must not swallow `-e` and hide inline payloads.

mod common;

use std::fs;

use common::{Fixture, run_corpus};

const ALLOW: Option<&str> = None;
const INTERP: Option<&str> = Some("interp_guard");

fn fixture_with_hooks() -> Fixture {
    let fx = Fixture::new();
    fs::create_dir_all(fx.root.join(".git/hooks")).unwrap();
    fx
}

#[test]
fn perl_detached_f_flag_does_not_hide_inline_payload() {
    let fx = fixture_with_hooks();
    run_corpus(
        &fx,
        &[(
            "perl -F -e 'BEGIN { open(F, \">.git/hooks/pre-commit\") }'",
            INTERP,
        )],
    );
}

#[test]
fn ruby_detached_k_flag_does_not_hide_inline_payload() {
    let fx = Fixture::new();
    run_corpus(&fx, &[("ruby -K -e \"exec 'git push --force'\"", INTERP)]);
}

#[test]
fn ruby_detached_f_flag_does_not_hide_inline_payload() {
    let fx = fixture_with_hooks();
    run_corpus(
        &fx,
        &[(
            "ruby -F -e \"File.write('.git/hooks/pre-commit', 'x')\"",
            INTERP,
        )],
    );
}

#[test]
fn plain_inline_payloads_are_still_denied() {
    let fx = fixture_with_hooks();
    run_corpus(
        &fx,
        &[
            (
                "perl -e 'BEGIN { open(F, \">.git/hooks/pre-commit\") }'",
                INTERP,
            ),
            ("ruby -e \"exec 'git push --force'\"", INTERP),
            (
                "ruby -e \"File.write('.git/hooks/pre-commit', 'x')\"",
                INTERP,
            ),
        ],
    );
}

#[test]
fn attached_value_flags_with_read_only_payload_stay_allowed() {
    let fx = Fixture::new();
    run_corpus(&fx, &[("perl -F: -lane 'print $F[0]' /etc/hosts", ALLOW)]);
}
