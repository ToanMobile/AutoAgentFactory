#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for SEC-082:
//! `delete_guard::check_cmd` must block `ln` overwriting a protected file
//! (such as `.claude/settings.json`) and `gzip` replacing a protected file in
//! place (such as `.env`), while allowing `ln` and `gzip` on non-protected
//! files.

mod common;

use common::{Fixture, run_corpus};

const ALLOW: Option<&str> = None;
const DELETE: Option<&str> = Some("delete_guard");

#[test]
fn ln_and_gzip_targeting_protected_files_are_denied() {
    let fx = Fixture::new();
    fx.write(".claude/settings.json", "{}");
    fx.write(".env", "K=V");
    run_corpus(
        &fx,
        &[
            ("ln -sf /dev/null .claude/settings.json", DELETE),
            ("gzip .env", DELETE),
        ],
    );
}

#[test]
fn ln_and_gzip_on_non_protected_files_stay_allowed() {
    let fx = Fixture::new();
    fx.write("src/lib.rs", "pub fn ok() {}\n");
    fx.write("docs/README.md", "# docs\n");
    fx.write("build/out.log", "ok\n");
    fx.write(".env.example", "KEY=example\n");
    run_corpus(
        &fx,
        &[
            ("ln -s src/lib.rs docs/link.rs", ALLOW),
            ("gzip build/out.log", ALLOW),
            ("gzip -c .env.example > /dev/null", ALLOW),
        ],
    );
}
