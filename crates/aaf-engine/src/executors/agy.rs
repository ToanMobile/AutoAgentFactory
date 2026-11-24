//! Native headless executor for Antigravity CLI (`agy` / `antigravity`) with
//! JSON output, token accounting, and least-privilege `~/.gemini` confinement.
//!
//! Flags and JSON output shape were verified against `agy` v1.5.15.

use std::ffi::OsStr;
use std::path::PathBuf;

use aaf_domain::account::ProviderKind;
use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, InfraFailure, LimitHit, PortError, PortResult,
};
use serde_json::{Map, Value};

use super::pty::{CommandSpec, GENERIC_TAIL_BYTES};
#[cfg(not(target_os = "linux"))]
use super::sandbox;
use super::{AccountPaths, ExecEnv, find_on_path, open_stream_log, run_confined_with_tail};

const RAW_LIMIT_CHARS: usize = 500;

#[path = "agy_classify.rs"]
mod classify;
use classify::classify_text_failure;

/// Safe upper bound on a single argv element on Linux (`MAX_ARG_STRLEN` = 128 KiB).
pub const LINUX_MAX_PROMPT_BYTES: usize = 100_000;
const VALID_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Entries under `~/.gemini` that stay closed for writing in every sandboxed
/// Antigravity or Gemini CLI run (shared with [`super::generic::GenericRunner`]).
pub const GEMINI_CONFIG_CLOSED: &[&str] = &[
    "settings.json",
    "trusted_hooks.json",
    "trustedFolders.json",
    "GEMINI.md",
    "oauth_creds.json",
    "google_accounts.json",
    "projects.json",
    "config",
    "skills",
    "agents",
    "antigravity-cli",
    "antigravity-ide",
    "antigravity/mcp",
    "antigravity/builtin",
    "antigravity/bin",
    "antigravity/lib",
    "antigravity/daemon",
    "antigravity/plugin_data",
    "antigravity/knowledge",
];

/// Explicit runtime subdirs under `~/.gemini` that `agy` writes to (no glob `*`).
pub const AGY_RUNTIME_WRITABLE_SUBDIRS: &[&str] = &[
    "antigravity/conversations",
    "antigravity/brain",
    "antigravity/presence",
    "antigravity/annotations",
    "antigravity/cache",
    "antigravity/tmp",
];

/// Returns absolute write-denied paths for every entry in [`GEMINI_CONFIG_CLOSED`].
pub fn closed_entries(gemini_root: &str) -> Vec<String> {
    let root = gemini_root.trim_end_matches('/');
    GEMINI_CONFIG_CLOSED
        .iter()
        .map(|entry| format!("{root}/{entry}"))
        .collect()
}

/// Native executor for Antigravity CLI (`agy` / `antigravity`).
#[derive(Debug, Clone)]
pub struct AgyRunner {
    bin: String,
    env: ExecEnv,
    model: Option<String>,
    effort: Option<String>,
}

fn validate_model(value: &str) -> PortResult<()> {
    if value.is_empty()
        || value.starts_with('-')
        || value.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(PortError::Invalid(format!(
            "agy model = {value:?} must be a single non-empty token not starting with '-'"
        )));
    }
    Ok(())
}

fn validate_effort(value: &str) -> PortResult<String> {
    let lower = value.to_ascii_lowercase();
    if value.chars().any(|c| c.is_whitespace() || c.is_control())
        || !VALID_EFFORTS.contains(&lower.as_str())
    {
        return Err(PortError::Invalid(format!(
            "agy effort = {value:?} must be one of: low, medium, high, xhigh, max"
        )));
    }
    Ok(lower)
}

fn validate_account(request: &AgentRequest) -> PortResult<()> {
    if let Some(acc) = &request.account
        && acc.provider != ProviderKind::Antigravity
    {
        return Err(PortError::Invalid(format!(
            "account {} is not an Antigravity account",
            acc.id
        )));
    }
    Ok(())
}

impl AgyRunner {
    /// `bin` is the path to the `agy` (or `antigravity`) binary.
    pub fn new(bin: impl Into<String>, env: ExecEnv) -> Self {
        Self {
            bin: bin.into(),
            env,
            model: None,
            effort: None,
        }
    }

