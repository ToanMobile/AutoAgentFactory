//! Runs Claude Code headless and reads its `stream-json` output.

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use aaf_domain::account::ProviderKind;
use aaf_domain::ports::{AgentOutcome, AgentRequest, AgentRunner, PortError, PortResult};

use super::pty::CommandSpec;
use super::{AccountPaths, ExecEnv, open_stream_log, run_confined};

mod stream;

use stream::auth_wording_infra;
pub use stream::{StreamParser, StreamSummary, parse_stream, resolve_context_window};

pub struct ClaudeCodeRunner {
    bin: String,
    env: ExecEnv,
    model: String,
    effort: String,
    allow_unsandboxed_bypass: bool,
}

impl ClaudeCodeRunner {
    pub fn new(bin: impl Into<String>, env: ExecEnv) -> Self {
        Self {
            bin: bin.into(),
            env,
            model: String::new(),
            effort: String::new(),
            allow_unsandboxed_bypass: false,
        }
    }

    #[must_use]
    pub fn with_model_and_effort(mut self, model: &str, effort: &str) -> Self {
        self.model = model.to_string();
        self.effort = effort.to_string();
        self
    }

    /// Explicit non-persistent opt-in allowing `--permission-mode bypassPermissions`
    /// on unsandboxed write runs. Defaults to `false`.
    #[must_use]
    pub fn with_allow_unsandboxed_bypass(mut self, allow: bool) -> Self {
        self.allow_unsandboxed_bypass = allow;
        self
    }

    /// The exact command a run of `request` executes (before sandboxing).
    pub fn command(&self, request: &AgentRequest) -> PortResult<CommandSpec> {
        let args = super::claude_flags::build_claude_args(
            request,
            &self.model,
            &self.effort,
            self.allow_unsandboxed_bypass,
        )?;

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
                writable_regex: Vec::new(),
            },
            (None, Some(home)) => AccountPaths {
                writable: vec![format!("{home}/.claude"), format!("{home}/.claude.json")],
                write_denied: closed_entries(&format!("{home}/.claude")),
                writable_regex: Vec::new(),
            },
            (None, None) => AccountPaths::default(),
        };
        let mut log = open_stream_log(&self.env, request);
        log.write_start("claude", Some(&self.model));
        let state = Arc::new(Mutex::new((StreamParser::default(), log)));
        let feed = Arc::clone(&state);
        let output = run_confined(
            spec,
            request,
            &self.env,
            account,
            "claude",
            Some(Box::new(move |line| {
                let mut guard = feed.lock().unwrap_or_else(PoisonError::into_inner);
                guard.0.feed_line(line);
                guard.1.record_claude_line(line);
            })),
        )?;
        let mut guard = state.lock().unwrap_or_else(PoisonError::into_inner);
        let summary = guard.0.summary();
        let exit_code =
            if (summary.is_error || summary.infra.is_some()) && output.exit_code == Some(0) {
                Some(1)
            } else {
                output.exit_code
            };
        let effective_model = summary
            .observed_model
            .as_deref()
            .or_else(|| (!self.model.trim().is_empty()).then_some(self.model.as_str()));
        let ctx_pct = resolve_context_window(effective_model)
            .filter(|&(win, _)| win > 0 && summary.peak_context_tokens > 0)
            .map(|(win, _)| {
                let pct = summary
                    .peak_context_tokens
                    .saturating_mul(100)
                    .checked_div(win)
                    .unwrap_or(0);
                u8::try_from(pct.min(100)).unwrap_or(100)
            });
        guard.1.write_claude_finish(
            exit_code,
            output.timed_out,
            summary.tokens_in,
            summary.tokens_out,
            summary.peak_context_tokens,
            summary.cache_read_tokens,
            effective_model,
            ctx_pct,
        );
        let mut limit_hit = summary.limit_hit.filter(|_| !output.timed_out);
        let mut infra = summary.infra.filter(|_| !output.timed_out);
        if exit_code != Some(0)
            && !output.timed_out
            && summary.tokens_in == 0
            && summary.tokens_out == 0
            && limit_hit.is_none()
            && infra.is_none()
        {
            infra = auth_wording_infra(&summary.final_text)
                .or_else(|| auth_wording_infra(&output.tail));
        }
        if exit_code != Some(0)
            && !output.timed_out
            && !summary.saw_result
            && summary.final_text.is_empty()
            && limit_hit.is_none()
            && infra.is_none()
        {
            (limit_hit, infra) =
                super::generic::classify_generic_failure(exit_code, false, &output.tail);
        }
        Ok(AgentOutcome {
            exit_code,
            timed_out: output.timed_out,
            final_text: summary.final_text,
            tokens_in: summary.tokens_in,
            tokens_out: summary.tokens_out,
            limit_hit,
            infra,
            log_tail: output.tail,
        })
    }
}
