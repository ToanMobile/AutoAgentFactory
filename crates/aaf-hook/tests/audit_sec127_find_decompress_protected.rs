#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for SEC-127:
//! `find` paired with a decompressor (`-exec gunzip`, `-exec unzstd`,
//! `-exec gzip -d`, or piped to `xargs gunzip`) strips the compression suffix
//! and writes the base filename, so a `-name '*.gz'` or
//! `-name 'settings.json.gz'` filter must not exempt it from `delete_guard`
//! when the decompressed name could match a protected file.

mod common;

use common::{Fixture, run_corpus};

const ALLOW: Option<&str> = None;
const DELETE: Option<&str> = Some("delete_guard");

fn fixture() -> Fixture {
    let fx = Fixture::new();
    fx.write(".claude/settings.json", "{}");
    fx.write(".claude/settings.json.gz", "compressed");
    fx.write(".env", "K=V");
    fx.write("dist/app.log.gz", "compressed");
    fx
}

#[test]
fn find_decompressing_into_protected_names_is_denied() {
    let fx = fixture();
    run_corpus(
        &fx,
        &[
            ("find . -name '*.gz' -exec gunzip -f {} +", DELETE),
            (
                r"find .claude -name 'settings.json.gz' -exec gunzip -f {} \;",
                DELETE,
            ),
            ("find . -name '*.zst' -exec unzstd -f {} +", DELETE),
            ("find . -name '*.gz' | xargs gunzip -f", DELETE),
            ("find . -name '*.gz' -exec gzip -df {} +", DELETE),
        ],
    );
}

#[test]
fn find_decompressing_unprotected_names_stays_allowed() {
    let fx = fixture();
    run_corpus(
        &fx,
        &[
            ("find dist -name '*.log.gz' -exec gunzip -f {} +", ALLOW),
            ("gunzip -f dist/app.log.gz", ALLOW),
            ("find dist -name '*.log' -exec gzip {} +", ALLOW),
        ],
    );
}
