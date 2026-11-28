//! Runner wrapper capturing infra failures, quota limit hits, failed log tails,
//! and final output text for auditor and proposer runs.

use std::sync::{Mutex, PoisonError};

use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, InfraFailure, InfraKind, LimitHit, PortResult,
};

use crate::factory::scheduler::DEFAULT_RATE_LIMIT_COOLDOWN_SECS;
use crate::factory::unblock;

/// Passes a run on to the real runner and captures any infra failure, usage
/// limit, failed log tail, and final text: wrappers like audit and subtask
/// proposer only report their own result.
pub(in crate::factory) struct InfraWatch<'a> {
    runner: &'a dyn AgentRunner,
    seen: Mutex<Option<InfraFailure>>,
    limit_hit: Mutex<Option<LimitHit>>,
    failed_tail: Mutex<Option<String>>,
    last_succeeded: Mutex<bool>,
    last_timed_out: Mutex<bool>,
    last_final_text: Mutex<String>,
}

impl<'a> InfraWatch<'a> {
    pub(in crate::factory) fn new(runner: &'a dyn AgentRunner) -> Self {
        Self {
            runner,
            seen: Mutex::new(None),
            limit_hit: Mutex::new(None),
            failed_tail: Mutex::new(None),
            last_succeeded: Mutex::new(false),
            last_timed_out: Mutex::new(false),
            last_final_text: Mutex::new(String::new()),
        }
    }

    pub(in crate::factory) fn take(self) -> (Option<InfraFailure>, Option<LimitHit>) {
        let (infra, hit, _, _, _, _) = self.take_all();
        (infra, hit)
    }

    pub(in crate::factory) fn take_all(
        self,
    ) -> (
        Option<InfraFailure>,
        Option<LimitHit>,
        Option<String>,
        bool,
        bool,
        String,
    ) {
        (
            self.seen
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner),
            self.limit_hit
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner),
            self.failed_tail
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner),
            self.last_succeeded
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner),
            self.last_timed_out
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner),
            self.last_final_text
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }
}

impl AgentRunner for InfraWatch<'_> {
    fn run(&self, request: &AgentRequest) -> PortResult<AgentOutcome> {
        let outcome = self.runner.run(request)?;
        *self
            .last_succeeded
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = outcome.succeeded();
        *self
            .last_timed_out
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = outcome.timed_out;
        *self
            .last_final_text
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = outcome.final_text.clone();
        if let Some(infra) = &outcome.infra {
            *self.seen.lock().unwrap_or_else(PoisonError::into_inner) = Some(infra.clone());
        }
        if let Some(hit) = &outcome.limit_hit {
            *self
                .limit_hit
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(hit.clone());
        }
        if !outcome.succeeded()
            && !outcome.timed_out
            && outcome.infra.is_none()
            && outcome.limit_hit.is_none()
            && !outcome.log_tail.trim().is_empty()
        {
            *self
                .failed_tail
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(outcome.log_tail.clone());
        }
        Ok(outcome)
    }

    fn reports_tokens(&self) -> bool {
        self.runner.reports_tokens()
    }
}

fn has_status_code(lower: &str, code: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "http ",
        "http/1.1 ",
        "http/2 ",
        "status ",
        "status: ",
        "status=",
        "code ",
        "code: ",
        "code=",
        "error ",
        "error: ",
    ];
    PREFIXES
        .iter()
        .any(|p| lower.contains(&format!("{p}{code}")))
}

/// Reads a failed auditor log tail for a rate limit, a refused login, or a
/// network failure. For runners with structured token parsers
/// (`tokens_measured == true`, e.g. `ClaudeCodeRunner` / `AgyRunner`), skips
/// JSON-object lines (already parsed by the runner) so `stream-json` telemetry
/// or assistant prose on `error_max_turns` never triggers false cooldowns or
/// auth halts, while plain-text stderr errors are still classified.
pub(super) fn classify_auditor_log_tail(
    tail: &str,
    tokens_measured: bool,
    now: i64,
    infra: &mut Option<InfraFailure>,
    limit_hit: &mut Option<LimitHit>,
) {
    let filtered: String;
    let effective = if tokens_measured {
        filtered = crate::executors::agy::strip_ansi_and_json_object(tail);
        if filtered.trim().is_empty() {
            return;
        }
        filtered.as_str()
    } else {
        tail
    };
    let lower = effective
        .to_ascii_lowercase()
        .replace("rate_limit_event", " ")
        .replace("rate_limit_info", " ");
    let raw = unblock::one_line(effective, 300);
    let has_any = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));
    let quota_hit = has_status_code(&lower, "429")
        || has_any(&[
            "429 too many",
            "too many requests",
            "rate limit",
            "rate_limit",
            "quota",
        ]);
    let auth_hit = has_status_code(&lower, "401")
        || has_status_code(&lower, "403")
        || has_any(&[
            "401 unauthorized",
            "403 forbidden",
            "unauthorized",
            "invalid api key",
            "invalid_api_key",
            "authentication failed",
        ]);
    let net_hit = has_any(&[
        "econnrefused",
        "etimedout",
        "econnreset",
        "enotfound",
        "eai_again",
        "ehostunreach",
        "enetunreach",
        "connection refused",
        "connection timed out",
        "connection dropped",
        "can't reach the api server",
        "unable to connect to api",
        "could not resolve host",
        "no internet route",
        "503 service unavailable",
        "529 overloaded",
    ]) || ["502", "503", "504", "529"]
        .iter()
        .any(|c| has_status_code(&lower, c));
    if quota_hit {
        *limit_hit = Some(LimitHit {
            reset_at: Some(now.saturating_add(DEFAULT_RATE_LIMIT_COOLDOWN_SECS)),
            raw,
        });
    } else if auth_hit {
        *infra = Some(InfraFailure {
            kind: InfraKind::Auth,
            raw,
        });
    } else if net_hit {
        *infra = Some(InfraFailure {
            kind: InfraKind::Network,
            raw,
        });
    }
}
