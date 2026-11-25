#![allow(clippy::unwrap_used, clippy::expect_used)]

use aaf_domain::config::{GlobalConfig, RepoConfig};
use aaf_domain::ports::Clock;
use aaf_domain::telegram::TelegramConfig;
use aaf_engine::config::{ConfigInputs, ConfigOverlay, load};
use aaf_engine::telegram::{
    ApprovePreview, Chat, CommandDispatcher, CommandHandler, LockedFileSummary, Message, Update,
    UpdateOutcome, User,
};
use tempfile::TempDir;

struct FixedClock(i64);

impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

#[derive(Default)]
struct SpyHandler {
    calls: Vec<String>,
}

impl CommandHandler for SpyHandler {
    fn status(&self, all: bool) -> Result<String, String> {
        Ok(format!("status(all={all})"))
    }
    fn board(&self, state: Option<&str>) -> Result<String, String> {
        Ok(format!("board(state={})", state.unwrap_or("all")))
    }
    fn show(&self, task_id: &str) -> Result<String, String> {
        Ok(format!("show({task_id})"))
    }
    fn handover(&self, task_id: &str) -> Result<String, String> {
        Ok(format!("handover({task_id})"))
    }
    fn preview_approve(&self, task_id: &str) -> Result<ApprovePreview, String> {
        Ok(ApprovePreview {
            task_id: task_id.to_string(),
            lock_fp: "deadbeef12345678".to_string(),
            locked_changes: vec![LockedFileSummary {
                change: "M".to_string(),
                path: "Cargo.toml".to_string(),
            }],
        })
    }
    fn approve(&mut self, id: &str, fp: &str, uid: i64) -> Result<String, String> {
        let msg = format!("approve({id},{fp},u={uid})");
        self.calls.push(msg.clone());
        Ok(msg)
    }
    fn unblock(&mut self, id: &str, uid: i64) -> Result<String, String> {
        let msg = format!("unblock({id},u={uid})");
        self.calls.push(msg.clone());
        Ok(msg)
    }
    fn rework(&mut self, id: &str, reason: &str, uid: i64) -> Result<String, String> {
        let msg = format!("rework({id},{reason},u={uid})");
        self.calls.push(msg.clone());
        Ok(msg)
    }
    fn steer(&mut self, id: &str, note: &str, uid: i64) -> Result<String, String> {
        let msg = format!("steer({id},{note},u={uid})");
        self.calls.push(msg.clone());
        Ok(msg)
    }
    fn unfreeze(&mut self, uid: i64) -> Result<String, String> {
        let msg = format!("unfreeze(u={uid})");
        self.calls.push(msg.clone());
        Ok(msg)
    }
}

fn cfg_with(allowed_chat_ids: Vec<i64>, allowed_user_ids: Vec<i64>) -> TelegramConfig {
    TelegramConfig {
        enabled: true,
        allowed_chat_ids,
        allowed_user_ids,
        allow_approve: true,
        allow_mutating: true,
        ..TelegramConfig::default()
    }
}

fn mk_update(
    update_id: i64,
    chat_id: i64,
    chat_type: &str,
    from: Option<User>,
    text: &str,
) -> Update {
    Update {
        update_id,
        message: Some(Message {
            message_id: update_id,
            from,
            chat: Chat {
                id: chat_id,
                chat_type: chat_type.to_string(),
            },
            text: Some(text.to_string()),
            date: 1_700_000_000,
        }),
    }
}

fn human(id: i64) -> User {
    User {
        id,
        is_bot: false,
        username: Some(format!("u{id}")),
        first_name: Some("User".to_string()),
    }
}

fn bot_user(id: i64) -> User {
    User {
        id,
        is_bot: true,
        username: Some(format!("bot{id}")),
        first_name: Some("Bot".to_string()),
    }
}

