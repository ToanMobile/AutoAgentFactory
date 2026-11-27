#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for GATE-131 rework 1: a glob target directory of `ln`
//! (with or without a trailing `/`) still gets every source name joined into
//! it, so `ln` cannot replace a file under `.autofactory/` through
//! `.auto*/roles/`.

mod common;

use common::{Fixture, run_corpus};

const ALLOW: Option<&str> = None;
const DELETE: Option<&str> = Some("delete_guard");

#[test]
fn glob_target_dirs_are_joined() {
    let fx = Fixture::new();
    fx.write(".autofactory/roles/prompt.md", "role\n");
    fx.write(".claude/settings.json", "{}\n");
    fx.write("y/prompt.md", "x\n");
    fx.write("y/settings.json", "{}\n");
    fx.write("build/k", "x\n");
    run_corpus(
        &fx,
        &[
            ("ln -sf ../y/prompt.md .auto*/roles/", DELETE),
            ("ln -sf ../y/prompt.md .auto*/roles", DELETE),
            ("ln -sf -t .auto*/roles ../y/prompt.md", DELETE),
            ("ln -sf y/settings.json .cl*de/", DELETE),
            ("ln -sf ../y/prompt.md bui*/", ALLOW),
        ],
    );
}
