#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Oracle Phase 0 for G7 — Manual CLI repro pre-flight.
//!
//! These tests define the behaviour that MUST be true after wiring
//! `detector::repro::verify` into `aaf board add --repro-cmd` and
//! `aaf repro`.  They are written against the base commit (`92367ec`),
//! where none of the CLI flags or subcommands exist yet, so they MUST
//! fail at an assertion (not a compile error) on that base.
//!
//! **NEVER EDIT THIS FILE AFTER PHASE 0.**

#[path = "factory_support.rs"]
mod support;

use support::Fixture;

// ── Test 1 ──────────────────────────────────────────────────────────────────
//
// `aaf board add --repro-cmd /usr/bin/true` should produce a **non-zero exit**
// because `/usr/bin/true` exits 0, so the bug is NOT reproduced, and the task
// must NOT be created (0 tasks on board).
//
// On base `92367ec` clap rejects `--repro-cmd` as an unknown flag with exit 2
// (also non-zero), and 0 tasks — so the exit-code half of this assertion
// accidentally passes.  The TRUE RED assertion is Test 2 below.
#[test]
fn repro_cmd_true_refuses_task_creation_non_zero_exit() {
    let fx = Fixture::new();
    let out = fx.aaf(&[
        "board",
        "add",
        "--title",
        "bug with true",
        "--spec",
        "x",
        "--accept",
        "y",
        "--repro-cmd",
        "/usr/bin/true",
    ]);
    // non-zero because true exits 0 → NotReproduced
    assert_ne!(
        out.code, 0,
        "expected non-zero exit when repro is not reproduced"
    );

    // No task must have been created
    let list = fx.aaf(&["board", "list"]);
    let count = list.json["count"].as_u64().unwrap_or(0);
    assert_eq!(count, 0, "expected 0 tasks when repro not confirmed");
}

// ── Test 2  (the TRUE RED test on base 92367ec) ──────────────────────────────
//
// `aaf board add --repro-cmd /usr/bin/false` should:
//   - exit 0  (bug confirmed: false exits non-zero every run)
//   - create exactly 1 task
//
// On base `92367ec` `--repro-cmd` is unknown to clap, so exit != 0 and 0 tasks
// are created — both assertions fail, giving RED at assertion.
#[test]
fn repro_cmd_false_confirms_bug_and_creates_task() {
    let fx = Fixture::new();
    let out = fx.aaf(&[
        "board",
        "add",
        "--title",
        "real bug",
        "--spec",
        "x",
        "--accept",
        "y",
        "--repro-cmd",
        "/usr/bin/false",
    ]);
    assert_eq!(
        out.code, 0,
        "expected exit 0 when repro confirmed: {}",
        out.stderr
    );

    let list = fx.aaf(&["board", "list"]);
    let count = list.json["count"].as_u64().unwrap_or(0);
    assert_eq!(count, 1, "expected exactly 1 task after confirmed repro");
}

// ── Test 3 ───────────────────────────────────────────────────────────────────
//
// `aaf repro -- /usr/bin/false`  → exit 0  (Confirmed)
// `aaf repro -- /usr/bin/true`   → non-zero exit (NotReproduced)
//
// On base `92367ec` `aaf repro` does not exist → clap returns exit 2 for
// both, failing the first assert (exit 0 for false).
#[test]
fn repro_subcommand_false_confirmed_true_not_reproduced() {
    let fx = Fixture::new();

    // /usr/bin/false → exits non-zero every run → Confirmed → exit 0
    let confirmed = fx.aaf(&["repro", "--", "/usr/bin/false"]);
    assert_eq!(
        confirmed.code, 0,
        "aaf repro -- /usr/bin/false should exit 0 (Confirmed): {}",
        confirmed.stderr
    );

    // /usr/bin/true → exits zero every run → NotReproduced → non-zero exit
    let not_repro = fx.aaf(&["repro", "--", "/usr/bin/true"]);
    assert_ne!(
        not_repro.code, 0,
        "aaf repro -- /usr/bin/true should exit non-zero (NotReproduced): {}",
        not_repro.stderr
    );
}
