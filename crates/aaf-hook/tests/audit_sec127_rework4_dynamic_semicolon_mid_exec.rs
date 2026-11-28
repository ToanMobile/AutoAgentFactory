#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for the SEC-127 rework 4:
//! a word of an `-exec`/`-ok` argv filled in at run time may turn into the
//! `;` that ends it, so whatever follows it in that argv (`-delete`, `-o`,
//! `-name x`) becomes part of the find expression, ahead of a later literal
//! `;` or `{} +` that the hook took as the end. Such an argv is opaque when
//! any later word before that end is dynamic, starts with `-`, or is one of
//! `(`, `)`, `!`, `,`. A dynamic word followed only by `{}` stays allowed: if
//! it were `;`, the leftover `{}` would make find refuse to run.

mod common;

use common::{Fixture, run_corpus};

const ALLOW: Option<&str> = None;
const DELETE: Option<&str> = Some("delete_guard");

fn fixture() -> Fixture {
    let fx = Fixture::new();
    fx.write(".claude/settings.json", "{}");
    fx.write(".env", "K=V");
    fx.write("build/out.log", "ok");
    fx.write("src/lib.rs", "pub fn ok() {}\n");
    // The only one-character name, so an unquoted `?` expands to `;`.
    fx.write(";", "");
    fx
}

#[test]
fn dynamic_word_mid_exec_argv_is_opaque() {
    let fx = fixture();
    run_corpus(
        &fx,
        &[
            (
                r#"S=';'; find . -exec true "$S" -delete -exec true \;"#,
                DELETE,
            ),
            (
                r#"S=';'; find . -exec true "$S" -delete -exec true {} +"#,
                DELETE,
            ),
            (
                r"find . -exec true $(printf ';') -delete -exec true \;",
                DELETE,
            ),
            (
                r"find . -exec true ${S} -name x -o -delete -exec true \;",
                DELETE,
            ),
            (r"find . -exec true ? -delete -exec true \;", DELETE),
            (
                r#"env S=';' find . -exec true "$S" -delete -exec true \;"#,
                DELETE,
            ),
            (
                r#"sh -c 'find . -exec true "$1" -delete -exec true \;' _ ';'"#,
                DELETE,
            ),
        ],
    );
}

#[test]
fn dynamic_word_followed_only_by_braces_stays_allowed() {
    let fx = fixture();
    run_corpus(
        &fx,
        &[
            (r#"find . -name '*.rs' -exec grep -l "$PAT" {} +"#, ALLOW),
            (r#"find . -name '*.rs' -exec grep -l "$PAT" {} \;"#, ALLOW),
            (
                r#"find . -name '*.log' -exec sed -i "s/$A/$B/" {} +"#,
                ALLOW,
            ),
            ("find . -name '*.log' -exec rm {} +", ALLOW),
        ],
    );
}
