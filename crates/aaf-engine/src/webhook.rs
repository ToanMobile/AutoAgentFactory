//! Generic JSON webhook. Owned by lane `misc`.
//!
//! One event is one `POST` of one JSON document. The receiver is untrusted:
//! only the status code is looked at, the response body is never read.

use std::hash::{BuildHasher, Hasher};
use std::time::Duration;

use aaf_domain::event::EventKind;
use serde::Serialize;

/// Hard ceiling on retries, whatever the options ask for.
pub const MAX_RETRIES: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WebhookEvent {
    /// Unique per event; sent as `Idempotency-Key` so a retried delivery can
    /// be de-duplicated by the receiver.
    pub event_id: String,
    pub kind: EventKind,
    pub task_id: Option<String>,
    pub status: String,
    pub summary: String,
    /// Unix seconds.
    pub created_at: i64,
}

pub struct WebhookOptions {
    pub connect_timeout: Duration,
    /// Whole-request budget of a single attempt.
    pub timeout: Duration,
    /// Retries after the first attempt; values above [`MAX_RETRIES`] are capped.
    pub max_retries: u32,
    /// First backoff; doubles per retry, plus up to 50 % jitter.
    pub base_backoff: Duration,
    /// How to wait between attempts. Injected so tests do not sleep.
    pub sleeper: Box<dyn Fn(Duration) + Send + Sync>,
}

impl Default for WebhookOptions {
    fn default() -> Self {
        WebhookOptions {
            connect_timeout: Duration::from_secs(10),
            timeout: Duration::from_secs(15),
            max_retries: MAX_RETRIES,
            base_backoff: Duration::from_millis(500),
            sleeper: Box::new(std::thread::sleep),
        }
    }
}

/// Every `url` field holds the masked form (see [`mask_url`]).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WebhookError {
    #[error("invalid webhook url {url}: {reason}")]
    InvalidUrl { url: String, reason: &'static str },
    #[error("invalid webhook event: {0}")]
    InvalidEvent(&'static str),
    #[error("webhook payload could not be serialised: {0}")]
    Serialize(String),
    #[error("webhook {url} answered HTTP {status} after {attempts} attempt(s)")]
    Status {
        url: String,
        status: u16,
        attempts: u32,
    },
    #[error("webhook {url} failed after {attempts} attempt(s): {detail}")]
    Transport {
        url: String,
        detail: String,
        attempts: u32,
    },
}

/// The URL reduced to `scheme://host[:port]`, with `/***` standing for
/// anything after it. Userinfo, path, query and fragment are all dropped:
/// chat webhooks carry their token in the path, not only in the query.
pub fn mask_url(url: &str) -> String {
    let Some((scheme, rest)) = url.trim().split_once("://") else {
        return "<redacted url>".to_string();
    };
    let scheme_ok = !scheme.is_empty()
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    if !scheme_ok {
        return "<redacted url>".to_string();
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let suffix = if tail.is_empty() { "" } else { "/***" };
    format!("{scheme}://{host}{suffix}")
}

/// Builds a `ureq::Agent` configured with the timeouts and no-redirect policy
/// from `opts` so callers can reuse one connection/TLS pool across multiple
/// [`send_with_agent`] calls.
pub fn build_agent(opts: &WebhookOptions) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(opts.connect_timeout))
        .timeout_global(Some(opts.timeout))
        // Statuses are classified below, and a redirect target was never
        // validated, so it is reported instead of followed.
        .http_status_as_error(false)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .build()
        .new_agent()
}

/// Post `event` to `url`. An empty URL means notifications are off.
pub fn send(url: &str, event: &WebhookEvent, opts: &WebhookOptions) -> Result<(), WebhookError> {
    let url = url.trim();
    if url.is_empty() {
        return Ok(());
    }
    validate_url(url)?;
    let agent = build_agent(opts);
    send_with_agent(&agent, url, event, opts)
}

/// Post `event` to `url` using a caller-supplied reusable `ureq::Agent`.
/// An empty URL means notifications are off.
pub fn send_with_agent(
    agent: &ureq::Agent,
    url: &str,
    event: &WebhookEvent,
    opts: &WebhookOptions,
) -> Result<(), WebhookError> {
    let url = url.trim();
    if url.is_empty() {
        return Ok(());
    }
    validate_url(url)?;
    // Checked here so a hostile id cannot smuggle a second header line.
    if event.event_id.is_empty() {
        return Err(WebhookError::InvalidEvent("event_id is empty"));
    }
    if !event.event_id.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        return Err(WebhookError::InvalidEvent(
            "event_id must be printable ASCII",
        ));
    }
    let body = serde_json::to_string(event).map_err(|e| WebhookError::Serialize(e.to_string()))?;

    let retries = opts.max_retries.min(MAX_RETRIES);
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        let outcome = agent
            .post(url)
            .header("Content-Type", "application/json")
            .header("Idempotency-Key", event.event_id.as_str())
            .send(body.as_str());
        let (error, retryable) = match outcome {
            Ok(response) => {
                let status = response.status().as_u16();
                if (200..300).contains(&status) {
                    return Ok(());
                }
                let error = WebhookError::Status {
                    url: mask_url(url),
                    status,
                    attempts: attempt,
                };
                (error, status == 429 || status >= 500)
            }
            Err(e) => {
                let (detail, retryable) = describe(&e);
                let error = WebhookError::Transport {
                    url: mask_url(url),
                    detail,
                    attempts: attempt,
                };
                (error, retryable)
            }
        };
        if !retryable || attempt > retries {
            return Err(error);
        }
        (opts.sleeper)(backoff(opts.base_backoff, attempt));
    }
}

fn validate_url(url: &str) -> Result<(), WebhookError> {
    let invalid = |reason| WebhookError::InvalidUrl {
        url: mask_url(url),
        reason,
    };
    let uri: ureq::http::Uri = url.parse().map_err(|_| invalid("not a valid URL"))?;
    if !matches!(uri.scheme_str(), Some("http" | "https")) {
        return Err(invalid("only http and https are allowed"));
    }
    if uri.host().is_none_or(str::is_empty) {
        return Err(invalid("missing host"));
    }
    Ok(())
}

/// A description built only from our own words, because `ureq`'s messages may
/// quote the URL. The flag says whether the failure is worth a retry: only
/// failures to reach the server are.
fn describe(error: &ureq::Error) -> (String, bool) {
    match error {
        ureq::Error::Io(e) => (format!("i/o error ({:?})", e.kind()), true),
        ureq::Error::Timeout(t) => (format!("timed out ({t})"), true),
        ureq::Error::HostNotFound => ("host not found".to_string(), true),
        ureq::Error::ConnectionFailed => ("connection failed".to_string(), true),
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) => ("TLS error".to_string(), false),
        ureq::Error::Protocol(_) => ("malformed HTTP response".to_string(), false),
        _ => ("request could not be sent".to_string(), false),
    }
}

/// `base * 2^(retry - 1)` plus up to half of that again.
fn backoff(base: Duration, retry: u32) -> Duration {
    let step = base.saturating_mul(1_u32 << (retry - 1).min(16));
    let half_ms = u64::try_from(step.as_millis() / 2).unwrap_or(u64::MAX);
    // `RandomState` is seeded per instance by the OS: enough spread to keep
    // retrying clients apart without a random-number dependency.
    let roll = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    step.saturating_add(Duration::from_millis(roll % (half_ms + 1)))
}
