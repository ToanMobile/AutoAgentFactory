//! `aaf accounts list|next|cooldown` against a fake home.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "cli_support.rs"]
mod support;

use serde_json::{Value, json};
use support::{Sandbox, write};

const SECRET: &str = "sk-fixture-SECRET-do-not-print";

/// Two Claude Code config dirs (`~/.claude`, `~/.claude-work`), the second
/// aliased to `team`, each holding a credential file that must stay unread.
fn with_accounts() -> Sandbox {
    let sb = Sandbox::new();
    for dir in [".claude", ".claude-work"] {
        let root = sb.home.join(dir);
        std::fs::create_dir_all(root.join("projects")).unwrap();
        write(&root.join("settings.json"), "{}");
        write(&root.join(".credentials.json"), SECRET);
    }
    write(
        &sb.home.join(".config/autofactory/config.toml"),
        "[accounts.aliases]\n\"~/.claude-work\" = \"team\"\n",
    );
    sb
}

fn accounts(out: &Value) -> &Vec<Value> {
    out["accounts"].as_array().unwrap()
}

fn id_of(out: &Value, name: &str) -> String {
    accounts(out)
        .iter()
        .find(|a| a["name"] == json!(name))
        .unwrap_or_else(|| panic!("no account {name}: {out}"))["id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn list_shows_discovered_accounts_with_aliases_and_no_credentials() {
    let sb = with_accounts();

    let out = sb.aaf(&["accounts", "list", "--json"]);

    assert_eq!(out.status.code(), Some(0));
    assert!(!String::from_utf8_lossy(&out.stdout).contains(SECRET));
    let out = support::one_object(&out);
    assert_eq!(out["command"], json!("accounts"));
    let mut names: Vec<&str> = accounts(&out)
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["claude", "team"]);
    for account in accounts(&out) {
        assert_eq!(account["provider"], json!("claude_code"));
        assert!(account["id"].as_str().unwrap().starts_with("claude_code-"));
        assert!(account["config_dir"].as_str().unwrap().contains(".claude"));
        assert_eq!(account["cooldown_until"], Value::Null);
        assert_eq!(account["tokens_5h"], json!(0));
    }

    let text = sb.aaf(&["accounts", "list"]);
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.contains("team") && !text.contains(SECRET), "{text}");
}

#[test]
fn list_hides_an_account_whose_config_dir_is_gone() {
    let sb = with_accounts();
    assert_eq!(accounts(&sb.json(&["accounts", "list"]).1).len(), 2);
    std::fs::rename(sb.home.join(".claude-work"), sb.home.join("moved-away")).unwrap();

    let (code, out) = sb.json(&["accounts", "list"]);

    assert_eq!(code, 0, "{out}");
    assert_eq!(accounts(&out).len(), 1);
    assert_eq!(accounts(&out)[0]["name"], json!("claude"));
}

#[test]
fn cooldown_sets_and_clears_by_id_or_name() {
    let sb = with_accounts();
    let id = id_of(&sb.json(&["accounts", "list"]).1, "team");

    let (code, out) = sb.json(&["accounts", "cooldown", &id, "--until", "99999999999"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["cooldown_until"], json!(99_999_999_999_i64));
    let listed = sb.json(&["accounts", "list"]).1;
    let team = accounts(&listed)
        .iter()
        .find(|a| a["id"] == json!(id))
        .unwrap();
    assert_eq!(team["cooldown_until"], json!(99_999_999_999_i64));
    assert_eq!(team["status"], json!("cooldown"));

    // `next` skips the account in cooldown.
    let (code, next) = sb.json(&["accounts", "next"]);
    assert_eq!(code, 0, "{next}");
    assert_eq!(next["account"]["name"], json!("claude"));

    let (code, out) = sb.json(&["accounts", "cooldown", "team", "--clear"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["cooldown_until"], Value::Null);
    let listed = sb.json(&["accounts", "list"]).1;
    assert!(
        accounts(&listed)
            .iter()
            .all(|a| a["cooldown_until"].is_null())
    );
}

#[test]
fn cooldown_without_a_flag_defaults_to_five_hours() {
    let sb = with_accounts();

    let (code, out) = sb.json(&["accounts", "cooldown", "team"]);

    assert_eq!(code, 0, "{out}");
    let until = out["cooldown_until"].as_i64().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let delta = until - i64::try_from(now).unwrap();
    assert!((5 * 3600 - 120..=5 * 3600).contains(&delta), "{delta}");
}

#[test]
fn cooldown_of_an_unknown_account_is_an_error() {
    let sb = with_accounts();

    let (code, out) = sb.json(&["accounts", "cooldown", "nobody", "--clear"]);

    assert_eq!(code, 1, "{out}");
    assert!(out["error"].as_str().unwrap().contains("nobody"), "{out}");
}

#[test]
fn next_exits_2_when_no_account_is_usable() {
    let sb = Sandbox::new();

    let (code, out) = sb.json(&["accounts", "next"]);

    assert_eq!(code, 2, "{out}");
    assert_eq!(out["status"], json!("none"));
    assert_eq!(out["account"], Value::Null);
}

#[test]
fn until_and_clear_together_is_a_usage_error_as_json() {
    let sb = with_accounts();

    let (code, out) = sb.json(&["accounts", "cooldown", "team", "--until", "5", "--clear"]);

    assert_eq!(code, 2, "{out}");
    assert_eq!(out["status"], json!("usage_error"));
    assert_eq!(out["ok"], json!(false));
}
