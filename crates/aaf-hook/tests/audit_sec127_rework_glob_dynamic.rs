#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for the SEC-127 rework:
//! a `find -name` glob with a literal prefix (`release[.]keystore[.]gz`)
//! still names a protected file once a decompressor strips the suffix; a
//! compressor word or tool filled in at run time (`gzip $F`, `$T`) may carry
//! `-d -S <suffix>` and must be treated as opaque; and a `-name` word that is
//! an argument of `-exec` or the operand of another primary (`-fprint`) is
//! not a test and filters nothing.

mod common;

use common::{Fixture, run_corpus};

const ALLOW: Option<&str> = None;
const DELETE: Option<&str> = Some("delete_guard");

fn fixture() -> Fixture {
    let fx = Fixture::new();
    fx.write(".claude/settings.json", "{}");
    fx.write(".claude/settings.json.bak", "compressed");
    fx.write("release.keystore", "secret");
    fx.write("release.keystore.gz", "compressed");
    fx.write("dist/app.log.gz", "compressed");
    fx.write("build/out.log", "ok");
    fx
}

#[test]
fn glob_with_literal_prefix_is_denied() {
    let fx = fixture();
    run_corpus(
        &fx,
        &[
            (
                "find . -name 'release[.]keystore[.]gz' -exec gunzip -f {} +",
                DELETE,
            ),
            (
                "find . -name 'release?keystore?gz' -exec gunzip -f {} +",
                DELETE,
            ),
            ("find . -name 'rel*[.]gz' -exec gunzip -f {} +", DELETE),
        ],
    );
}

#[test]
fn dynamic_compressor_words_are_opaque() {
    let fx = fixture();
    run_corpus(
        &fx,
        &[
            ("find . -name '*.bak' -exec gzip $F {} +", DELETE),
            ("find . -name '*.bak' -exec gzip -df$X {} +", DELETE),
            ("find . -name '*.bak' -exec $T {} +", DELETE),
            ("find . -name '*.bak' | xargs $T", DELETE),
            ("find . -name '*.bak' | xargs gzip $F", DELETE),
        ],
    );
}

#[test]
fn name_inside_exec_argv_or_primary_operand_does_not_filter() {
    let fx = fixture();
    run_corpus(
        &fx,
        &[
            (r"find . -exec true -name x \; -delete", DELETE),
            (
                r"find . -exec true -name x.log \; -exec gunzip -f {} +",
                DELETE,
            ),
            ("find . -fprint -name -fprint x -delete", DELETE),
        ],
    );
}

#[test]
fn legitimate_find_uses_stay_allowed() {
    let fx = fixture();
    run_corpus(
        &fx,
        &[
            ("find dist -name '*.log.gz' -exec gunzip -f {} +", ALLOW),
            ("find build -name '*.log' -exec gzip {} +", ALLOW),
            ("find . -name '*.log' -exec rm {} +", ALLOW),
            ("find . -name '*.log' -delete", ALLOW),
            (r"find . -name '*.tar.gz' -exec tar -xzf {} \;", ALLOW),
            (r"find . -exec echo {} -exec rm {} \;", ALLOW),
        ],
    );
}
