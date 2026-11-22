//! Runs Claude Code headless and reads its `stream-json` output.
//!
//! Flags and event shapes were checked against `claude --help` and a real run
//! of Claude Code 2.1.330 (fixtures in `tests/fixtures/exec`).

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use aaf_domain::account::ProviderKind;
use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, InfraFailure, InfraKind, LimitHit, PortError,
    PortResult,
};
use serde_json::Value;

use super::pty::CommandSpec;
use super::{AccountPaths, ExecEnv, run_confined};

/// Longest provider message kept in [`LimitHit::raw`].
const RAW_LIMIT_CHARS: usize = 500;

pub struct ClaudeCodeRunner {
    bin: String,
    env: ExecEnv,
    model: String,
    effort: String,
}

/// A value for `--model` or `--effort` from the repo config: one word that
/// cannot be read as another flag.
fn flag_value(key: &str, value: &str) -> PortResult<()> {
    if value.starts_with('-') || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(PortError::Invalid(format!(
            "run.{key} = {value:?} is not a single word"
        )));
    }
    Ok(())
}

impl ClaudeCodeRunner {
    /// `bin` is the absolute path of the `claude` binary.
    pub fn new(bin: impl Into<String>, env: ExecEnv) -> Self {
        ClaudeCodeRunner {
            bin: bin.into(),
            env,
            model: String::new(),
            effort: String::new(),
        }
    }

    /// `run.model` and `run.effort`: passed as `--model` and `--effort`
    /// (both in `claude --help` of 2.1.330). Empty leaves the flag out.
    #[must_use]
    pub fn with_model_and_effort(mut self, model: &str, effort: &str) -> Self {
        self.model = model.to_string();
        self.effort = effort.to_string();
        self
    }