#[test]
fn group_chat_without_allowed_user_ids_is_fail_closed_for_all_commands() {
    let clock = FixedClock(10_000);
    let mut handler = SpyHandler::default();
    let mut disp =
        CommandDispatcher::new(cfg_with(vec![-100_123_456, -987_654, -555], vec![])).unwrap();

    for (idx, (chat_id, chat_type, cmd)) in [
        (-987_654, "group", "/status"),
        (-987_654, "group", "/board"),
        (-987_654, "group", "/handover t-1"),
        (-987_654, "group", "/approve t-1"),
        (-987_654, "group", "/unblock t-1"),
        (-100_123_456, "supergroup", "/status"),
        (-100_123_456, "supergroup", "/approve t-1"),
        (-100_123_456, "supergroup", "/unblock t-1"),
        (-555, "channel", "/status"),
    ]
    .into_iter()
    .enumerate()
    {
        let upd = mk_update(idx as i64 + 1, chat_id, chat_type, Some(human(100)), cmd);
        let outcome = disp.handle_update(&upd, &mut handler, &clock).unwrap();
        assert_eq!(
            outcome,
            UpdateOutcome::IgnoredUnauthorized,
            "expected `{cmd}` in {chat_type} to be rejected when allowed_user_ids is empty"
        );
    }
    assert!(handler.calls.is_empty());
    assert_eq!(disp.pending_len(), 0);
}

#[test]
fn private_dm_with_matching_sender_works_when_allowed_user_ids_is_empty() {
    let clock = FixedClock(10_000);
    let mut handler = SpyHandler::default();
    let mut disp = CommandDispatcher::new(cfg_with(vec![42], vec![])).unwrap();

    let status = disp
        .handle_update(
            &mk_update(1, 42, "private", Some(human(42)), "/status"),
            &mut handler,
            &clock,
        )
        .unwrap();
    assert_eq!(
        status,
        UpdateOutcome::Reply {
            chat_id: 42,
            text: "status(all=false)".to_string(),
        }
    );

    let unblock = disp
        .handle_update(
            &mk_update(2, 42, "private", Some(human(42)), "/unblock t-7"),
            &mut handler,
            &clock,
        )
        .unwrap();
    assert_eq!(
        unblock,
        UpdateOutcome::Reply {
            chat_id: 42,
            text: "unblock(t-7,u=42)".to_string(),
        }
    );
}

#[test]
fn missing_from_bot_sender_empty_chat_type_and_mismatched_dm_are_rejected() {
    let clock = FixedClock(10_000);
    let mut handler = SpyHandler::default();
    let mut disp = CommandDispatcher::new(cfg_with(vec![42, -42], vec![])).unwrap();

    let cases = [
        ("from=None", mk_update(1, 42, "private", None, "/status")),
        (
            "is_bot=true",
            mk_update(2, 42, "private", Some(bot_user(42)), "/status"),
        ),
        (
            "empty chat_type",
            mk_update(3, 42, "", Some(human(42)), "/status"),
        ),
        (
            "private with negative chat_id",
            mk_update(4, -42, "private", Some(human(-42)), "/status"),
        ),
        (
            "private with from.id != chat.id",
            mk_update(5, 42, "private", Some(human(99)), "/status"),
        ),
    ];
    for (label, upd) in cases {
        assert_eq!(
            disp.handle_update(&upd, &mut handler, &clock).unwrap(),
            UpdateOutcome::IgnoredUnauthorized,
            "case `{label}` must be rejected fail-closed"
        );
    }

    // Deserializing Telegram Bot API JSON with `"is_bot": true` must also be rejected.
    let json_bot: Update = serde_json::from_str(
        r#"{"update_id":99,"message":{"message_id":99,"from":{"id":42,"is_bot":true},"chat":{"id":42,"type":"private"},"text":"/status","date":1700000000}}"#,
    )
    .unwrap();
    assert!(
        json_bot
            .message
            .as_ref()
            .unwrap()
            .from
            .as_ref()
            .unwrap()
            .is_bot
    );
    assert_eq!(
        disp.handle_update(&json_bot, &mut handler, &clock).unwrap(),
        UpdateOutcome::IgnoredUnauthorized
    );
}

