#![allow(clippy::unwrap_used, clippy::expect_used)]
//! A launcher's own directory option (`pnpm -C`, `yarn --cwd`, `mise -C`,
//! `poetry -C`, `uv --directory`) moves its command the way `env -C` and
//! `sudo -D` do: `..` after a symlinked directory is resolved physically,
//! as `chdir(2)` does, not lexically.

mod common;

#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;

#[cfg(unix)]
use common::{Fixture, run_corpus};

#[cfg(unix)]
const ALLOW: Option<&str> = None;
#[cfg(unix)]
const DELETE: Option<&str> = Some("delete_guard");

#[test]
#[cfg(unix)]
fn launcher_dir_option_resolves_dotdot_through_a_symlink_physically() {
    let fx = Fixture::new();
    fs::create_dir_all(fx.root.join("build")).unwrap();
    fs::create_dir_all(fx.root.join("sub")).unwrap();
    let outside = fx.home.join("outside_fx5");
    fs::create_dir_all(outside.join("sub")).unwrap();
    // `escape/..` is the repo lexically but `outside_fx5` physically.
    symlink(outside.join("sub"), fx.root.join("escape")).unwrap();

    run_corpus(
        &fx,
        &[
            // The reference: `env -C` / `sudo -D` already resolve physically.
            ("env -C escape/.. rm -rf important", DELETE),
            ("sudo -D escape/.. rm -rf important", DELETE),
            // Launchers must agree with them.
            ("pnpm -C escape/.. exec rm -rf important", DELETE),
            ("pnpm --dir=escape/.. exec rm -rf important", DELETE),
            ("yarn --cwd escape/.. exec rm -rf important", DELETE),
            ("mise -C escape/.. exec -- rm -rf important", DELETE),
            ("mise -Cescape/.. exec -- rm -rf important", DELETE),
            ("poetry -C escape/.. run rm -rf important", DELETE),
            ("uv --directory escape/.. run rm -rf important", DELETE),
            // A real directory's `..` stays in the repository.
            ("env -C sub/.. rm -rf build", ALLOW),
            ("pnpm -C sub/.. exec rm -rf build", ALLOW),
            ("yarn --cwd sub/.. exec rm -rf build", ALLOW),
        ],
    );
}