    /// Sets optional `--model` and `--effort` flags. Empty strings leave the flag out.
    #[must_use]
    pub fn with_model_and_effort(mut self, model: &str, effort: &str) -> Self {
        self.model = (!model.is_empty()).then(|| model.to_string());
        self.effort = (!effort.is_empty()).then(|| effort.to_string());
        self
    }

    /// Resolves `agy` on `path_var`, falling back to `antigravity`.
    pub fn resolve_binary(path_var: &OsStr) -> Option<PathBuf> {
        find_on_path(path_var, "agy").or_else(|| find_on_path(path_var, "antigravity"))
    }

    /// The exact command executed for `request` (before sandboxing).
    pub fn command(&self, request: &AgentRequest) -> PortResult<CommandSpec> {
        if request.prompt.is_empty() {
            return Err(PortError::Invalid("empty prompt".to_string()));
        }
        validate_account(request)?;
        if cfg!(target_os = "linux") && request.prompt.len() > LINUX_MAX_PROMPT_BYTES {
            return Err(PortError::Invalid(
                "prompt exceeds Linux MAX_ARG_STRLEN safe limit for agy -p=".to_string(),
            ));
        }

        let mut args = vec![
            "--output-format".to_string(),
            "json".to_string(),
            "--disable-slash-commands".to_string(),
        ];
        if request.sandbox.read_only {
            args.extend(["--mode", "plan"].map(String::from));
        } else if request.sandbox.enabled {
            args.extend(["--mode", "auto", "--dangerously-skip-permissions"].map(String::from));
        } else {
            args.extend(["--mode", "accept-edits"].map(String::from));
        }
        if let Some(model) = &self.model {
            validate_model(model)?;
            args.extend(["--model".to_string(), model.clone()]);
        }
        if let Some(effort) = &self.effort {
            let norm = validate_effort(effort)?;
            args.extend(["--effort".to_string(), norm]);
        }
        if let Some(schema) = request
            .json_schema
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            args.extend(["--json-schema".to_string(), schema.to_string()]);
        }
        args.push(format!("-p={}", request.prompt));
        Ok(CommandSpec {
            program: self.bin.clone(),
            args,
            cwd: None,
            env: Vec::new(),
            env_clear: false,
        })
    }

    /// Builds least-privilege sandbox paths for `~/.gemini`, pre-creating the 6
    /// runtime subdirectories on both macOS and Linux when sandboxed.
    pub fn account_paths(&self, request: &AgentRequest) -> PortResult<AccountPaths> {
        validate_account(request)?;
        let gemini_root = request
            .account
            .as_ref()
            .map(|a| PathBuf::from(&a.config_dir))
            .or_else(|| {
                self.env
                    .home
                    .as_deref()
                    .filter(|h| !h.trim().is_empty())
                    .map(|h| PathBuf::from(h).join(".gemini"))
            });
        let Some(root) = gemini_root else {
            return Ok(AccountPaths::default());
        };
        let root_str = root.to_string_lossy().into_owned();
        let ag_dir = root.join("antigravity");
        let mut writable = Vec::with_capacity(AGY_RUNTIME_WRITABLE_SUBDIRS.len() + 4);
        for rel in AGY_RUNTIME_WRITABLE_SUBDIRS {
            let dir = root.join(rel);
            if request.sandbox.enabled {
                std::fs::create_dir_all(&dir).map_err(|err| {
                    PortError::Io(format!("create agy runtime dir {}: {err}", dir.display()))
                })?;
            }
            writable.push(dir.to_string_lossy().into_owned());
        }
        #[cfg(target_os = "linux")]
        let writable_regex = {
            const DB_FILES: [&str; 4] = [
                "conversation_summaries.db",
                "conversation_summaries.db-wal",
                "conversation_summaries.db-shm",
                "conversation_summaries.db-journal",
            ];
            if request.sandbox.enabled {
                std::fs::create_dir_all(&ag_dir).map_err(|err| {
                    PortError::Io(format!("create agy dir {}: {err}", ag_dir.display()))
                })?;
                let db_path = ag_dir.join(DB_FILES[0]);
                match std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&db_path)
                {
                    Ok(_) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(err) => {
                        return Err(PortError::Io(format!(
                            "create agy db file {}: {err}",
                            db_path.display()
                        )));
                    }
                }
            }
            for name in DB_FILES {
                let path = ag_dir.join(name);
                if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
                    writable.push(path.to_string_lossy().into_owned());
                }
            }
            Vec::new()
        };
        #[cfg(not(target_os = "linux"))]
        let writable_regex = {
            let ag_str = ag_dir.to_string_lossy();
            let canon_ag = sandbox::canonical(&ag_str)?;
            let canon_ag_str = canon_ag.to_str().ok_or_else(|| {
                PortError::Invalid(format!("gemini path is not UTF-8: {}", canon_ag.display()))
            })?;
            vec![format!(
                "^{}/conversation_summaries\\.db(-wal|-shm|-journal)?$",
                sandbox::regex_escape(canon_ag_str)
            )]
        };
        Ok(AccountPaths {
            writable,
            write_denied: closed_entries(&root_str),
            writable_regex,
        })
    }
}

