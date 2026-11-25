//! Outbound-only Telegram Bot API HTTP client (`ureq`, zero listening ports).

use std::collections::HashSet;
use std::fmt;
use std::sync::OnceLock;
use std::time::Duration;

use aaf_domain::telegram::{MAX_POLL_TIMEOUT_SECS, TelegramConfig};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::webhook::mask_url;

/// Safe ceiling in UTF-8 bytes for outgoing Telegram messages (API limit is 4096 chars).
pub const MAX_MESSAGE_BYTES: usize = 4000;
/// Default HTTP connect timeout.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Default HTTP whole-request timeout for non-polling calls (`sendMessage`).
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Hard cap on bytes read from a single Bot API response (2 MiB).
const MAX_RESPONSE_BYTES: u64 = 2 * 1024 * 1024;
/// Updates per `getUpdates` batch: keeps a full batch of maximal messages far
/// below [`MAX_RESPONSE_BYTES`] so a backlog can always drain.
const GET_UPDATES_LIMIT: u64 = 20;
/// Extra headroom added on top of Telegram's server-side long-poll timeout.
const POLL_TIMEOUT_HEADROOM_SECS: u64 = 15;

/// Errors produced by the Telegram companion.
///
/// Security invariant: neither `Display` nor `Debug` may ever contain a bot token.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TelegramError {
    #[error("telegram companion is disabled in Tier-1 config (telegram.enabled = false)")]
    Disabled,
    #[error("telegram allowed_chat_ids is empty (fail-closed)")]
    EmptyAllowlist,
    #[error("telegram bot token environment variable {env_var} is missing or empty")]
    MissingToken { env_var: String },
    #[error("telegram bot token has invalid format")]
    InvalidToken,
    #[error("invalid telegram api_base_url {url}: {reason}")]
    InvalidBaseUrl { url: String, reason: &'static str },
    #[error("telegram request serialization failed: {0}")]
    Serialize(String),
    #[error("telegram request to {url} failed: {detail}")]
    Transport { url: String, detail: String },
    #[error("telegram endpoint {url} returned HTTP {status}")]
    HttpStatus { url: String, status: u16 },
    #[error("telegram {method} API error ({code:?}): {description}")]
    ApiError {
        method: &'static str,
        code: Option<i64>,
        description: String,
    },
    #[error("telegram {method} response decode failed: {detail}")]
    ResponseDecode {
        method: &'static str,
        detail: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<Message>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub message_id: i64,
    #[serde(default)]
    pub from: Option<User>,
    pub chat: Chat,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub date: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: i64,
    #[serde(default)]
    pub is_bot: bool,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub first_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chat {
    pub id: i64,
    #[serde(rename = "type", default)]
    pub chat_type: String,
}

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    #[serde(default)]
    error_code: Option<i64>,
    #[serde(default)]
    description: Option<String>,
}

/// Narrow port for Telegram Bot API calls so tests use deterministic mocks.
pub trait TelegramApi: Send + Sync {
    fn get_updates(
        &self,
        offset: Option<i64>,
        timeout_secs: u64,
    ) -> Result<Vec<Update>, TelegramError>;
    fn send_message(&self, chat_id: i64, text: &str) -> Result<(), TelegramError>;
}

/// Synchronous outbound-only HTTP client for the Telegram Bot API.
pub struct HttpTelegramApi {
    base_url: String,
    token: String,
    connect_timeout: Duration,
    request_timeout: Duration,
    agent: ureq::Agent,
}

impl fmt::Debug for HttpTelegramApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpTelegramApi")
            .field("base_url", &mask_url(&self.base_url))
            .field("token", &"[REDACTED]")
            .field("connect_timeout", &self.connect_timeout)
            .field("request_timeout", &self.request_timeout)
            .finish()
    }
}

impl HttpTelegramApi {
    pub fn new(api_base_url: &str, token: impl Into<String>) -> Result<Self, TelegramError> {
        Self::with_timeouts(
            api_base_url,
            token,
            DEFAULT_CONNECT_TIMEOUT,
            DEFAULT_REQUEST_TIMEOUT,
        )
    }

