//! Text classification of Antigravity failures into usage limits, auth and
//! network infrastructure errors.

use aaf_domain::ports::{InfraFailure, InfraKind, LimitHit};

use super::RAW_LIMIT_CHARS;

fn any_needle(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

/// True when `code` (e.g. `"401"`) appears in `lower` as an HTTP status, not as
/// part of a line number, hash, or count: a standalone number right after a
/// status word (`http 401`, `status: 401`, `"code":401`) or right before its
/// reason phrase (`401 unauthorized`). `src/lib.rs:1401` or `403 tests` never match.
fn mentions_status_code(lower: &str, code: &str) -> bool {
    const BEFORE: &[&str] = &[
        "http",
        "http/1.0",
        "http/1.1",
        "http/2",
        "status",
        "status_code",
        "statuscode",
        "code",
        "error",
    ];
    const AFTER: &[&str] = &[
        "too many requests",
        "unauthorized",
        "forbidden",
        "resource_exhausted",
        "unauthenticated",
        "permission_denied",
    ];
    lower.match_indices(code).any(|(at, _)| {
        let head = &lower[..at];
        let tail = &lower[at + code.len()..];
        let standalone = !head.ends_with(|c: char| c.is_ascii_alphanumeric() || c == '.')
            && !tail.starts_with(|c: char| c.is_ascii_alphanumeric());
        if !standalone {
            return false;
        }
        let prefix = head.trim_end_matches(|c: char| " \t:=\"'([".contains(c));
        let suffix = tail.trim_start_matches(|c: char| " \t:-\"')]".contains(c));
        BEFORE.iter().any(|w| {
            prefix.ends_with(w)
                && !prefix[..prefix.len() - w.len()].ends_with(|c: char| c.is_ascii_alphanumeric())
        }) || AFTER.iter().any(|p| suffix.starts_with(p))
    })
}

pub(super) fn classify_text_failure(
    status: Option<i64>,
    text: &str,
) -> (Option<LimitHit>, Option<InfraFailure>) {
    const LIMIT: &[&str] = &[
        "too many requests",
        "resource_exhausted",
        "rate limit",
        "rate_limit",
        "quota exceeded",
        "insufficient_quota",
        "usage limit",
    ];
    // Status codes are matched only in an HTTP context (`mentions_status_code`),
    // and a file-system "permission denied" is a sandbox error, not an auth one.
    const AUTH: &[&str] = &[
        "unauthenticated",
        "invalid_api_key",
        "invalid api key",
        "invalid credentials",
        "invalid_grant",
        "authentication failed",
        "authentication_error",
        "login required",
        "not logged in",
        "permission_denied",
    ];
    const NET: &[&str] = &[
        "econnrefused",
        "econnreset",
        "etimedout",
        "enotfound",
        "eai_again",
        "connection refused",
        "connection reset",
        "connection timed out",
        "could not resolve host",
        "failed to resolve",
        "dns lookup failed",
        "temporary failure in name resolution",
        "network is unreachable",
        "bad gateway",
        "502 bad gateway",
        "503 service unavailable",
        "504 gateway timeout",
        "gateway timeout",
        "tls handshake",
    ];
    let lower = text.to_ascii_lowercase();
    let raw: String = text.chars().take(RAW_LIMIT_CHARS).collect();
    if status == Some(429) || any_needle(&lower, LIMIT) || mentions_status_code(&lower, "429") {
        return (
            Some(LimitHit {
                reset_at: None,
                raw,
            }),
            None,
        );
    }
    if matches!(status, Some(401 | 403))
        || any_needle(&lower, AUTH)
        || mentions_status_code(&lower, "401")
        || mentions_status_code(&lower, "403")
    {
        return (
            None,
            Some(InfraFailure {
                kind: InfraKind::Auth,
                raw,
            }),
        );
    }
    if matches!(status, Some(500..=599)) || any_needle(&lower, NET) {
        return (
            None,
            Some(InfraFailure {
                kind: InfraKind::Network,
                raw,
            }),
        );
    }
    (None, None)
}