impl AgentRunner for AgyRunner {
    fn run(&self, request: &AgentRequest) -> PortResult<AgentOutcome> {
        let spec = self.command(request)?;
        let account = self.account_paths(request)?;
        let expect_structured = request
            .json_schema
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty());
        let mut log = open_stream_log(&self.env, request);
        log.write_start("antigravity", self.model.as_deref());
        let out = run_confined_with_tail(
            spec,
            request,
            &self.env,
            account,
            "antigravity",
            None,
            GENERIC_TAIL_BYTES,
        )?;
        let mut parsed = parse_agy_output(&out.tail, expect_structured);
        let should_scan_tail =
            !parsed.usage_present || parsed.is_error || parsed.final_text.is_empty();
        if out.exit_code != Some(0)
            && parsed.limit_hit.is_none()
            && parsed.infra.is_none()
            && should_scan_tail
        {
            let (limit, infra) = classify_text_failure(None, &strip_ansi(&out.tail));
            parsed.limit_hit = limit;
            parsed.infra = infra;
        }
        let exit_code = if parsed.is_error && out.exit_code == Some(0) {
            Some(1)
        } else {
            out.exit_code
        };
        let exit = exit_code.map_or_else(|| "signal".to_string(), |c| c.to_string());
        let finish = format!(
            "exit={exit} timed_out={} tokens_in={} tokens_out={}",
            out.timed_out, parsed.tokens_in, parsed.tokens_out
        );
        log.write_event("finish", Some("antigravity"), Some(&finish));
        Ok(AgentOutcome {
            exit_code,
            timed_out: out.timed_out,
            final_text: parsed.final_text,
            tokens_in: parsed.tokens_in,
            tokens_out: parsed.tokens_out,
            limit_hit: parsed.limit_hit.filter(|_| !out.timed_out),
            infra: parsed.infra.filter(|_| !out.timed_out),
            log_tail: out.tail,
        })
    }

    fn reports_tokens(&self) -> bool {
        true
    }
}

/// Parsed result of `agy --output-format json`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgyParsed {
    pub final_text: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub usage_present: bool,
    pub is_error: bool,
    pub limit_hit: Option<LimitHit>,
    pub infra: Option<InfraFailure>,
}

/// Parses `agy --output-format json` stdout (tolerating ANSI escapes and log lines).
pub fn parse_agy_output(stdout: &str, expect_structured: bool) -> AgyParsed {
    let cleaned = strip_ansi(stdout);
    let Some(obj) = extract_json_object(&cleaned) else {
        let trimmed = cleaned.trim().to_string();
        let (limit_hit, infra) = classify_text_failure(None, &trimmed);
        return AgyParsed {
            final_text: trimmed,
            is_error: limit_hit.is_some() || infra.is_some(),
            limit_hit,
            infra,
            ..AgyParsed::default()
        };
    };

    let structured = obj
        .get("structured_output")
        .filter(|v| !v.is_null())
        .and_then(|v| serde_json::to_string(v).ok());
    let response_text = ["response", "result", "text"]
        .into_iter()
        .find_map(|k| obj.get(k).and_then(Value::as_str))
        .unwrap_or_default()
        .to_string();
    let final_text = if expect_structured {
        structured.unwrap_or_else(|| response_text.clone())
    } else if !response_text.is_empty() {
        response_text.clone()
    } else {
        structured.unwrap_or_default()
    };

    let (tokens_in, tokens_out, usage_present) = extract_usage(&obj);
    let is_error = obj
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || obj.get("error").is_some_and(|e| !e.is_null())
        || obj
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|s| s.eq_ignore_ascii_case("error"))
        || obj.get("subtype").and_then(Value::as_str)
            == Some("error_max_structured_output_retries");
    let (limit_hit, infra) = if is_error {
        let (status, msg) = extract_error_details(&obj, &response_text);
        classify_text_failure(status, &msg)
    } else {
        (None, None)
    };
    AgyParsed {
        final_text,
        tokens_in,
        tokens_out,
        usage_present,
        is_error,
        limit_hit,
        infra,
    }
}

