#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Verification tests for SEC-083:
//! `paste` and `fmt` print file contents to stdout and must be covered by
//! `secret_guard` when given a secret file such as `.env`, while continuing
//! to allow plain non-secret files.

mod common;

use common::{Fixture, run_corpus};

const ALLOW: Option<&str> = None;
const SECRET: Option<&str> = Some("secret_guard");

#[test]
fn paste_and_fmt_of_secret_file_are_blocked() {
    let fx = Fixture::new();
    fx.write(".env", "K=V");
    run_corpus(
        &fx,
        &[
            ("paste .env", SECRET),
            ("fmt .env", SECRET),
            ("paste -s .env", SECRET),
            ("fmt -w 80 .env", SECRET),
        ],
    );
}

#[test]
fn cat_env_is_blocked_baseline() {
    let fx = Fixture::new();
    fx.write(".env", "K=V");
    run_corpus(&fx, &[("cat .env", SECRET)]);
}

#[test]
fn paste_and_fmt_of_plain_files_still_allowed() {
    let fx = Fixture::new();
    fx.write("a.txt", "alpha\n");
    fx.write("b.txt", "beta\n");
    fx.write("README.md", "hello world\n");
    run_corpus(
        &fx,
        &[
            ("paste a.txt b.txt", ALLOW),
            ("fmt README.md", ALLOW),
            ("fmt -w 72 README.md", ALLOW),
        ],
    );
}
