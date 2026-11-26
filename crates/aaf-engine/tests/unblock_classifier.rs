#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "unblock_support.rs"]
mod support;

use aaf_domain::budget::BudgetCheck;
use aaf_domain::gate::GateStatus;
use aaf_domain::ports::AgentOutcome;
use aaf_engine::unblocker::{FailureSignal, RerunResult, classify, failure_digest};
use support::{limit_outcome, outcome, receipt, stuck_signal};

fn base() -> FailureSignal {
    FailureSignal::empty()
}

fn timed_out() -> AgentOutcome {
    AgentOutcome {
        timed_out: true,
        exit_code: None,
        ..outcome(1, "still compiling")
    }
}

/// Every lower-precedence cause at once; rows switch on what they test.
fn everything_below_budget() -> FailureSignal {
    FailureSignal {
        outcome: Some(AgentOutcome {
            timed_out: true,
            ..limit_outcome(Some(77))
        }),
        gate: Some(receipt(
            GateStatus::Blocked,
            &["test_scoped failed"],
            "boom",
        )),
        merge_conflict_files: vec!["src/a.rs".to_string()],
        open_dependency: Some("dep-9".to_string()),
        test_rerun: Some(RerunResult { runs: 2, passed: 2 }),
        same_failure_count: 5,
        budget: BudgetCheck::Ok,
        audit_rejected: None,
    }
}

#[test]
fn precedence_table() {
    let all = everything_below_budget;
    let rows: Vec<(&str, FailureSignal, &str)> = vec![
        // One row per rule.
        (
            "tokens",
            FailureSignal {
                budget: BudgetCheck::TokensExceeded,
                ..base()
            },
            "budget_exceeded",
        ),
        (
            "wall",
            FailureSignal {
                budget: BudgetCheck::WallClockExceeded,
                ..base()
            },
            "budget_exceeded",
        ),
        (
            "cycles",
            FailureSignal {
                budget: BudgetCheck::CyclesExceeded,
                ..base()
            },
            "budget_exceeded",
        ),
        (
            "frozen",
            FailureSignal {
                budget: BudgetCheck::Frozen,
                ..base()
            },
            "budget_exceeded",
        ),
        (
            "dependency",
            FailureSignal {
                open_dependency: Some("d".into()),
                ..base()
            },
            "dependency",
        ),
        (
            "limit",
            FailureSignal {
                outcome: Some(limit_outcome(None)),
                ..base()
            },
            "quota",
        ),
        (
            "conflict",
            FailureSignal {
                merge_conflict_files: vec!["a".into()],
                ..base()
            },
            "merge_conflict",
        ),
        (
            "gate blocked",
            FailureSignal {
                gate: Some(receipt(GateStatus::Blocked, &["x"], "")),
                ..base()
            },
            "gate_failed",
        ),
        (
            "flaky",
            FailureSignal {
                test_rerun: Some(RerunResult { runs: 2, passed: 1 }),
                ..base()
            },
            "flaky_test",
        ),
        (
            "timeout",
            FailureSignal {
                outcome: Some(timed_out()),
                ..base()
            },
            "stuck",
        ),
        (
            "repeat",
            FailureSignal {
                same_failure_count: 3,
                ..stuck_signal("e")
            },
            "stuck",
        ),
        ("fallback", stuck_signal("e"), "stuck"),
        ("nothing known", base(), "stuck"),
        // Ties: the more specific cause wins.
        (
            "budget over all",
            FailureSignal {
                budget: BudgetCheck::Frozen,
                ..all()
            },
            "budget_exceeded",
        ),
        ("dependency over limit", all(), "dependency"),
        (
            "limit over conflict",
            FailureSignal {
                open_dependency: None,
                ..all()
            },
            "quota",
        ),
        (
            "conflict over gate",
            FailureSignal {
                open_dependency: None,
                outcome: Some(timed_out()),
                ..all()
            },
            "merge_conflict",
        ),
        (
            "non-test gate over flaky",
            FailureSignal {
                open_dependency: None,
                outcome: Some(timed_out()),
                merge_conflict_files: vec![],
                gate: Some({
                    let mut r = receipt(GateStatus::Blocked, &["lint failed"], "err");
                    r.commands[0].name = "lint".to_string();
                    r
                }),
                ..all()
            },
            "gate_failed",
        ),
        (
            "flaky over pure-test gate",
            FailureSignal {
                open_dependency: None,
                outcome: Some(timed_out()),
                merge_conflict_files: vec![],
                ..all()
            },
            "flaky_test",
        ),
        (
            "flaky over timeout",
            FailureSignal {
                open_dependency: None,
                outcome: Some(timed_out()),
                merge_conflict_files: vec![],
                gate: None,
                ..all()
            },
            "flaky_test",
        ),
        // Not-quite cases must not match the rule above them.
        (
            "rerun never passed is not flaky",
            FailureSignal {
                test_rerun: Some(RerunResult { runs: 2, passed: 0 }),
                ..base()
            },
            "stuck",
        ),
        (
            "unverified gate is not gate_failed",
            FailureSignal {
                gate: Some(receipt(GateStatus::Unverified, &["x"], "")),
                ..base()
            },
            "stuck",
        ),
    ];
    for (name, signal, want) in rows {
        assert_eq!(classify(&signal).kind(), want, "row: {name}");
    }
}

