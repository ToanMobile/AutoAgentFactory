#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for the SEC-127 rework 3:
//! a word of a destructive `find` expression filled in at run time may turn
//! into `{}` right before `+` (ending an `-exec` argv early), into the `;`
//! that ends it, into `-delete` or `-o`, or into a `-name` pattern that names
//! a protected file, so such a `find` is opaque and refused. GNU `-ok` and
//! `-okdir` never end at `+` (only `;` ends them) while BSD ends them at
//! `{} +`, so an `-ok`/`-okdir` argv holding `{} +` is refused too.
//! Finds that only print, and static destructive finds, are unchanged.

mod common;

use common::{Fixture, run_corpus};

const ALLOW: Option<&str> = None;
const DELETE: Option<&str> = Some("delete_guard");

fn fixture() -> Fixture {
    let fx = Fixture::new();
    fx.write(".claude/settings.json", "{}");
    fx.write(".env", "K=V");
    fx.write("dist/app.log.gz", "compressed");
    fx.write("build/out.log", "ok");
    fx
}

#[test]
fn dynamic_word_before_plus_is_opaque() {
    let fx = fixture();
    run_corpus(
        &fx,
        &[
            (r#"B={}; find . -exec true "$B" + -delete"#, DELETE),
            (r#"find . -exec true "$B" + -delete"#, DELETE),
            ("find . -exec true $B + -delete", DELETE),
            ("find . -exec true $(printf '{}') + -delete", DELETE),
        ],
    );
}

#[test]
fn dynamic_terminator_or_primary_is_opaque() {
    let fx = fixture();
    run_corpus(
        &fx,
        &[
            (r#"S=';'; find . -exec true "$S" -delete"#, DELETE),
            (r#"find . -exec true "$S" -delete"#, DELETE),
            ("find . -exec true $(printf ';') -delete", DELETE),
            (r#"D=-delete; find . "$D""#, DELETE),
            (r#"find . "$D""#, DELETE),
            ("find . $D", DELETE),
            (r#"find . -name x "$D""#, DELETE),
            (r#"find . -name '*.log' -exec true \; "$D""#, DELETE),
            (
                r"find . -name '*.log' -exec rm {} \; $(printf -- '-o') -delete",
                DELETE,
            ),
            (r#"find . -name '*.log' "$N" -delete"#, DELETE),
            (r#"find . -name "$P" -delete"#, DELETE),
            (r#"find . -name "$P" -exec rm {} +"#, DELETE),
        ],
    );
}

#[test]
fn ok_with_braces_plus_does_not_end_on_gnu() {
    let fx = fixture();
    run_corpus(
        &fx,
        &[
            (r"yes | find . -ok true {} + -name x \; -delete", DELETE),
            (r"yes | find . -okdir true {} + -name x \; -delete", DELETE),
        ],
    );
}

#[test]
fn non_destructive_or_static_find_stays_allowed() {
    let fx = fixture();
    run_corpus(
        &fx,
        &[
            (r#"find . -name "$P" -print"#, ALLOW),
            (r#"find . -name "$P" -ls"#, ALLOW),
            (r#"find . -newer "$F" -print"#, ALLOW),
            (r#"find "$D" -name '*.log' -print"#, ALLOW),
            ("find . -name '*.log' -exec rm {} +", ALLOW),
            (r"find . -name '*.log' -exec rm {} \;", ALLOW),
            ("find . -name '*.log' -exec rm {} + -print", ALLOW),
            ("find dist -name '*.log.gz' -execdir gunzip -f {} +", ALLOW),
            (r"find . -name '*.log' -ok rm {} \;", ALLOW),
            ("find . -type f -name '*.log' -delete", ALLOW),
        ],
    );
}
