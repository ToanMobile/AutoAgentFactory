#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]
//! `accounts.selection` as the real binary shows it: `aaf accounts next`
//! and `aaf doctor` name the mode and the account a repository is bound to.

#[path = "factory_support.rs"]
mod support;

use std::fs;

use serde_json::{Value, json};
use support::Fixture;

/// A machine with two Claude Code profiles, `home` and `work`, and `tier1`
/// appended to the global config.
fn two_accounts(tier1: &str) -> Fixture {
    let fx = Fixture::new();
    let home = fx.root().join("home");
    for name in ["home", "work"] {
        let dir = home.join(format!(".claude-{name}"));
        fs::create_dir_all(dir.join("projects")).unwrap();
        fs::write(dir.join(".claude.json"), "{}").unwrap();
    }
    fs::write(
        home.join(".config/autofactory/config.toml"),
        format!("[machine]\nmax_load_percent = 0\n{tier1}"),
    )
    .unwrap();
    fx
}

fn bind(fx: &Fixture, account: &str) {
    fs::create_dir_all(fx.repo.join(".claude")).unwrap();
    fs::write(
        fx.repo.join(".claude/settings.local.json"),
        json!({ "account": account }).to_string(),
    )
    .unwrap();
}

#[test]
fn without_a_key_the_selection_is_pinned_and_next_follows_the_binding() {
    let fx = two_accounts("");

    // No settings entry, no history: the least-used account would become
    // the binding, and nothing is bound yet.
    let fresh = fx.aaf(&["accounts", "next"]);
    assert_eq!(fresh.code, 0, "{}", fresh.stderr);
    assert_eq!(fresh.json["selection"], json!("pinned"), "{}", fresh.stderr);
    assert_eq!(fresh.json["binding"]["account"], Value::Null);
    assert!(fresh.json["account"]["name"].is_string());

    bind(&fx, "work");
    let bound = fx.aaf(&["accounts", "next"]);
    assert_eq!(bound.code, 0, "{}", bound.stderr);
    assert_eq!(bound.json["account"]["name"], json!("work"));
    assert_eq!(bound.json["binding"]["account"]["name"], json!("work"));
    assert_eq!(bound.json["binding"]["source"], json!("settings"));

    // The bound account cools down: `home` is free, and is not offered.
    let cooled = fx.aaf(&["accounts", "cooldown", "work", "--until", "4102444800"]);
    assert_eq!(cooled.code, 0, "{}", cooled.stderr);
    let waiting = fx.aaf(&["accounts", "next"]);
    assert_eq!(waiting.code, 2, "{}", waiting.stderr);
    assert_eq!(waiting.json["account"], Value::Null);
    assert_eq!(waiting.json["binding"]["account"]["name"], json!("work"));
    assert_eq!(
        waiting.json["binding"]["cooldown_until"],
        json!(4_102_444_800_i64)
    );
    // The same for a human: which account, and until when.
    let mut plain = fx.command(&fx.repo, &["accounts", "next"]);
    let text = plain.output().unwrap();
    let text = String::from_utf8_lossy(&text.stdout).into_owned();
    assert!(
        text.contains("work") && text.contains("4102444800") && text.contains("pinned"),
        "{text}"
    );
}

#[test]
fn least_used_offers_another_account_when_one_cools_down() {
    let fx = two_accounts("\n[accounts]\nselection = \"least_used\"\n");
    bind(&fx, "work");
    let cooled = fx.aaf(&["accounts", "cooldown", "work"]);
    assert_eq!(cooled.code, 0, "{}", cooled.stderr);

    let next = fx.aaf(&["accounts", "next"]);

    assert_eq!(next.code, 0, "{}", next.stderr);
    assert_eq!(next.json["selection"], json!("least_used"));
    assert_eq!(next.json["account"]["name"], json!("home"));
    assert_eq!(next.json["binding"]["account"], Value::Null);
}

#[test]
fn an_unknown_selection_is_a_config_error() {
    let fx = two_accounts("\n[accounts]\nselection = \"round_robin\"\n");
    let next = fx.aaf(&["accounts", "next"]);
    assert_eq!(next.code, 1, "{}", next.stderr);
}

#[test]
fn doctor_shows_the_mode_and_the_binding() {
    let fx = two_accounts("");
    bind(&fx, "work");

    let doctor = fx.aaf(&["doctor"]);

    assert_eq!(doctor.json["accounts"]["selection"], json!("pinned"));
    assert_eq!(
        doctor.json["accounts"]["binding"]["account"]["name"],
        json!("work")
    );
    assert_eq!(
        doctor.json["accounts"]["binding"]["source"],
        json!("settings")
    );
    let checks = doctor.json["checks"].as_array().unwrap();
    let accounts = checks.iter().find(|c| c["name"] == "accounts").unwrap();
    let detail = accounts["detail"].as_str().unwrap();
    assert!(
        detail.contains("selection=pinned") && detail.contains("bound to work"),
        "{detail}"
    );
}