    pub fn with_timeouts(
        api_base_url: &str,
        token: impl Into<String>,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self, TelegramError> {
        let base_url = validate_base_url(api_base_url)?;
        let token = token.into();
        validate_token(&token)?;
        let agent = ureq::Agent::config_builder()
            .timeout_connect(Some(connect_timeout))
            .timeout_global(Some(request_timeout))
            .http_status_as_error(false)
            .max_redirects(0)
            .max_redirects_will_error(false)
            .build()
            .new_agent();
        Ok(Self {
            base_url,
            token,
            connect_timeout,
            request_timeout,
            agent,
        })
    }

    fn post_json<T: for<'de> Deserialize<'de>>(
        &self,
        method: &'static str,
        payload: &serde_json::Value,
        global_timeout: Duration,
    ) -> Result<T, TelegramError> {
        let url = format!("{}/bot{}/{method}", self.base_url, self.token);
        let masked = mask_url(&url);
        let body =
            serde_json::to_string(payload).map_err(|e| TelegramError::Serialize(e.to_string()))?;

        let response = self
            .agent
            .post(&url)
            .config()
            .timeout_global(Some(global_timeout))
            .build()
            .header("Content-Type", "application/json")
            .send(body.as_str())
            .map_err(|err| TelegramError::Transport {
                url: masked.clone(),
                detail: describe_ureq_error(&err),
            })?;

        let status = response.status().as_u16();
        let raw_body = response
            .into_body()
            .with_config()
            .limit(MAX_RESPONSE_BYTES)
            .read_to_string()
            .map_err(|err| match err {
                // Re-polling returns the same oversized batch: permanent, not transient.
                ureq::Error::BodyExceedsLimit(_) => TelegramError::ResponseDecode {
                    method,
                    detail: format!("response body exceeds {MAX_RESPONSE_BYTES} bytes"),
                },
                other => TelegramError::Transport {
                    url: masked.clone(),
                    detail: describe_ureq_error(&other),
                },
            })?;

        if !(200..300).contains(&status) {
            if let Ok(api_err) = serde_json::from_str::<ApiResponse<serde_json::Value>>(&raw_body)
                && !api_err.ok
            {
                let desc = api_err
                    .description
                    .unwrap_or_else(|| format!("HTTP {status}"));
                return Err(TelegramError::ApiError {
                    method,
                    code: api_err.error_code.or(Some(i64::from(status))),
                    description: redact_bot_token(&desc, &self.token),
                });
            }
            return Err(TelegramError::HttpStatus {
                url: masked,
                status,
            });
        }

        let parsed: ApiResponse<T> =
            serde_json::from_str(&raw_body).map_err(|err| TelegramError::ResponseDecode {
                method,
                detail: redact_bot_token(&err.to_string(), &self.token),
            })?;
        if !parsed.ok {
            let desc = parsed
                .description
                .unwrap_or_else(|| "unknown error".to_string());
            return Err(TelegramError::ApiError {
                method,
                code: parsed.error_code,
                description: redact_bot_token(&desc, &self.token),
            });
        }
        parsed.result.ok_or_else(|| TelegramError::ResponseDecode {
            method,
            detail: "missing result field in ok response".to_string(),
        })
    }
}

impl TelegramApi for HttpTelegramApi {
    fn get_updates(
        &self,
        offset: Option<i64>,
        timeout_secs: u64,
    ) -> Result<Vec<Update>, TelegramError> {
        let clamped = timeout_secs.min(MAX_POLL_TIMEOUT_SECS);
        let mut payload = json!({
            "timeout": clamped,
            "limit": GET_UPDATES_LIMIT,
            "allowed_updates": ["message"],
        });
        if let Some(off) = offset {
            payload["offset"] = json!(off);
        }
        let global_timeout = self.request_timeout.max(Duration::from_secs(
            clamped.saturating_add(POLL_TIMEOUT_HEADROOM_SECS),
        ));
        self.post_json("getUpdates", &payload, global_timeout)
    }