    /// The exact command a run of `request` executes (before sandboxing).
    pub fn command(&self, request: &AgentRequest) -> PortResult<CommandSpec> {
        if request.prompt.is_empty() {
            return Err(PortError::Invalid("empty prompt".to_string()));
        }
        let mut args: Vec<String> = ["-p", "--output-format", "stream-json", "--verbose"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        // Nobody can answer a permission prompt in an unattended run, so the
        // choice is between granting up front and denying automatically
        // (`claude --help`, 2.1.330):
        // - Sandboxed: `--permission-mode bypassPermissions`. The agent must
        //   edit files and run the project's build and test commands, which no
        //   static allow-list can enumerate; the Seatbelt profile and the
        //   aaf-hook guard are what confine it.
        // - Unsandboxed (explicit opt-in): `acceptEdits` lets it edit files,
        //   and `--permission-prompts none` turns everything that would still
        //   prompt (shell commands outside the user's own allow rules) into
        //   an automatic denial instead of a hang. Bypassing all checks with
        //   no OS confinement behind it is never chosen here.
        if request.sandbox.enabled {
            args.extend(["--permission-mode", "bypassPermissions"].map(String::from));
        } else {
            args.extend(
                [
                    "--permission-mode",
                    "acceptEdits",
                    "--permission-prompts",
                    "none",
                ]
                .map(String::from),
            );
        }
        for (flag, key, value) in [
            ("--model", "model", &self.model),
            ("--effort", "effort", &self.effort),
        ] {
            if !value.is_empty() {
                flag_value(key, value)?;
                args.extend([flag.to_string(), value.clone()]);
            }
        }
        // After `--` so a prompt that starts with a dash is still the prompt.
        args.push("--".to_string());
        args.push(request.prompt.clone());

        let mut env = Vec::new();
        if let Some(account) = &request.account {
            if account.provider != ProviderKind::ClaudeCode {
                return Err(PortError::Invalid(format!(
                    "account {} is not a Claude Code account",
                    account.id
                )));
            }
            env.push(("CLAUDE_CONFIG_DIR".to_string(), account.config_dir.clone()));
        }
        Ok(CommandSpec {
            program: self.bin.clone(),
            args,
            cwd: None,
            env,
            env_clear: false,
        })
    }
}

/// What Claude Code writes below its config dir while it runs a session.
const SESSION_STATE: [&str; 8] = [
    "projects",
    "sessions",
    "session-env",
    "file-history",
    "history.jsonl",
    "paste-cache",
    "shell-snapshots",
    "todos",
];

/// Entries of a Claude config dir that every later session of that account
/// runs (hooks) or obeys (settings: hooks, permissions, env), also outside
/// the sandbox. Closed as directory entries too, so a link into a shared
/// `~/.claude` cannot be swapped for a file of the agent's own.
/// `.claude.json` (it holds MCP server commands) is NOT here: Claude Code
/// rewrites it in every session, and whether a run survives it being
/// read-only has not been tried with a real session.
const CONFIG_CLOSED: [&str; 3] = ["settings.json", "settings.local.json", "hooks"];

fn closed_entries(config_dir: &str) -> Vec<String> {
    CONFIG_CLOSED
        .iter()
        .map(|name| format!("{config_dir}/{name}"))
        .collect()
}

/// The account's config dir plus where its session state really lives.
///
/// A profile directory often links these entries into one shared
/// `~/.claude`. The sandbox matches resolved paths, so with only the profile
/// directory open the transcript of a run was silently never written (seen
/// with a real account). Only the session-state entries are followed:
/// `settings.json`, `hooks`, `skills` and the like stay closed when they are
/// links to a shared place, because the user's own sessions obey them.
fn account_state_paths(config_dir: &str) -> Vec<String> {
    let mut paths = vec![config_dir.to_string()];
    for name in SESSION_STATE {
        let entry = Path::new(config_dir).join(name);
        let is_link = fs::symlink_metadata(&entry).is_ok_and(|m| m.file_type().is_symlink());
        // A dangling link has no place to write to; the run goes without it.
        if is_link && let Ok(target) = fs::canonicalize(&entry) {
            paths.push(target.to_string_lossy().into_owned());
        }
    }
    paths
}

impl AgentRunner for ClaudeCodeRunner {
    fn run(&self, request: &AgentRequest) -> PortResult<AgentOutcome> {
        let spec = self.command(request)?;
        // Where Claude Code keeps its own state: the account's directory, or
        // the default location when no account was assigned.
        let account = match (&request.account, &self.env.home) {
            (Some(account), _) => AccountPaths {
                writable: account_state_paths(&account.config_dir),
                write_denied: closed_entries(&account.config_dir),
            },
            (None, Some(home)) => AccountPaths {
                writable: vec![format!("{home}/.claude"), format!("{home}/.claude.json")],
                write_denied: closed_entries(&format!("{home}/.claude")),
            },
            (None, None) => AccountPaths::default(),
        };
        let parser = Arc::new(Mutex::new(StreamParser::default()));
        let feed = Arc::clone(&parser);
        let output = run_confined(
            spec,
            request,
            &self.env,
            account,
            Some(Box::new(move |line| {
                feed.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .feed_line(line);
            })),
        )?;
        let summary = parser
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .summary();
        Ok(AgentOutcome {
            exit_code: output.exit_code,
            timed_out: output.timed_out,
            final_text: summary.final_text,
            tokens_in: summary.tokens_in,
            tokens_out: summary.tokens_out,
            limit_hit: summary.limit_hit,
            // A run that timed out was cut off by aaf: whatever it said last
            // is not why it ended.
            infra: summary.infra.filter(|_| !output.timed_out),
            log_tail: output.tail,
        })
    }
}

/// What one `stream-json` transcript amounts to.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StreamSummary {
    /// Text of the `result` event, or of the last assistant message when the
    /// run ended before one was written.
    pub final_text: String,
    /// input + cache-creation tokens; cache reads are not counted.
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub limit_hit: Option<LimitHit>,
    /// The run failed because of the account or the network.
    pub infra: Option<InfraFailure>,
    /// The `result` event reported an error.
    pub is_error: bool,
    pub saw_result: bool,
}

