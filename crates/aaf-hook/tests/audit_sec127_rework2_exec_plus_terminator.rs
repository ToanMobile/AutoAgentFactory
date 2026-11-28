#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for the SEC-127 rework 2:
//! GNU and BSD `find` end `-exec`/`-execdir` at `+` only when the word right
//! before it is exactly `{}`; any other `+` is an argument and the command
//! runs on to `;`. Ending the argv at a bare `+` lets a `-name` that is an
//! argument of the command (`-exec true + -name x \;`) pass as a filter,
//! hides the `{}` of `-exec rm + {} \;` so every file is deleted, and hides
//! the operands the command is checked with (`-exec rm + .env \;`).

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
    fx.write("build/out.log", "ok");
    fx
}

#[test]
fn plus_ends_exec_only_after_braces() {
    let fx = fixture();
    run_corpus(
        &fx,
        &[
            (r"find . -exec true + -name x \; -delete", DELETE),
            (r"find . -exec true {} x + -name y \; -delete", DELETE),
            (
                r"find . -exec true + -name x.log \; -exec gunzip -f {} +",
                DELETE,
            ),
            (r"find . -exec rm + {} \;", DELETE),
            (r"find . -name '*.log' -exec rm + .env \;", DELETE),
        ],
    );
}

#[test]
fn exec_terminated_by_braces_plus_or_semicolon_stays_allowed() {
    let fx = fixture();
    run_corpus(
        &fx,
        &[
            ("find . -name '*.log' -exec rm {} +", ALLOW),
            ("find . -name '*.log' -exec gzip {} +", ALLOW),
            ("find dist -name '*.log.gz' -exec gunzip -f {} +", ALLOW),
            (r"find . -name '*.log' -exec rm {} \;", ALLOW),
            ("find . -name '*.log' -exec echo {} + -delete", ALLOW),
            // `rm + ./x.log` per match: a file named `+` and the logs only.
            (r"find . -name '*.log' -exec rm + {} \;", ALLOW),
        ],
    );
}