#[test]
fn group_chat_with_allowed_user_ids_permits_listed_humans_and_blocks_strangers_and_bots() {
    let clock = FixedClock(10_000);
    let mut handler = SpyHandler::default();
    let group_id = -100_999_888;
    let mut disp = CommandDispatcher::new(cfg_with(vec![group_id], vec![100])).unwrap();

    // Listed human user (id=100) is authorized for read-only and mutating commands.
    let status = disp
        .handle_update(
            &mk_update(1, group_id, "supergroup", Some(human(100)), "/status"),
            &mut handler,
            &clock,
        )
        .unwrap();
    assert_eq!(
        status,
        UpdateOutcome::Reply {
            chat_id: group_id,
            text: "status(all=false)".to_string(),
        }
    );

    let approve = disp
        .handle_update(
            &mk_update(2, group_id, "supergroup", Some(human(100)), "/approve t-1"),
            &mut handler,
            &clock,
        )
        .unwrap();
    match approve {
        UpdateOutcome::Reply { chat_id, text } => {
            assert_eq!(chat_id, group_id);
            assert!(text.contains("Approve preview for t-1"), "{text}");
        }
        other => panic!("expected approve preview reply, got {other:?}"),
    }

    // Unlisted human member (id=200) is blocked for BOTH read-only and mutating commands.
    for (idx, cmd) in [
        "/status",
        "/board",
        "/handover t-1",
        "/approve t-1",
        "/unblock t-1",
    ]
    .into_iter()
    .enumerate()
    {
        let upd = mk_update(
            10 + idx as i64,
            group_id,
            "supergroup",
            Some(human(200)),
            cmd,
        );
        assert_eq!(
            disp.handle_update(&upd, &mut handler, &clock).unwrap(),
            UpdateOutcome::IgnoredUnauthorized,
            "stranger user=200 must be blocked on `{cmd}`"
        );
    }

    // Bot user with listed id=100 is STILL blocked.
    assert_eq!(
        disp.handle_update(
            &mk_update(20, group_id, "supergroup", Some(bot_user(100)), "/status"),
            &mut handler,
            &clock,
        )
        .unwrap(),
        UpdateOutcome::IgnoredUnauthorized
    );
}

#[test]
fn allowed_user_ids_is_strictly_tier1_only_and_defaults_empty() {
    assert!(TelegramConfig::default().allowed_user_ids.is_empty());

    let parsed_default: GlobalConfig = toml::from_str(
        r#"
[telegram]
enabled = true
allowed_chat_ids = [42]
"#,
    )
    .unwrap();
    assert!(parsed_default.telegram.allowed_user_ids.is_empty());

    // Tier-2 repo config must reject `[telegram]` altogether (`deny_unknown_fields`).
    let tier2_err = toml::from_str::<RepoConfig>(
        r#"
[telegram]
allowed_user_ids = [100]
"#,
    );
    assert!(
        tier2_err.is_err(),
        "Tier-2 RepoConfig must reject [telegram]"
    );

    // Tier-1 loads `allowed_user_ids`; Tier-3 `config.local.toml` cannot override `[telegram]`.
    let tmp = TempDir::new().unwrap();
    let global_path = tmp.path().join("global.toml");
    std::fs::write(
        &global_path,
        r#"
[telegram]
enabled = true
allowed_chat_ids = [-1001]
allowed_user_ids = [100]
"#,
    )
    .unwrap();

    let repo_root = tmp.path().join("repo");
    std::fs::create_dir_all(repo_root.join(".autofactory")).unwrap();
    std::fs::write(
        repo_root.join(".autofactory/config.local.toml"),
        r#"
[telegram]
allowed_user_ids = [100, 999]
"#,
    )
    .unwrap();

    let eff = load(&ConfigInputs {
        global_config: Some(global_path),
        repo_root,
        env: &|_| None,
        cli: ConfigOverlay::default(),
    })
    .unwrap();

    assert_eq!(eff.global.telegram.allowed_chat_ids, vec![-1001]);
    assert_eq!(eff.global.telegram.allowed_user_ids, vec![100]);
    assert!(
        eff.warnings
            .iter()
            .any(|w| w.contains("[telegram] ignored")),
        "expected Tier-3 warning for [telegram], got: {:?}",
        eff.warnings
    );
}
