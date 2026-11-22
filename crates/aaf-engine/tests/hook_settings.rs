#![allow(clippy::unwrap_used, clippy::expect_used)]
//! What counts as "the hook is registered": the registration must send
//! every guarded tool to `aaf-hook` and nothing in the file may take its
//! verdict away again. The adversary is an agent that can write any file in
//! its worktree.

use aaf_engine::hook_settings::{covering_hook, merge_hook};

fn settings(command: &str) -> String {
    serde_json::json!({
        "hooks": {"PreToolUse": [{
            "matcher": "Bash|Write|Edit|MultiEdit|Read",
            "hooks": [{"type": "command", "command": command}],
        }]}
    })
    .to_string()
}

#[test]
fn a_command_that_is_one_program_is_accepted() {
    for command in [
        "aaf-hook",
        "  aaf-hook  ",
        "/usr/local/bin/aaf-hook",
        "'/opt/my tools/aaf-hook'",
        "\"/opt/my tools/aaf-hook\"",
        "'/opt/it'\\''s/aaf-hook'",
    ] {
        let hook = covering_hook(&settings(command));
        assert!(hook.is_ok(), "{command:?}: {hook:?}");
    }
}

#[test]
fn a_command_with_anything_after_the_program_is_rejected() {
    for command in [
        "aaf-hook >/dev/null 2>&1 || true",
        "aaf-hook || true",
        "aaf-hook; true",
        "aaf-hook | cat",
        "aaf-hook & true",
        "aaf-hook --dry-run",
        "aaf-hook>/dev/null",
        "aaf-hook\ntrue",
        "aaf-hook # ",
        "X=/tmp/aaf-hook",
        "\"$(echo /x)/aaf-hook\"",
        "\"`echo /x`/aaf-hook\"",
        "$HOME/bin/aaf-hook",
        "~/bin/aaf-hook",
        "/opt/*/aaf-hook",
    ] {
        let problem = covering_hook(&settings(command)).unwrap_err();
        // Either the entry is ours and more than the program, or its first
        // word is not `aaf-hook` at all (`aaf-hook;`): no registration.
        assert!(
            problem.contains("only the program") || problem.contains("no PreToolUse entry"),
            "{command:?}: {problem}"
        );
    }
}

#[test]
fn disable_all_hooks_is_rejected_whatever_else_the_file_says() {
    let mut root: serde_json::Value = serde_json::from_str(&settings("aaf-hook")).unwrap();
    root["disableAllHooks"] = serde_json::json!(true);
    let problem = covering_hook(&root.to_string()).unwrap_err();
    assert!(problem.contains("disableAllHooks"), "{problem}");

    // Explicitly off is what the key's absence means.
    root["disableAllHooks"] = serde_json::json!(false);
    assert!(covering_hook(&root.to_string()).is_ok());
    // Anything else is not a value Claude Code documents: fail closed.
    root["disableAllHooks"] = serde_json::json!("true");
    assert!(covering_hook(&root.to_string()).is_err());
}

#[test]
fn a_hook_entry_that_cannot_block_is_rejected() {
    for (key, value) in [
        ("async", serde_json::json!(true)),
        ("type", serde_json::json!("prompt")),
        ("if", serde_json::json!("Bash(git *)")),
    ] {
        let mut root: serde_json::Value = serde_json::from_str(&settings("aaf-hook")).unwrap();
        root["hooks"]["PreToolUse"][0]["hooks"][0][key] = value;
        let problem = covering_hook(&root.to_string()).unwrap_err();
        assert!(problem.contains(key), "{key}: {problem}");
    }
    // A timeout only bounds the hook.
    let mut root: serde_json::Value = serde_json::from_str(&settings("aaf-hook")).unwrap();
    root["hooks"]["PreToolUse"][0]["hooks"][0]["timeout"] = serde_json::json!(30);
    assert!(covering_hook(&root.to_string()).is_ok());
}

#[test]
fn aaf_init_replaces_a_neutered_registration() {
    let neutered = settings("aaf-hook || true");
    let (text, _) = merge_hook(Some(&neutered), "aaf-hook").unwrap();
    let text = text.expect("the file is rewritten");
    assert!(covering_hook(&text).is_ok(), "{text}");
    assert!(!text.contains("|| true"), "{text}");
}