    fn send_message(&self, chat_id: i64, text: &str) -> Result<(), TelegramError> {
        let safe = redact_bot_token(text, &self.token);
        let bounded = truncate_message(&safe, MAX_MESSAGE_BYTES);
        let payload = json!({
            "chat_id": chat_id,
            "text": bounded,
            "disable_web_page_preview": true,
        });
        let _: serde_json::Value = self.post_json("sendMessage", &payload, self.request_timeout)?;
        Ok(())
    }
}

/// Validates and normalizes `api_base_url` (`http` or `https` only, no userinfo/query/fragment).
pub fn validate_base_url(raw: &str) -> Result<String, TelegramError> {
    let trimmed = raw.trim().trim_end_matches('/');
    let invalid = |reason| TelegramError::InvalidBaseUrl {
        url: mask_url(raw),
        reason,
    };
    if trimmed.is_empty() {
        return Err(invalid("empty URL"));
    }
    let uri: ureq::http::Uri = trimmed.parse().map_err(|_| invalid("not a valid URL"))?;
    if !matches!(uri.scheme_str(), Some("http" | "https")) {
        return Err(invalid("only http and https are allowed"));
    }
    if uri.host().is_none_or(str::is_empty) {
        return Err(invalid("missing host"));
    }
    if uri.authority().is_some_and(|a| a.as_str().contains('@')) {
        return Err(invalid("userinfo is not allowed in api_base_url"));
    }
    if uri.query().is_some() || trimmed.contains('#') {
        return Err(invalid(
            "query and fragment are not allowed in api_base_url",
        ));
    }
    Ok(trimmed.to_string())
}

/// Validates a Telegram bot token so it cannot inject path segments or headers.
pub fn validate_token(token: &str) -> Result<(), TelegramError> {
    if token.is_empty() || token.len() > 256 || token.trim() != token {
        return Err(TelegramError::InvalidToken);
    }
    let valid_chars = token
        .bytes()
        .all(|b| b.is_ascii_graphic() && !matches!(b, b'/' | b'?' | b'#' | b'@' | b'\\'));
    if !valid_chars {
        return Err(TelegramError::InvalidToken);
    }
    Ok(())
}

/// Strips both a known `token` literal and any `/bot<token>` URL segments from text.
pub fn redact_bot_token(input: &str, token: &str) -> String {
    static BOT_SEG_RE: OnceLock<Option<Regex>> = OnceLock::new();
    let mut out = if !token.is_empty() {
        input.replace(token, "[REDACTED]")
    } else {
        input.to_string()
    };
    if let Some(re) = BOT_SEG_RE
        .get_or_init(|| Regex::new(r"/bot[^/\s?#]+").ok())
        .as_ref()
    {
        out = re.replace_all(&out, "/bot[REDACTED]").into_owned();
    }
    out
}

/// Truncates `text` cleanly on a UTF-8 character boundary to at most `max_bytes`.
pub fn truncate_message(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    const SUFFIX: &str = "\n...(truncated)";
    if max_bytes <= SUFFIX.len() {
        let mut end = max_bytes.min(text.len());
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        return text[..end].to_string();
    }
    let mut end = (max_bytes - SUFFIX.len()).min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + SUFFIX.len());
    out.push_str(&text[..end]);
    out.push_str(SUFFIX);
    out
}

/// Sends a one-shot alert to every chat in `cfg.allowed_chat_ids`.
pub fn send_alert(cfg: &TelegramConfig, token: &str, text: &str) -> Result<(), TelegramError> {
    if !cfg.enabled {
        return Err(TelegramError::Disabled);
    }
    if cfg.allowed_chat_ids.is_empty() {
        return Err(TelegramError::EmptyAllowlist);
    }
    let api = HttpTelegramApi::new(&cfg.api_base_url, token)?;
    let redacted = redact_bot_token(text, token);
    send_alert_with(&api, cfg, &redacted)
}