fn first_u64(map: &Map<String, Value>, keys: &[&str]) -> u64 {
    keys.iter()
        .find_map(|k| map.get(*k).and_then(Value::as_u64))
        .unwrap_or(0)
}

fn extract_usage(obj: &Map<String, Value>) -> (u64, u64, bool) {
    if let Some(u) = obj.get("usage").and_then(Value::as_object) {
        let input = first_u64(u, &["input_tokens"]);
        let cache = first_u64(u, &["cache_read_tokens", "cache_read_input_tokens"]);
        return (
            input.saturating_sub(cache),
            first_u64(u, &["output_tokens"]),
            true,
        );
    }
    let Some(models) = obj
        .get("stats")
        .and_then(|s| s.get("models"))
        .and_then(Value::as_object)
    else {
        return (0, 0, false);
    };
    let (mut tin, mut tout, mut found) = (0u64, 0u64, false);
    for t in models
        .values()
        .filter_map(|e| e.get("tokens").and_then(Value::as_object))
    {
        found = true;
        let input = first_u64(t, &["input", "prompt"]);
        tin = tin.saturating_add(input.saturating_sub(first_u64(t, &["cached"])));
        tout = tout.saturating_add(first_u64(t, &["output", "candidates"]));
    }
    (tin, tout, found)
}

fn extract_error_details(obj: &Map<String, Value>, fallback: &str) -> (Option<i64>, String) {
    let err_val = obj.get("error");
    let err_obj = err_val.and_then(Value::as_object);
    let status = ["status_code", "status"]
        .into_iter()
        .find_map(|k| obj.get(k).and_then(Value::as_i64))
        .or_else(|| {
            err_obj.and_then(|o| {
                ["status_code", "code"]
                    .into_iter()
                    .find_map(|k| o.get(k).and_then(Value::as_i64))
            })
        });
    let mut parts: Vec<&str> = err_val.and_then(Value::as_str).into_iter().collect();
    if let Some(o) = err_obj {
        for key in ["status", "type", "code", "message"] {
            parts.extend(o.get(key).and_then(Value::as_str));
        }
    }
    parts.extend(obj.get("message").and_then(Value::as_str));
    if !fallback.is_empty() {
        parts.push(fallback);
    }
    let detail = if parts.is_empty() {
        status.map(|s| format!("HTTP {s}")).unwrap_or_default()
    } else {
        parts.join(" ")
    };
    (status, detail)
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.next_if_eq(&'[').is_some() {
                let _ = chars.by_ref().find(|n| ('@'..='~').contains(n));
            } else if chars.next_if_eq(&']').is_some() {
                while let Some(n) = chars.next() {
                    if n == '\x07' || (n == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
        } else if c != '\r' {
            out.push(c);
        }
    }
    out
}

fn extract_json_object(cleaned: &str) -> Option<Map<String, Value>> {
    let trimmed = cleaned.trim();
    if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(trimmed) {
        return Some(map);
    }
    for line in trimmed.lines().rev().map(str::trim) {
        if line.starts_with('{')
            && line.ends_with('}')
            && let Ok(Value::Object(map)) = serde_json::from_str::<Value>(line)
        {
            return Some(map);
        }
    }
    let (mut last, mut skip_until) = (None, 0usize);
    for (idx, ch) in trimmed.char_indices() {
        if idx < skip_until || ch != '{' {
            continue;
        }
        let mut stream = serde_json::Deserializer::from_str(&trimmed[idx..]).into_iter::<Value>();
        if let Some(Ok(Value::Object(map))) = stream.next() {
            skip_until = idx.saturating_add(stream.byte_offset());
            last = Some(map);
        }
    }
    last
}
