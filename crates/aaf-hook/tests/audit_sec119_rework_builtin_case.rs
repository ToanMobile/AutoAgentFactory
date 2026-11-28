#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Rework oracle for SEC-119 (Mẫu C finding F1):
//! folding the command name must not fold shell builtins. `DECLARE`,
//! `Export`, `TYPESET`, `READONLY` and `LOCAL` are not builtins (macOS has no
//! `/usr/bin/declare`), so `DECLARE CDPATH=` fails and leaves an inherited
//! `CDPATH` in place. The hook must not take it for a clear of `CDPATH` and
//! then judge a later `cd` in the wrong directory.
//!
//! The hook reads `CDPATH` from its own environment. Tests may not use
//! `unsafe` (`std::env::set_var`), so each test re-runs itself in a child
//! process of this test binary with `CDPATH=../..` set. From `repo/src` that
//! entry resolves `cd repo` to the repository root itself, while a cleared
//! `CDPATH` would resolve it to the harmless `repo/src/repo`.

mod common;

use std::process::Command;

use common::{Fixture, rule, run_corpus_in};

const ALLOW: Option<&str> = None;
const DELETE: Option<&str> = Some("delete_guard");

/// Marks the child run that carries the inherited `CDPATH`.
const CHILD_VAR: &str = "AAF_SEC119_REWORK_CHILD";

/// Runs `body` in a child test process whose environment has `CDPATH=../..`;
/// the parent fails with the child's output when the child fails.
fn with_inherited_cdpath(test_name: &str, body: impl FnOnce()) {
    if std::env::var_os(CHILD_VAR).is_some() {
        assert_eq!(std::env::var("CDPATH").as_deref(), Ok("../.."));
        body();
        return;
    }
    let out = Command::new(std::env::current_exe().unwrap())
        .args([test_name, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD_VAR, "1")
        .env("CDPATH", "../..")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "child run with CDPATH=../.. failed:\n{stdout}\n{stderr}"
    );
}

/// The repository lives at `<base>/repo`; the call is made from `repo/src`,
/// which also holds a harmless `repo/src/repo/` for a cleared `CDPATH`.
fn fixture() -> Fixture {
    let fx = Fixture::new();
    assert_eq!(fx.root.file_name().unwrap(), "repo");
    fx.write("src/lib.rs", "pub fn ok() {}\n");
    fx.write("src/repo/keep.txt", "x\n");
    fx
}

#[test]
fn uppercase_declarers_do_not_clear_an_inherited_cdpath() {
    with_inherited_cdpath(
        "uppercase_declarers_do_not_clear_an_inherited_cdpath",
        || {
            let fx = fixture();
            // Any refusal will do: base refuses these as `unparsable_command`
            // (the value `CDPATH` ends up with is unknown), a fix may refuse
            // them in `delete_guard`.
            let allowed: Vec<&str> = [
                "DECLARE CDPATH=; cd repo && rm -rf .",
                "Export CDPATH=; cd repo && rm -rf .",
                "TYPESET CDPATH=; cd repo && rm -rf .",
                "READONLY CDPATH=; cd repo && rm -rf .",
                "LOCAL CDPATH=; cd repo && rm -rf .",
            ]
            .into_iter()
            .filter(|command| rule(&fx.bash_in(&fx.root.join("src"), command)).is_none())
            .collect();
            assert!(
                allowed.is_empty(),
                "{} rows allowed, expected a refusal:\n{}",
                allowed.len(),
                allowed.join("\n")
            );
        },
    );
}

#[test]
fn real_cdpath_clear_and_cdpath_lookup_stay_judged_as_before() {
    with_inherited_cdpath(
        "real_cdpath_clear_and_cdpath_lookup_stay_judged_as_before",
        || {
            let fx = fixture();
            run_corpus_in(
                &fx,
                &fx.root.join("src"),
                &[
                    // `CDPATH` lookup lands on the repository root.
                    ("cd repo && rm -rf .", DELETE),
                    // A real clear: `cd repo` is `repo/src/repo`.
                    ("declare CDPATH=; cd repo && rm -rf .", ALLOW),
                ],
            );
        },
    );
}