/// Testable alert sender using an injected [`TelegramApi`].
pub fn send_alert_with(
    api: &dyn TelegramApi,
    cfg: &TelegramConfig,
    text: &str,
) -> Result<(), TelegramError> {
    if !cfg.enabled {
        return Err(TelegramError::Disabled);
    }
    if cfg.allowed_chat_ids.is_empty() {
        return Err(TelegramError::EmptyAllowlist);
    }
    let sanitized = super::commands::sanitize_output(text);
    let mut seen = HashSet::new();
    let mut first_err: Option<TelegramError> = None;
    for &chat_id in &cfg.allowed_chat_ids {
        if !seen.insert(chat_id) {
            continue;
        }
        if let Err(err) = api.send_message(chat_id, &sanitized)
            && first_err.is_none()
        {
            first_err = Some(err);
        }
    }
    match first_err {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Per-chat delivery result returned by [`send_alert_each`] and [`send_alert_each_with`].
pub type ChatSendResult = (i64, Result<(), TelegramError>);

/// Sends a one-shot alert to each chat in `chat_ids` that is present in
/// `cfg.allowed_chat_ids`, returning per-chat outcomes.
///
/// Configuration or client setup errors (`Disabled`, `EmptyAllowlist`, invalid
/// base URL or token) fail fast at the outer `Result`.
pub fn send_alert_each(
    cfg: &TelegramConfig,
    token: &str,
    text: &str,
    chat_ids: &[i64],
) -> Result<Vec<ChatSendResult>, TelegramError> {
    if !cfg.enabled {
        return Err(TelegramError::Disabled);
    }
    if cfg.allowed_chat_ids.is_empty() {
        return Err(TelegramError::EmptyAllowlist);
    }
    let api = HttpTelegramApi::new(&cfg.api_base_url, token)?;
    let redacted = redact_bot_token(text, token);
    send_alert_each_with(&api, cfg, &redacted, chat_ids)
}

/// Testable per-chat alert sender using an injected [`TelegramApi`].
///
/// Chat ids outside `cfg.allowed_chat_ids` are silently skipped, and duplicate
/// ids in `chat_ids` are sent at most once.
pub fn send_alert_each_with(
    api: &dyn TelegramApi,
    cfg: &TelegramConfig,
    text: &str,
    chat_ids: &[i64],
) -> Result<Vec<ChatSendResult>, TelegramError> {
    if !cfg.enabled {
        return Err(TelegramError::Disabled);
    }
    if cfg.allowed_chat_ids.is_empty() {
        return Err(TelegramError::EmptyAllowlist);
    }
    let allowed: HashSet<i64> = cfg.allowed_chat_ids.iter().copied().collect();
    let sanitized = super::commands::sanitize_output(text);
    let mut seen = HashSet::new();
    let mut outcomes = Vec::new();
    for &chat_id in chat_ids {
        if !allowed.contains(&chat_id) || !seen.insert(chat_id) {
            continue;
        }
        let outcome = api.send_message(chat_id, &sanitized);
        outcomes.push((chat_id, outcome));
    }
    Ok(outcomes)
}

/// Own-words transport error description so `ureq` never leaks the URL/token.
fn describe_ureq_error(error: &ureq::Error) -> String {
    match error {
        ureq::Error::Io(e) => format!("i/o error ({:?})", e.kind()),
        ureq::Error::Timeout(t) => format!("timed out ({t})"),
        ureq::Error::HostNotFound => "host not found".to_string(),
        ureq::Error::ConnectionFailed => "connection failed".to_string(),
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) => "TLS error".to_string(),
        ureq::Error::Protocol(_) => "malformed HTTP response".to_string(),
        ureq::Error::BodyStalled => "response body stalled".to_string(),
        ureq::Error::RedirectFailed => "redirect refused".to_string(),
        _ => "request failed".to_string(),
    }
}
