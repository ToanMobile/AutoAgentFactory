#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `AgyRunner` text failure classification: HTTP status codes count only in an
//! HTTP context, never as line numbers, counts, ids or sandbox errors.

use aaf_domain::ports::InfraKind;
use aaf_engine::executors::agy::parse_agy_output;

#[test]
fn agy_status_digits_outside_http_context_are_not_infra() {
    // Line numbers, counts, ids and sandbox errors that merely contain
    // 401/403/429 must reach the unblock ladder as ordinary failures.
    for text in [
        "test failed at src/lib.rs:401:9",
        "error[E0403]: the name `T` is already used",
        "ran 403 tests, 2 failed",
        "commit 4291f0c broke the build",
        "cp: /repo/.git/hooks/pre-commit: Permission denied (os error 13)",
        "warning: credential helper not found, using defaults",
    ] {
        let parsed = parse_agy_output(text, false);
        assert!(
            parsed.limit_hit.is_none() && parsed.infra.is_none(),
            "{text:?} misclassified: limit={:?} infra={:?}",
            parsed.limit_hit,
            parsed.infra.as_ref().map(|i| i.kind)
        );
    }
    // The same codes in an HTTP context still classify.
    for (text, kind) in [
        ("request failed: status 403", InfraKind::Auth),
        ("HTTP/1.1 401 Unauthorized", InfraKind::Auth),
        ("upstream returned code=403", InfraKind::Auth),
    ] {
        let parsed = parse_agy_output(text, false);
        assert_eq!(
            parsed.infra.as_ref().map(|i| i.kind),
            Some(kind),
            "{text:?}"
        );
    }
    for text in ["Error 429: slow down", "got 429 Too Many Requests"] {
        assert!(
            parse_agy_output(text, false).limit_hit.is_some(),
            "{text:?}"
        );
    }
}