/// Incremental parser for `claude --output-format stream-json`.
///
/// Every line is untrusted: anything that is not a JSON object of a known
/// type is skipped (the raw text stays in the run's log tail).
#[derive(Debug, Default)]
pub struct StreamParser {
    /// Usage per assistant message id. One message is streamed as several
    /// events repeating the same usage, so the last one per id wins.
    assistant_usage: HashMap<String, (u64, u64)>,
    last_assistant_text: String,
    result: Option<ResultEvent>,
    /// An assistant message flagged `"error":"rate_limit"`.
    rate_limit_error: Option<String>,
    /// The last `rate_limit_event` whose status was `rejected`.
    rejected: Option<(Option<i64>, String)>,
    /// What the last API error message of the CLI says about the account
    /// or the network; `None` when that error was of another kind.
    api_error: Option<InfraFailure>,
}

#[derive(Debug)]
struct ResultEvent {
    text: String,
    is_error: bool,
    api_error_status: Option<i64>,
    tokens: (u64, u64),
}

impl StreamParser {
    pub fn feed_line(&mut self, line: &str) {
        let Ok(Value::Object(event)) = serde_json::from_str::<Value>(line.trim()) else {
            return;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                let message = event.get("message");
                let text = message.map(message_text).unwrap_or_default();
                let error = event.get("error").and_then(Value::as_str);
                if error == Some("rate_limit") {
                    self.rate_limit_error = Some(text.clone());
                }
                let flagged = event.get("is_api_error_message").and_then(Value::as_bool);
                if error.is_some() || flagged == Some(true) {
                    self.api_error =
                        infra_kind(error, flagged == Some(true), &text).map(|kind| InfraFailure {
                            kind,
                            raw: text.chars().take(RAW_LIMIT_CHARS).collect(),
                        });
                }
                if let Some(usage) = message.and_then(|m| m.get("usage")) {
                    let id = message
                        .and_then(|m| m.get("id"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("#{}", self.assistant_usage.len()));
                    self.assistant_usage.insert(id, usage_tokens(usage));
                }
                if !text.is_empty() {
                    self.last_assistant_text = text;
                }
            }
            Some("result") => {
                self.result = Some(ResultEvent {
                    text: event
                        .get("result")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    is_error: event
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    api_error_status: event.get("api_error_status").and_then(Value::as_i64),
                    tokens: event.get("usage").map(usage_tokens).unwrap_or_default(),
                });
            }
            Some("rate_limit_event") => {
                let info = event.get("rate_limit_info");
                let status = info.and_then(|i| i.get("status")).and_then(Value::as_str);
                if status == Some("rejected") {
                    let resets_at = info.and_then(|i| i.get("resetsAt")).and_then(Value::as_i64);
                    self.rejected = Some((resets_at, line.trim().to_string()));
                }
            }
            _ => {}
        }
    }

    pub fn summary(&self) -> StreamSummary {
        // The result event carries the totals of the whole run. Without one
        // (timeout, crash) the per-message numbers are all there is.
        let (tokens_in, tokens_out) = match &self.result {
            Some(result) => result.tokens,
            None => self
                .assistant_usage
                .values()
                .fold((0u64, 0u64), |acc, usage| {
                    (acc.0.saturating_add(usage.0), acc.1.saturating_add(usage.1))
                }),
        };
        let final_text = match &self.result {
            Some(result) => result.text.clone(),
            None => self.last_assistant_text.clone(),
        };
        StreamSummary {
            final_text,
            tokens_in,
            tokens_out,
            limit_hit: self.limit_hit(),
            infra: self.infra(),
            is_error: self.result.as_ref().is_some_and(|r| r.is_error),
            saw_result: self.result.is_some(),
        }
    }

