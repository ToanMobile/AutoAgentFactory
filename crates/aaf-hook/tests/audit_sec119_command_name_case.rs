#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for SEC-119:
//! On case-insensitive filesystems (APFS default on macOS), uppercase or
//! mixed-case command names (`RM`, `Ln`, `Gzip`, `/BIN/RM`, `GIT`, `CAT`)
//! execute the exact same binaries as their lowercase counterparts and must
//! hit the same `aaf-hook` guards, while harmless uppercase commands stay
//! allowed.

mod common;

use common::{Fixture, run_corpus};

const ALLOW: Option<&str> = None;
const DELETE: Option<&str> = Some("delete_guard");
const GIT: Option<&str> = Some("git_guard");
const SECRET: Option<&str> = Some("secret_guard");

#[test]
fn uppercase_command_names_hit_the_same_guards() {
    let fx = Fixture::new();
    fx.write(".claude/settings.json", "{}");
    fx.write(".env", "K=V\n");
    fx.write("src/lib.rs", "pub fn ok() {}\n");

    // Baseline: lowercase command names are blocked by their respective guards.
    run_corpus(
        &fx,
        &[
            ("rm .env", DELETE),
            ("ln -sf /dev/null .claude/settings.json", DELETE),
            ("gzip .env", DELETE),
            ("/bin/rm -rf .claude", DELETE),
            ("git push --force origin main", GIT),
            ("cat .env", SECRET),
        ],
    );

    // Uppercase/mixed-case variants must trigger the exact same guards.
    run_corpus(
        &fx,
        &[
            ("RM .env", DELETE),
            ("Ln -sf /dev/null .claude/settings.json", DELETE),
            ("Gzip .env", DELETE),
            ("/BIN/RM -rf .claude", DELETE),
            ("GIT push --force origin main", GIT),
            ("CAT .env", SECRET),
        ],
    );
}

#[test]
fn uppercase_names_of_harmless_commands_stay_allowed() {
    let fx = Fixture::new();
    fx.write("src/lib.rs", "pub fn ok() {}\n");
    fx.write("build/out.log", "ok\n");

    run_corpus(
        &fx,
        &[
            ("rm build/out.log", ALLOW),
            ("LS -la src", ALLOW),
            ("Echo hi", ALLOW),
            ("RM build/out.log", ALLOW),
        ],
    );
}