#[test]
fn reasons_carry_the_useful_detail() {
    use aaf_domain::blocked::BlockedReason as R;

    let quota = FailureSignal {
        outcome: Some(limit_outcome(Some(42))),
        ..base()
    };
    assert_eq!(classify(&quota), R::Quota { reset_at: Some(42) });

    let dep = FailureSignal {
        open_dependency: Some("dep-9".into()),
        ..base()
    };
    assert_eq!(
        classify(&dep),
        R::Dependency {
            task_id: "dep-9".into()
        }
    );

    let long = "x".repeat(500);
    let gate = FailureSignal {
        gate: Some(receipt(GateStatus::Blocked, &["lint failed", &long], "")),
        ..base()
    };
    let R::GateFailed { summary } = classify(&gate) else {
        panic!("expected gate_failed");
    };
    assert!(summary.starts_with("lint failed; xxx"));
    assert!(summary.chars().count() <= 200);

    let no_reasons = FailureSignal {
        gate: Some(receipt(GateStatus::Blocked, &[], "")),
        ..base()
    };
    assert_eq!(
        classify(&no_reasons),
        R::GateFailed {
            summary: "test_scoped failed".into()
        }
    );

    let two = FailureSignal {
        same_failure_count: 2,
        ..stuck_signal("e")
    };
    let three = FailureSignal {
        same_failure_count: 3,
        ..stuck_signal("e")
    };
    let detail = |s: &FailureSignal| match classify(s) {
        R::Stuck { detail } => detail,
        other => panic!("expected stuck, got {other:?}"),
    };
    assert!(detail(&two).starts_with("unclassified failure"));
    assert!(detail(&three).starts_with("same failure 3 times"));
    assert!(detail(&two).contains(&failure_digest(&two)));
}

#[test]
fn digest_ignores_numbers_paths_hex_and_uuids() {
    let a = stuck_signal(
        "thread 'main' (pid 4121) panicked at /tmp/run-17/src/auth.rs:42:9\n\
         ptr 0x7ffee4c0 commit 3f9a2c1b77 id 123e4567-e89b-12d3-a456-426614174000\n\
         finished in 0.52s at 2026-11-21T10:00:01Z",
    );
    let b = stuck_signal(
        "thread 'main' (pid 99) panicked at /Users/x/wt/other/src/auth.rs:57:13\n\
         ptr 0x10ab commit 00deadbeef12 id abcdefab-cdef-abcd-efab-cdefabcdefab\n\
         finished in 12.07s at 2027-01-02T23:59:59Z",
    );
    assert_eq!(failure_digest(&a), failure_digest(&b));
    assert_eq!(failure_digest(&a).len(), 16);
}

#[test]
fn digest_separates_different_failures() {
    let a = stuck_signal("panicked at src/auth.rs:42: token expired");
    let other_file = stuck_signal("panicked at src/session.rs:42: token expired");
    let other_text = stuck_signal("panicked at src/auth.rs:42: missing header");
    assert_ne!(failure_digest(&a), failure_digest(&other_file));
    assert_ne!(failure_digest(&a), failure_digest(&other_text));

    let gate_a = FailureSignal {
        gate: Some(receipt(
            GateStatus::Blocked,
            &["test_scoped failed"],
            "test login ... FAILED",
        )),
        ..base()
    };
    let gate_b = FailureSignal {
        gate: Some(receipt(
            GateStatus::Blocked,
            &["test_scoped failed"],
            "test logout ... FAILED",
        )),
        ..base()
    };
    assert_ne!(failure_digest(&gate_a), failure_digest(&gate_b));
}

#[test]
fn digest_prefers_the_gate_over_varying_agent_prose() {
    let with = |prose: &str| FailureSignal {
        outcome: Some(outcome(1, prose)),
        gate: Some(receipt(
            GateStatus::Blocked,
            &["test_scoped failed"],
            "test login ... FAILED",
        )),
        ..base()
    };
    assert_eq!(
        failure_digest(&with("I tried approach A")),
        failure_digest(&with("Sorry, attempting B instead"))
    );
}

#[test]
fn digest_is_order_independent_for_conflict_files() {
    let files = |list: &[&str]| FailureSignal {
        merge_conflict_files: list.iter().map(|f| (*f).to_string()).collect(),
        ..base()
    };
    assert_eq!(
        failure_digest(&files(&["src/a.rs", "src/b.rs"])),
        failure_digest(&files(&["src/b.rs", "src/a.rs"]))
    );
    assert_ne!(
        failure_digest(&files(&["src/a.rs"])),
        failure_digest(&files(&["src/b.rs"]))
    );
}

#[test]
fn digest_keeps_digits_inside_identifiers_such_as_compiler_codes() {
    let run = |code: &str, pid: u32, line: u32, secs: &str| {
        stuck_signal(&format!(
            "[pid {pid}] error[{code}]: cannot find value `x1` in this scope\n\
             --> /tmp/wt-{pid}/src/utf8.rs:{line}:5\n\
             {line} | let y = x1;\n\
             worker-{pid} took {secs}s (12ms idle) at 2026-11-21T10:00:01Z"
        ))
    };
    // Same failure, different pid, line number and duration: one digest.
    assert_eq!(
        failure_digest(&run("E0425", 4121, 42, "0.52")),
        failure_digest(&run("E0425", 99, 1057, "12.07"))
    );
    // A different compiler error at the same place is a different failure.
    assert_ne!(
        failure_digest(&run("E0425", 4121, 42, "0.52")),
        failure_digest(&run("E0308", 4121, 42, "0.52"))
    );
    assert_ne!(
        failure_digest(&stuck_signal("TS2304: cannot find name")),
        failure_digest(&stuck_signal("TS2322: cannot find name"))
    );
    assert_ne!(
        failure_digest(&stuck_signal("failed in sha256.rs")),
        failure_digest(&stuck_signal("failed in sha512.rs"))
    );
}