    fn limit_hit(&self) -> Option<LimitHit> {
        // A run that produced a successful result was served, whatever the
        // rate-limit telemetry said on the way (a window can report
        // `rejected` while paid overage still covers the request).
        if self.result.as_ref().is_some_and(|r| !r.is_error) {
            return None;
        }
        let failed_text = self
            .result
            .as_ref()
            .map(|r| r.text.as_str())
            .unwrap_or(&self.last_assistant_text);
        let message = if let Some(text) = &self.rate_limit_error {
            text.clone()
        } else if self.result.as_ref().and_then(|r| r.api_error_status) == Some(429)
            || is_limit_wording(failed_text)
        {
            failed_text.to_string()
        } else if let Some((_, raw)) = &self.rejected {
            raw.clone()
        } else {
            return None;
        };
        let reset_at = self
            .rejected
            .as_ref()
            .and_then(|(resets_at, _)| *resets_at)
            .or_else(|| reset_from_text(&message));
        Some(LimitHit {
            reset_at,
            raw: message.chars().take(RAW_LIMIT_CHARS).collect(),
        })
    }

    /// A failed run whose last API error was about the account or the
    /// network, or whose result carries HTTP 401/403. A run that was served
    /// in the end, or that hit a limit (handled as a limit), is none.
    fn infra(&self) -> Option<InfraFailure> {
        if self.result.as_ref().is_some_and(|r| !r.is_error) || self.limit_hit().is_some() {
            return None;
        }
        if let Some(failure) = &self.api_error {
            return Some(failure.clone());
        }
        let result = self.result.as_ref()?;
        matches!(result.api_error_status, Some(401 | 403)).then(|| InfraFailure {
            kind: InfraKind::Auth,
            raw: result.text.chars().take(RAW_LIMIT_CHARS).collect(),
        })
    }
}

/// Values of the `error` field of an API error message that mean the
/// provider refused the account itself. From the error enum of the Claude
/// Code 2.1.330 binary; `authentication_failed` is also in a real run
/// (`tests/fixtures/exec/claude_not_logged_in.stream.jsonl`). The other
/// values of that enum (`rate_limit`, `overloaded`, `server_error`,
/// `invalid_request`, `model_not_found`, `max_output_tokens`, `unknown`)
/// are not taken as trouble with the account.
const ACCOUNT_ERRORS: [&str; 6] = [
    "authentication_failed",
    "oauth_org_not_allowed",
    "account_on_hold",
    "verification_required",
    "billing_error",
    "cloud_credential_error",
];

/// How Claude Code 2.1.330 words a request that never reached the API
/// (read from the binary's connection-error messages; never observed
/// live). Matched only in a message the CLI flags as its own API error,
/// never in the agent's words.
const NETWORK_WORDING: [&str; 6] = [
    "unable to connect to api",
    "can't reach the api server",
    "no internet route",
    "a firewall or proxy may be blocking it",
    "couldn't connect through your proxy",
    "connection dropped",
];

fn infra_kind(error: Option<&str>, flagged: bool, text: &str) -> Option<InfraKind> {
    if error.is_some_and(|error| ACCOUNT_ERRORS.contains(&error)) {
        return Some(InfraKind::Auth);
    }
    let lower = text.to_lowercase();
    (flagged && NETWORK_WORDING.iter().any(|words| lower.contains(words)))
        .then_some(InfraKind::Network)
}

/// Parses a whole transcript.
pub fn parse_stream(text: &str) -> StreamSummary {
    let mut parser = StreamParser::default();
    for line in text.lines() {
        parser.feed_line(line);
    }
    parser.summary()
}

/// `(input + cache creation, output)` of a `usage` object.
fn usage_tokens(usage: &Value) -> (u64, u64) {
    let field = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
    (
        field("input_tokens").saturating_add(field("cache_creation_input_tokens")),
        field("output_tokens"),
    )
}

fn message_text(message: &Value) -> String {
    message
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// Wording Claude Code 2.1.330 uses for exhausted usage (read from the
/// binary's message table; never observed live).
fn is_limit_wording(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("usage limit reached")
        || lower.contains("out of usage credits")
        || (lower.contains("hit your") && lower.contains("limit"))
}

/// Older releases appended the reset time as `...|<unix seconds>`.
fn reset_from_text(text: &str) -> Option<i64> {
    let (_, tail) = text.rsplit_once('|')?;
    let digits: String = tail
        .trim()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    // Ten digits: a plausible Unix time in seconds, not some other number.
    (digits.len() == 10).then(|| digits.parse().ok()).flatten()
}
