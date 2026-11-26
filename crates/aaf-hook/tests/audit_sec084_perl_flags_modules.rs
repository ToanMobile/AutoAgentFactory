#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for SEC-084:
//! 1. Perl `-0x<hex>` record-separator flags ending in `e`/`E` must not treat
//!    that hex digit as `-e`/`-E` and swallow the real `-e`/`-E` flag and payload.
//! 2. Perl `-M<module>;<code>`, `-M-<module>;<code>`, `-d:<module>;<code>`, and
//!    `-dt:<module>;<code>` (all execute inline Perl code at compile time on
//!    real `perl`) must be inspected by `interp_guard`.

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
fn perl_hex_record_separator_lowercase_e_does_not_hide_inline_payload() {
    let fx = fixture_with_hooks();
    run_corpus(
        &fx,
        &[(
            "perl -0x1e -e 'BEGIN { open(F, \">.git/hooks/pre-commit\") }'",
            INTERP,
        )],
    );
}

#[test]
fn perl_hex_record_separator_uppercase_e_does_not_hide_inline_payload() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[("perl -0x1E -e 'system(\"git push --force\")'", INTERP)],
    );
}

#[test]
fn perl_hex_record_separator_does_not_hide_uppercase_e_flag() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[("perl -0x1e -E 'system(\"git push --force\")'", INTERP)],
    );
}

#[test]
fn perl_uppercase_m_module_inline_code_exec_is_denied() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            (
                "perl '-MPOSIX;system(\"git push --force\")' /dev/null",
                INTERP,
            ),
            (
                "perl '-M-POSIX;system(\"git push --force\")' /dev/null",
                INTERP,
            ),
        ],
    );
}

#[test]
fn perl_uppercase_m_module_inline_code_write_is_denied() {
    let fx = Fixture::new();
    fx.write(".claude/settings.json", "{}");
    run_corpus(
        &fx,
        &[(
            "perl '-MPOSIX;open(F,\">.claude/settings.json\")' /dev/null",
            INTERP,
        )],
    );
}

#[test]
fn perl_debugger_module_inline_code_exec_is_denied() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            (
                "perl '-d:PPPort;system(\"git push --force\")' /dev/null",
                INTERP,
            ),
            (
                "perl '-dt:PPPort;system(\"git push --force\")' /dev/null",
                INTERP,
            ),
        ],
    );
}

#[test]
fn perl_debugger_module_inline_code_write_is_denied() {
    let fx = Fixture::new();
    fx.write(".claude/settings.json", "{}");
    run_corpus(
        &fx,
        &[(
            "perl '-d:PPPort;open(F,\">.claude/settings.json\")' /dev/null",
            INTERP,
        )],
    );
}

#[test]
fn plain_inline_payloads_are_still_denied() {
    let fx = fixture_with_hooks();
    fx.write(".claude/settings.json", "{}");
    run_corpus(
        &fx,
        &[
            (
                "perl -e 'BEGIN { open(F, \">.git/hooks/pre-commit\") }'",
                INTERP,
            ),
            ("perl -e 'system(\"git push --force\")'", INTERP),
            ("perl -e 'open(F,\">.claude/settings.json\")'", INTERP),
        ],
    );
}

#[test]
fn benign_perl_flags_and_modules_stay_allowed() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("perl -0x0a -ne 'print' notes.txt", ALLOW),
            ("perl -MPOSIX -e 'print 1'", ALLOW),
            ("perl -Mstrict -Mwarnings script.pl", ALLOW),
        ],
    );
}
