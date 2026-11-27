//! Native headless executor for Antigravity CLI (`agy` / `antigravity`) with
//! JSON output, token accounting, and least-privilege `~/.gemini` confinement.
//!
//! Flags and JSON output shape were verified against `agy` v1.5.15.

use std::ffi::OsStr;
use std::path::PathBuf;

use aaf_domain::account::ProviderKind;
use aaf_domain::ports::{AgentOutcome, AgentRequest, AgentRunner, PortError, PortResult};

use super::macos_oauth_refresh;
use super::pty::{CommandSpec, GENERIC_TAIL_BYTES};
#[cfg(not(target_os = "linux"))]
use super::sandbox;
use super::{AccountPaths, ExecEnv, find_on_path, open_stream_log, run_confined_with_tail};

const RAW_LIMIT_CHARS: usize = 500;
mod classify;
mod parse;
use classify::classify_text_failure;
use parse::strip_ansi;
pub use parse::{AgyParsed, parse_agy_output};

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
    "antigravity-cli/antigravity-oauth-token",
    "antigravity-cli/bin",
    "antigravity-cli/builtin",
    "antigravity-cli/mcp",
    "antigravity-cli/plugin_data",
    "antigravity-cli/settings.json",
    "antigravity-cli/updater",
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
    "antigravity-cli/log",
    "antigravity-cli/presence",
    "antigravity-cli/annotations",
    "antigravity-cli/brain",
    "antigravity-cli/cache",
    "antigravity-cli/conversations",
    "antigravity-cli/crashes",
    "antigravity-cli/implicit",
    "antigravity-cli/knowledge",
    "antigravity-cli/persistent_terminals",
    "antigravity-cli/scratch",
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
            let cli_dir = root.join("antigravity-cli");
            if request.sandbox.enabled {
                let _ = std::fs::create_dir_all(&cli_dir);
            }
            let cli_str = cli_dir.to_string_lossy();
            let canon_cli = sandbox::canonical(&cli_str)?;
            let canon_cli_str = canon_cli.to_str().ok_or_else(|| {
                PortError::Invalid(format!(
                    "gemini cli path is not UTF-8: {}",
                    canon_cli.display()
                ))
            })?;
            vec![
                format!(
                    "^{}/conversation_summaries\\.db(-wal|-shm|-journal)?$",
                    sandbox::regex_escape(canon_ag_str)
                ),
                format!(
                    "^{}/(conversation_summaries\\.db(-wal|-shm|-journal)?|cli\\.log|jetbox_summaries_proto\\.pb(\\.[0-9a-fA-F-]+\\.tmp)?|jetski_state\\.pbtxt|last_check\\.timestamp)$",
                    sandbox::regex_escape(canon_cli_str)
                ),
            ]
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
        // Refresh Antigravity OAuth on the host every 40m before entering Seatbelt.
        macos_oauth_refresh::preflight_agy(&self.bin, &self.env, request);
        let mut log = open_stream_log(&self.env, request);
        log.write_start("antigravity", self.model.as_deref());

        let mut outcome = self.run_once(request, &spec)?;
        let non_json_tail = macos_oauth_refresh::non_json_lines(&outcome.log_tail);
        if outcome.exit_code != Some(0)
            && !outcome.timed_out
            && outcome.limit_hit.is_none()
            && macos_oauth_refresh::is_agy_auth_retryable(outcome.infra.as_ref(), &non_json_tail)
            && macos_oauth_refresh::refresh_agy_now(&self.bin, &self.env, request)
        {
            if outcome.tokens_in == 0 && outcome.tokens_out == 0 {
                log.write_event(
                    "retry",
                    Some("antigravity"),
                    Some("oauth_refreshed_on_host"),
                );
                outcome = self.run_once(request, &spec)?;
            } else {
                outcome.infra =
                    macos_oauth_refresh::reclassify_refreshed_keychain_failure(outcome.infra);
            }
        }
        let exit = outcome
            .exit_code
            .map_or_else(|| "signal".to_string(), |c| c.to_string());
        let finish = format!(
            "exit={exit} timed_out={} tokens_in={} tokens_out={}",
            outcome.timed_out, outcome.tokens_in, outcome.tokens_out
        );
        log.write_event("finish", Some("antigravity"), Some(&finish));
        Ok(outcome)
    }

    fn reports_tokens(&self) -> bool {
        true
    }
}

impl AgyRunner {
    fn run_once(&self, request: &AgentRequest, spec: &CommandSpec) -> PortResult<AgentOutcome> {
        let account = self.account_paths(request)?;
        let expect_structured = request
            .json_schema
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty());
        let out = run_confined_with_tail(
            spec.clone(),
            request,
            &self.env,
            account,
            "antigravity",
            None,
            GENERIC_TAIL_BYTES,
        )?;
        let mut parsed = parse_agy_output(&out.tail, expect_structured);
        let should_scan_tail = !parsed.is_local_cap_error
            && (!parsed.usage_present || parsed.is_error || parsed.final_text.is_empty());
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
}
