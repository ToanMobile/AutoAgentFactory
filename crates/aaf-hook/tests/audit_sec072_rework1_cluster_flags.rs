#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for SEC-072 rework 1:
//! - Ruby `-K<ch>e` consumes one char after `-K` and continues cluster parsing.
//! - Perl `'-F... -e'` ends `-F`'s value at ASCII whitespace and parses a
//!   following `-...` as a new flag cluster (also feeding `git_files`).

mod common;

use std::fs;

use common::{Fixture, run_corpus};

const ALLOW: Option<&str> = None;
const INTERP: Option<&str> = Some("interp_guard");
const GIT_GUARD: Option<&str> = Some("git_guard");

fn fixture_with_hooks() -> Fixture {
    let fx = Fixture::new();
    fs::create_dir_all(fx.root.join(".git/hooks")).unwrap();
    fx
}

#[test]
fn ruby_k_consumes_one_char_and_continues_cluster() {
    let fx = fixture_with_hooks();
    run_corpus(
        &fx,
        &[
            ("ruby -Kue \"exec 'git push --force'\"", INTERP),
            ("ruby -Kne \"exec 'git push --force'\"", INTERP),
            ("ruby -Kse \"exec 'git push --force'\"", INTERP),
            ("ruby -Kae \"exec 'git push --force'\"", INTERP),
            (
                "ruby -KUe \"File.write('.git/hooks/pre-commit', 'x')\"",
                INTERP,
            ),
        ],
    );
}

#[test]
fn perl_f_space_delimited_flag_cluster_is_inspected() {
    let fx = fixture_with_hooks();
    run_corpus(
        &fx,
        &[
            (
                "perl '-F -e' 'BEGIN { open(F, \">.git/hooks/pre-commit\") }'",
                INTERP,
            ),
            (
                "perl '-F: -e' 'BEGIN { open(F, \">.git/hooks/pre-commit\") }'",
                INTERP,
            ),
        ],
    );
}

#[test]
fn valid_ruby_k_and_perl_f_commands_stay_allowed() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("ruby -Ku -e 'puts 1'", ALLOW),
            ("perl -F: -lane 'print $F[0]' /etc/hosts", ALLOW),
            ("perl '-F: -lane' 'print $F[0]' /etc/hosts", ALLOW),
        ],
    );
}

#[test]
fn perl_f_space_delimited_inline_write_blocked_by_git_files() {
    let fx = fixture_with_hooks();
    run_corpus(
        &fx,
        &[
            (
                "perl '-F -e' 'open(my $f, \">\", $ARGV[0])' .git/hooks/pre-commit",
                GIT_GUARD,
            ),
            (
                "perl '-F: -e' 'open(my $f, \">\", $ARGV[0])' .git/hooks/pre-commit",
                GIT_GUARD,
            ),
        ],
    );
}
