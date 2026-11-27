#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for GATE-131 rework 2: an `ln` target directory that does
//! not exist when the hook runs (made by `mkdir` earlier in the same line) or
//! that is a glob with `..` must still get every source name joined into it.

mod common;

use common::{Fixture, run_corpus};

const ALLOW: Option<&str> = None;
const DELETE: Option<&str> = Some("delete_guard");

#[test]
fn not_yet_existing_target_dirs_are_joined() {
    let fx = Fixture::new();
    fx.write(".autofactory/roles/prompt.md", "role\n");
    fx.write(".autofactory/README.md", "x\n");
    fx.write("build/k", "x\n");
    fx.write("a", "x\n");
    run_corpus(
        &fx,
        &[
            (
                "cd .autofactory/roles && mkdir n && ln -sf ../README.md n/",
                DELETE,
            ),
            (
                "cd .autofactory/roles && mkdir n && ln -sf ../README.md -tn/",
                DELETE,
            ),
            ("mkdir build/n && ln -sf ../../a build/n/", ALLOW),
            ("ln -tbuild/ a", ALLOW),
        ],
    );
}

#[test]
fn glob_dotdot_target_dirs_are_not_deduped() {
    let fx = Fixture::new();
    fx.write(".autofactory/roles/sub/inner/k", "x\n");
    fx.write(".autofactory/roles/sub/p.md", "role\n");
    fx.write("y/p.md", "x\n");
    std::os::unix::fs::symlink(
        fx.root.join(".autofactory/roles/sub/inner"),
        fx.root.join("s1"),
    )
    .unwrap();
    std::os::unix::fs::symlink(fx.root.join(".autofactory"), fx.root.join("link")).unwrap();
    run_corpus(
        &fx,
        &[
            ("ln -sf y/p.md . s*/..", DELETE),
            (
                "mkdir link/roles/n && ln -sf ../../../y/p.md link/roles/n/",
                DELETE,
            ),
            (
                "mkdir link/roles/n && ln -sf ../../../y/p.md -tlink/roles/n/",
                DELETE,
            ),
        ],
    );
}
