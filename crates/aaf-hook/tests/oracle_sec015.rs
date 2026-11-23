#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Oracle test for SEC-015: `secret_guard` option-parsing bypasses.

mod common;

use common::{Fixture, run_corpus};

const SECRET: Option<&str> = Some("secret_guard");

fn fixture() -> Fixture {
    let fx = Fixture::new();
    fx.write(".env", "KEY=supersecret\n");
    fx.write("app/release.keystore", "dummy\n");
    fx
}

#[test]
fn sec015_blocks_base64_input_flag_on_secret_files() {
    let fx = fixture();
    run_corpus(
        &fx,
        &[
            ("base64 -i .env", SECRET),
            ("base64 -i app/release.keystore", SECRET),
        ],
    );
}

#[test]
fn sec015_blocks_inline_expr_flags_on_secret_files() {
    let fx = fixture();
    run_corpus(
        &fx,
        &[
            ("grep --regexp=KEY .env", SECRET),
            ("grep -eKEY .env", SECRET),
            ("sed --expression=p .env", SECRET),
            ("rg --regexp=KEY .env", SECRET),
        ],
    );
}
