//! Fail-closed Telegram command parsing, authorization, and two-step confirmation.

use std::collections::hash_map::RandomState;
use std::fmt::Write as _;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

use aaf_domain::ports::Clock;
use aaf_domain::telegram::TelegramConfig;
use sha2::{Digest, Sha256};

use super::client::{TelegramError, Update};
pub use super::{sanitize_input, sanitize_output};
use crate::git::worktree::validate_task_id;

/// Hard ceiling on in-memory pending confirmations to keep memory bounded.
pub const MAX_PENDING_CONFIRMATIONS: usize = 32;
/// Maximum length (in Unicode scalar values) accepted for free-form arguments.
pub const MAX_ARG_CHARS: usize = 512;
const MAX_RAW_COMMAND_CHARS: usize = 1024;
const CONFIRM_CODE_HEX_LEN: usize = 8;

static NONCE_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedFileSummary {
    pub change: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovePreview {
    pub task_id: String,
    pub lock_fp: String,
    pub locked_changes: Vec<LockedFileSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MutatingAction {
    Approve { task_id: String, lock_fp: String },
    Unblock { task_id: String },
    UnblockAll,
    Rework { task_id: String, reason: String },
    Steer { task_id: String, note: String },
    Unfreeze,
}

/// Backend operations invoked by [`CommandDispatcher`].
pub trait CommandHandler {
    fn status(&self, all: bool) -> Result<String, String>;
    fn board(&self, state: Option<&str>) -> Result<String, String>;
    fn show(&self, task_id: &str) -> Result<String, String>;
    fn handover(&self, task_id: &str) -> Result<String, String>;
    fn preview_approve(&self, task_id: &str) -> Result<ApprovePreview, String>;
    fn approve(
        &mut self,
        task_id: &str,
        expected_lock_fp: &str,
        user_id: i64,
    ) -> Result<String, String>;
    fn unblock(&mut self, task_id: &str, user_id: i64) -> Result<String, String>;
    fn unblock_all(&mut self, _user_id: i64) -> Result<String, String> {
        Err("unblock all is not implemented by this handler".to_string())
    }
    fn rework(&mut self, task_id: &str, reason: &str, user_id: i64) -> Result<String, String>;
    fn steer(&mut self, task_id: &str, note: &str, user_id: i64) -> Result<String, String>;
    fn unfreeze(&mut self, user_id: i64) -> Result<String, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedCommand {
    Help,
    Status { all: bool },
    Board { state: Option<String> },
    Show { task_id: String },
    Handover { task_id: String },
    Approve { task_id: String },
    Unblock { task_id: String },
    UnblockAll,
    Rework { task_id: String, reason: String },
    Steer { task_id: String, note: String },
    Unfreeze,
    Confirm { code: String },
    UsageError(&'static str),
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// Update came from an unauthorized chat, missing/bot sender, or disallowed user — silently dropped.
    IgnoredUnauthorized,
    /// Authorized message was empty or not a `/command`.
    IgnoredNonCommand,
    /// Authorized command produced a sanitized reply for `chat_id`.
    Reply { chat_id: i64, text: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingConfirmation {
    code: String,
    chat_id: i64,
    user_id: i64,
    action: MutatingAction,
    expires_at: i64,
}

/// Bounded store of single-use confirmation codes bound to `(chat_id, user_id)`.
#[derive(Debug, Default)]
pub struct PendingStore {
    entries: Vec<PendingConfirmation>,
    max_entries: usize,
}

impl PendingStore {
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: Vec::new(),
            max_entries: max_entries.max(1),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn evict_expired(&mut self, now: i64) {
        self.entries.retain(|e| now <= e.expires_at);
    }

    pub fn issue(
        &mut self,
        chat_id: i64,
        user_id: i64,
        action: MutatingAction,
        now: i64,
        ttl_secs: u64,
    ) -> String {
        self.evict_expired(now);
        while self.entries.len() >= self.max_entries {
            self.entries.remove(0);
        }
        let ttl_i64 = i64::try_from(ttl_secs).unwrap_or(i64::MAX);
        let expires_at = now.saturating_add(ttl_i64);
        let mut code = generate_nonce(chat_id, user_id, now);
        while self.entries.iter().any(|e| e.code == code) {
            code = generate_nonce(chat_id, user_id, now);
        }
        self.entries.push(PendingConfirmation {
            code: code.clone(),
            chat_id,
            user_id,
            action,
            expires_at,
        });
        code
    }

    /// Consumes a matching pending confirmation for the exact `(chat_id, user_id)`.
    /// Entries belonging to another chat or user are never consumed or leaked.
    pub fn consume(
        &mut self,
        code: &str,
        chat_id: i64,
        user_id: i64,
        now: i64,
    ) -> Option<MutatingAction> {
        self.evict_expired(now);
        let normalized = code.trim().to_ascii_lowercase();
        let pos = self.entries.iter().position(|e| {
            e.code == normalized
                && e.chat_id == chat_id
                && e.user_id == user_id
                && now <= e.expires_at
        })?;
        Some(self.entries.remove(pos).action)
    }
}

/// Stateful command router holding Tier-1 config and pending confirmations.
#[derive(Debug)]
pub struct CommandDispatcher {
    cfg: TelegramConfig,
    pending: PendingStore,
}

impl CommandDispatcher {
    pub fn new(cfg: TelegramConfig) -> Result<Self, TelegramError> {
        if cfg.allowed_chat_ids.is_empty() {
            return Err(TelegramError::EmptyAllowlist);
        }
        Ok(Self {
            cfg,
            pending: PendingStore::new(MAX_PENDING_CONFIRMATIONS),
        })
    }

    pub fn config(&self) -> &TelegramConfig {
        &self.cfg
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn handle_update(
        &mut self,
        update: &Update,
        handler: &mut dyn CommandHandler,
        clock: &dyn Clock,
    ) -> Result<UpdateOutcome, TelegramError> {
        if self.cfg.allowed_chat_ids.is_empty() {
            return Err(TelegramError::EmptyAllowlist);
        }
        let Some(msg) = &update.message else {
            return Ok(UpdateOutcome::IgnoredNonCommand);
        };
        let chat_id = msg.chat.id;
        if !self.cfg.allowed_chat_ids.contains(&chat_id) {
            return Ok(UpdateOutcome::IgnoredUnauthorized);
        }
        let Some(from) = msg.from.as_ref() else {
            return Ok(UpdateOutcome::IgnoredUnauthorized);
        };
        if from.is_bot {
            return Ok(UpdateOutcome::IgnoredUnauthorized);
        }
        let sender_allowed = if self.cfg.allowed_user_ids.is_empty() {
            msg.chat.chat_type == "private" && chat_id > 0 && from.id == chat_id
        } else {
            self.cfg.allowed_user_ids.contains(&from.id)
        };
        if !sender_allowed {
            return Ok(UpdateOutcome::IgnoredUnauthorized);
        }
        let Some(cmd) = msg.text.as_deref().and_then(parse_command) else {
            return Ok(UpdateOutcome::IgnoredNonCommand);
        };
        let reply = self.dispatch(chat_id, from.id, cmd, handler, clock);
        Ok(UpdateOutcome::Reply {
            chat_id,
            text: sanitize_output(&reply),
        })
    }

    fn dispatch(
        &mut self,
        chat_id: i64,
        user_id: i64,
        cmd: ParsedCommand,
        handler: &mut dyn CommandHandler,
        clock: &dyn Clock,
    ) -> String {
        match cmd {
            ParsedCommand::Help => help_text(&self.cfg),
            ParsedCommand::Status { all } => format_result("status", handler.status(all)),
            ParsedCommand::Board { state } => {
                format_result("board", handler.board(state.as_deref()))
            }
            ParsedCommand::Show { task_id } => {
                with_valid_id(&task_id, |id| format_result("show", handler.show(id)))
            }
            ParsedCommand::Handover { task_id } => with_valid_id(&task_id, |id| {
                format_result("handover", handler.handover(id))
            }),
            ParsedCommand::Approve { task_id } => {
                self.prepare_approve(chat_id, user_id, &task_id, handler, clock)
            }
            ParsedCommand::Unblock { task_id } => self.run_safe_recovery(
                user_id,
                Some(&task_id),
                |h, u| format_result("unblock", h.unblock(&task_id, u)),
                handler,
            ),
            ParsedCommand::UnblockAll => self.run_safe_recovery(
                user_id,
                None,
                |h, u| format_result("unblock", h.unblock_all(u)),
                handler,
            ),
            ParsedCommand::Rework { task_id, reason } => self.prepare_mutating(
                chat_id,
                user_id,
                Some(&task_id),
                MutatingAction::Rework {
                    task_id: task_id.clone(),
                    reason: reason.clone(),
                },
                format!("rework task {task_id}: {reason}"),
                clock,
            ),
            ParsedCommand::Steer { task_id, note } => self.prepare_mutating(
                chat_id,
                user_id,
                Some(&task_id),
                MutatingAction::Steer {
                    task_id: task_id.clone(),
                    note: note.clone(),
                },
                format!("steer task {task_id}: {note}"),
                clock,
            ),
            ParsedCommand::Unfreeze => self.run_safe_recovery(
                user_id,
                None,
                |h, u| format_result("unfreeze", h.unfreeze(u)),
                handler,
            ),
            ParsedCommand::Confirm { code } => {
                self.execute_confirm(chat_id, user_id, &code, handler, clock)
            }
            ParsedCommand::UsageError(msg) => format!("usage: {msg}"),
            ParsedCommand::Unknown(name) => {
                format!("unknown command: {name}. Send /help for available commands.")
            }
        }
    }

    fn prepare_approve(
        &mut self,
        chat_id: i64,
        uid: i64,
        task_id: &str,
        handler: &dyn CommandHandler,
        clock: &dyn Clock,
    ) -> String {
        if !self.cfg.allow_approve {
            return "refused: /approve is disabled in Tier-1 config (telegram.allow_approve = false)"
                .to_string();
        }
        if let Err(e) = validate_task_id(task_id) {
            return format!("invalid task id: {e}");
        }
        let preview = match handler.preview_approve(task_id) {
            Ok(p) => p,
            Err(e) => return format!("approve preview failed: {e}"),
        };
        if preview.locked_changes.is_empty() || preview.lock_fp.is_empty() {
            return format!("no locked file is changed for {task_id}; nothing to approve");
        }
        let code = self.pending.issue(
            chat_id,
            uid,
            MutatingAction::Approve {
                task_id: preview.task_id.clone(),
                lock_fp: preview.lock_fp.clone(),
            },
            clock.now(),
            self.cfg.confirm_ttl_secs,
        );
        let mut out = format!(
            "Approve preview for {}\nlock_fp: {}\nlocked changes:\n",
            preview.task_id, preview.lock_fp
        );
        for ch in &preview.locked_changes {
            let _ = writeln!(out, "  {} {}", ch.change, ch.path);
        }
        let _ = write!(
            out,
            "\nSend /confirm {code} within {}s to sign approval as telegram:{uid}.",
            self.cfg.confirm_ttl_secs
        );
        out
    }

    fn check_mutating(&self, uid: i64, task_id: Option<&str>) -> Result<i64, String> {
        if !self.cfg.allow_mutating {
            return Err(
                "refused: mutating commands are disabled in Tier-1 config (telegram.allow_mutating = false)"
                    .to_string(),
            );
        }
        if let Some(id) = task_id
            && let Err(e) = validate_task_id(id)
        {
            return Err(format!("invalid task id: {e}"));
        }
        Ok(uid)
    }

    fn run_safe_recovery(
        &self,
        uid: i64,
        task_id: Option<&str>,
        exec: impl FnOnce(&mut dyn CommandHandler, i64) -> String,
        handler: &mut dyn CommandHandler,
    ) -> String {
        match self.check_mutating(uid, task_id) {
            Ok(uid) if !self.cfg.allowed_chat_ids.is_empty() => exec(handler, uid),
            Ok(_) => "refused: telegram allowed_chat_ids is empty".to_string(),
            Err(msg) => msg,
        }
    }

    fn prepare_mutating(
        &mut self,
        chat_id: i64,
        uid: i64,
        task_id: Option<&str>,
        action: MutatingAction,
        summary: String,
        clock: &dyn Clock,
    ) -> String {
        let uid = match self.check_mutating(uid, task_id) {
            Ok(uid) => uid,
            Err(msg) => return msg,
        };
        let code = self
            .pending
            .issue(chat_id, uid, action, clock.now(), self.cfg.confirm_ttl_secs);
        format!(
            "Pending action: {summary}\nSend /confirm {code} within {}s to execute.",
            self.cfg.confirm_ttl_secs
        )
    }

    fn execute_confirm(
        &mut self,
        chat_id: i64,
        uid: i64,
        code: &str,
        handler: &mut dyn CommandHandler,
        clock: &dyn Clock,
    ) -> String {
        if !is_valid_confirm_code(code) {
            return "invalid or expired confirmation code".to_string();
        }
        let Some(action) = self.pending.consume(code, chat_id, uid, clock.now()) else {
            return "invalid or expired confirmation code".to_string();
        };
        match action {
            MutatingAction::Approve { task_id, lock_fp } if self.cfg.allow_approve => {
                format_result("approve", handler.approve(&task_id, &lock_fp, uid))
            }
            MutatingAction::Approve { .. } => {
                "refused: /approve is disabled in Tier-1 config".to_string()
            }
            _ if !self.cfg.allow_mutating => {
                "refused: mutating commands are disabled in Tier-1 config".to_string()
            }
            MutatingAction::Unblock { task_id } => {
                format_result("unblock", handler.unblock(&task_id, uid))
            }
            MutatingAction::UnblockAll => format_result("unblock", handler.unblock_all(uid)),
            MutatingAction::Rework { task_id, reason } => {
                format_result("rework", handler.rework(&task_id, &reason, uid))
            }
            MutatingAction::Steer { task_id, note } => {
                format_result("steer", handler.steer(&task_id, &note, uid))
            }
            MutatingAction::Unfreeze => format_result("unfreeze", handler.unfreeze(uid)),
        }
    }
}

/// Parses an incoming Telegram text message into a [`ParsedCommand`].
/// Returns `None` when the text does not start with `/`.
pub fn parse_command(raw: &str) -> Option<ParsedCommand> {
    let cleaned = sanitize_input(raw, MAX_RAW_COMMAND_CHARS);
    if !cleaned.starts_with('/') {
        return None;
    }
    let (head, rest) = cleaned
        .split_once(' ')
        .map_or((cleaned.as_str(), ""), |(h, r)| (h, r.trim()));
    let cmd = head
        .split_once('@')
        .map_or(head, |(name, _)| name)
        .to_ascii_lowercase();

    let parsed = match cmd.as_str() {
        "/help" | "/start" => ParsedCommand::Help,
        "/status" if rest.is_empty() => ParsedCommand::Status { all: false },
        "/status" if rest.eq_ignore_ascii_case("all") => ParsedCommand::Status { all: true },
        "/status" => ParsedCommand::UsageError("/status [all]"),
        "/board" if rest.is_empty() => ParsedCommand::Board { state: None },
        "/board" if !rest.contains(' ') => ParsedCommand::Board {
            state: Some(rest.to_ascii_lowercase()),
        },
        "/board" => ParsedCommand::UsageError("/board [state]"),
        "/show" => single_arg(rest, "/show <task_id>", |task_id| ParsedCommand::Show {
            task_id,
        }),
        "/handover" => single_arg(rest, "/handover <task_id>", |task_id| {
            ParsedCommand::Handover { task_id }
        }),
        "/approve" => single_arg(rest, "/approve <task_id>", |task_id| {
            ParsedCommand::Approve { task_id }
        }),
        "/unblock" if rest.eq_ignore_ascii_case("all") => ParsedCommand::UnblockAll,
        "/unblock" => single_arg(rest, "/unblock <task_id|all>", |task_id| {
            ParsedCommand::Unblock { task_id }
        }),
        "/rework" => id_and_text(rest, "/rework <task_id> <reason>", |task_id, reason| {
            ParsedCommand::Rework { task_id, reason }
        }),
        "/steer" => id_and_text(rest, "/steer <task_id> <note>", |task_id, note| {
            ParsedCommand::Steer { task_id, note }
        }),
        "/unfreeze" if rest.is_empty() => ParsedCommand::Unfreeze,
        "/unfreeze" => ParsedCommand::UsageError("/unfreeze"),
        "/confirm" => single_arg(rest, "/confirm <code>", |code| ParsedCommand::Confirm {
            code: code.to_ascii_lowercase(),
        }),
        _ => ParsedCommand::Unknown(cmd),
    };
    Some(parsed)
}

fn single_arg(
    rest: &str,
    usage: &'static str,
    build: impl FnOnce(String) -> ParsedCommand,
) -> ParsedCommand {
    if rest.is_empty() || rest.contains(' ') {
        return ParsedCommand::UsageError(usage);
    }
    build(rest.to_string())
}

fn id_and_text(
    rest: &str,
    usage: &'static str,
    build: impl FnOnce(String, String) -> ParsedCommand,
) -> ParsedCommand {
    let Some((id, tail)) = rest.split_once(' ') else {
        return ParsedCommand::UsageError(usage);
    };
    let note = sanitize_input(tail, MAX_ARG_CHARS);
    if id.is_empty() || note.is_empty() {
        return ParsedCommand::UsageError(usage);
    }
    build(id.to_string(), note)
}

fn with_valid_id(task_id: &str, f: impl FnOnce(&str) -> String) -> String {
    match validate_task_id(task_id) {
        Ok(()) => f(task_id),
        Err(e) => format!("invalid task id: {e}"),
    }
}

fn is_valid_confirm_code(code: &str) -> bool {
    code.len() == CONFIRM_CODE_HEX_LEN && code.bytes().all(|b| b.is_ascii_hexdigit())
}

fn format_result(action: &str, res: Result<String, String>) -> String {
    match res {
        Ok(text) => text,
        Err(err) => format!("{action} failed: {err}"),
    }
}

fn help_text(cfg: &TelegramConfig) -> String {
    format!(
        "AAF Telegram Remote Companion\n\
         Read-only:\n\
         /help - show commands\n\
         /status [all] - factory status overview\n\
         /board [state] - list tasks\n\
         /show <task_id> - task details\n\
         /handover <task_id> - compact handover summary\n\n\
         Mutating (allow_approve={}, allow_mutating={}, ttl={}s):\n\
         /approve <task_id> - preview locked diff & issue /confirm code\n\
         /unblock <task_id|all> - unblock task(s) (1-step for allowlisted chats)\n\
         /unfreeze - clear factory freeze (1-step for allowlisted chats)\n\
         /rework <task_id> <reason> - send review task back (2-step)\n\
         /steer <task_id> <note> - append human steering note (2-step)\n\
         /confirm <code> - confirm pending action",
        cfg.allow_approve, cfg.allow_mutating, cfg.confirm_ttl_secs
    )
}

fn generate_nonce(chat_id: i64, user_id: i64, now: i64) -> String {
    let seq = NONCE_SEQ.fetch_add(1, Ordering::Relaxed);
    let r1 = RandomState::new().build_hasher().finish();
    let r2 = RandomState::new().build_hasher().finish();
    let mut hasher = Sha256::new();
    for bytes in [
        r1.to_le_bytes(),
        r2.to_le_bytes(),
        seq.to_le_bytes(),
        chat_id.to_le_bytes(),
        user_id.to_le_bytes(),
        now.to_le_bytes(),
    ] {
        hasher.update(bytes);
    }
    hasher.update(std::process::id().to_le_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(CONFIRM_CODE_HEX_LEN);
    for byte in &digest[..CONFIRM_CODE_HEX_LEN / 2] {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}
