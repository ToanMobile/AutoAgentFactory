use std::ffi::OsString;
use std::fs;
use std::path::Path;

use serde_json::{Value, json};

use super::settings::{
    HookChange, command_program, find_hooks, matcher, matcher_covers_all, merge_hook, shell_quote,
    strip_empty_hooks_object,
};
use super::{
    HookSource, gitignore_unignore_settings, gitignore_with_local, hook_command, resolve_hook,
};

fn touch(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, "").unwrap();
}

/// Was `hook_next_to_the_binary_wins_over_path`: reversed on purpose. The
/// settings file is committed and travels to other machines and to task
/// worktrees, so the form that survives there is preferred.
#[test]
fn hook_on_path_is_registered_by_name_even_when_one_sits_next_to_the_binary() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let exe = root.join("install/aaf");
    touch(&exe);
    touch(&root.join("install/aaf-hook"));
    touch(&root.join("onpath/aaf-hook"));
    let path_var = OsString::from(root.join("onpath"));

    let (found, source) = resolve_hook(Some(&exe), &path_var).unwrap();

    assert_eq!(source, HookSource::Path);
    assert_eq!(found, root.join("onpath/aaf-hook"));
    assert_eq!(hook_command(&found, source).unwrap(), "aaf-hook");
}

#[test]
fn hook_only_next_to_the_binary_is_registered_by_its_absolute_path() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let exe = root.join("my install/aaf");
    touch(&exe);
    touch(&root.join("my install/aaf-hook"));
    let path_var = OsString::from(root.join("empty"));

    let (found, source) = resolve_hook(Some(&exe), &path_var).unwrap();

    assert_eq!(source, HookSource::Sibling);
    assert_eq!(found, root.join("my install/aaf-hook"));
    // Quoted for the shell Claude Code runs it through.
    let command = hook_command(&found, source).unwrap();
    assert_eq!(command_program(&command).as_deref(), found.to_str());
    assert!(command.starts_with('\''), "{command}");
}

#[test]
fn tracking_is_read_from_one_line_of_git_status() {
    use super::settings::Tracking;
    for (status, expected) in [
        ("", Tracking::Committed),
        ("?? .claude/", Tracking::Untracked),
        ("?? .claude/settings.json", Tracking::Untracked),
        ("!! .claude/", Tracking::Ignored),
        ("M .claude/settings.json", Tracking::Modified),
        ("A  .claude/settings.json", Tracking::Modified),
        ("AM .claude/settings.json", Tracking::Modified),
    ] {
        assert_eq!(Tracking::from_status(status), expected, "{status:?}");
    }
}

#[test]
fn hook_falls_back_to_path_and_is_none_when_nowhere() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let exe = root.join("install/aaf");
    touch(&exe);
    touch(&root.join("onpath/aaf-hook"));
    let path_var = OsString::from(root.join("onpath"));

    let (found, source) = resolve_hook(Some(&exe), &path_var).unwrap();
    assert_eq!(source, HookSource::Path);
    assert_eq!(found, root.join("onpath/aaf-hook"));

    assert_eq!(resolve_hook(None, &path_var).unwrap().1, HookSource::Path);
    assert!(resolve_hook(Some(&exe), &OsString::from(root.join("empty"))).is_none());
}

#[test]
fn quoting_round_trips_through_the_program_parser() {
    assert_eq!(
        shell_quote("/usr/local/bin/aaf-hook"),
        "/usr/local/bin/aaf-hook"
    );
    for path in [
        "/usr/local/bin/aaf-hook",
        "/Users/a b/bin/aaf-hook",
        "/odd/it's $HOME;x/aaf-hook",
    ] {
        assert_eq!(command_program(&shell_quote(path)).as_deref(), Some(path));
    }
    assert_eq!(
        command_program("\"/a b/aaf-hook\" --flag").as_deref(),
        Some("/a b/aaf-hook")
    );
    assert_eq!(command_program("   "), None);
}

#[test]
fn matcher_must_name_all_five_tools() {
    assert!(matcher_covers_all(&matcher()));
    assert!(matcher_covers_all("Read|Bash|Write|Edit|MultiEdit|Glob"));
    assert!(matcher_covers_all("*"));
    assert!(matcher_covers_all(""));
    assert!(!matcher_covers_all("Bash|Write|Edit|MultiEdit"));
    assert!(!matcher_covers_all("Bash"));
}

#[test]
fn merge_into_a_missing_file_adds_one_entry_and_is_then_unchanged() {
    let (text, change) = merge_hook(None, "/bin/aaf-hook").unwrap();
    let text = text.unwrap();
    assert_eq!(change, HookChange::Added);
    let hooks = find_hooks(&text).unwrap();
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0].matcher, matcher());
    assert_eq!(hooks[0].command, "/bin/aaf-hook");

    let (again, change) = merge_hook(Some(&text), "/bin/aaf-hook").unwrap();
    assert_eq!(change, HookChange::Unchanged);
    assert_eq!(again, None);
}

#[test]
fn merge_replaces_a_stale_registration_and_keeps_the_foreign_hook_beside_it() {
    let existing = json!({
        "model": "x",
        "hooks": {"PreToolUse": [
            {"matcher": "Bash", "hooks": [
                {"type": "command", "command": "/old/place/aaf-hook"},
                {"type": "command", "command": "/usr/bin/other"}
            ]},
            {"matcher": "Bash", "hooks": [{"type": "command", "command": "'/old two/aaf-hook'"}]},
            {"matcher": "Write", "hooks": []}
        ]}
    })
    .to_string();

    let (text, change) = merge_hook(Some(&existing), "/new/aaf-hook").unwrap();

    assert_eq!(change, HookChange::Updated);
    let merged: Value = serde_json::from_str(&text.unwrap()).unwrap();
    assert_eq!(merged["model"], json!("x"));
    assert_eq!(
        merged["hooks"]["PreToolUse"],
        json!([
            {"matcher": "Bash", "hooks": [{"type": "command", "command": "/usr/bin/other"}]},
            {"matcher": "Write", "hooks": []},
            {"matcher": matcher(), "hooks": [{"type": "command", "command": "/new/aaf-hook"}]}
        ])
    );
}

#[test]
fn merge_refuses_what_it_cannot_safely_rewrite() {
    for broken in [
        "{ not json",
        "[1, 2]",
        "{\"hooks\": []}",
        "{\"hooks\": {\"PreToolUse\": {}}}",
    ] {
        assert!(
            merge_hook(Some(broken), "/bin/aaf-hook").is_err(),
            "{broken}"
        );
    }
    // A hook that merely mentions the name in an argument is not ours.
    let foreign = json!({"hooks": {"PreToolUse": [
        {"matcher": "Bash", "hooks": [{"type": "command", "command": "echo aaf-hook"}]}
    ]}})
    .to_string();
    assert_eq!(find_hooks(&foreign).unwrap(), []);
}

#[test]
fn gitignore_line_is_added_once_and_keeps_existing_lines() {
    let local_cfg = ".autofactory/config.local.toml";
    let local_settings = ".claude/settings.local.json";
    assert_eq!(
        gitignore_with_local(None),
        Some(format!("{local_cfg}\n{local_settings}\n"))
    );
    assert_eq!(
        gitignore_with_local(Some("target")),
        Some(format!("target\n{local_cfg}\n{local_settings}\n"))
    );
    assert_eq!(
        gitignore_with_local(Some(&format!("a\n{local_cfg}\nb\n"))),
        Some(format!("a\n{local_cfg}\nb\n{local_settings}\n"))
    );
    assert_eq!(
        gitignore_with_local(Some(&format!("a\n{local_cfg}\n{local_settings}\nb\n"))),
        None
    );
    assert_eq!(
        gitignore_with_local(Some(&format!("/{local_cfg}\n/{local_settings}\n"))),
        None
    );
}

#[test]
fn gitignore_unignore_settings_handles_claude_dir_and_file_rules() {
    let dir_ignored = ".claude/\n.autofactory/config.local.toml\n.claude/settings.local.json\n";
    let updated = gitignore_unignore_settings(Some(dir_ignored)).unwrap();
    assert_eq!(
        updated,
        format!("{dir_ignored}!.claude/\n.claude/*\n!.claude/settings.json\n")
    );
    assert_eq!(gitignore_unignore_settings(Some(&updated)), None);

    let pattern_ignored = "*.json\n";
    let updated = gitignore_unignore_settings(Some(pattern_ignored)).unwrap();
    assert_eq!(updated, "*.json\n!.claude/settings.json\n");
    assert_eq!(gitignore_unignore_settings(Some(&updated)), None);
}

#[test]
fn strip_empty_hooks_object_cleans_only_empty_object() {
    let with_empty = json!({"permissions": {"allow": ["Bash(ls:*)"]}, "hooks": {}}).to_string();
    let cleaned = strip_empty_hooks_object(&with_empty).unwrap();
    let parsed: Value = serde_json::from_str(&cleaned).unwrap();
    assert!(parsed.get("hooks").is_none(), "{cleaned}");
    assert_eq!(parsed["permissions"]["allow"], json!(["Bash(ls:*)"]));
    assert_eq!(strip_empty_hooks_object(&cleaned), None);

    let non_empty = json!({"hooks": {"Stop": []}}).to_string();
    assert_eq!(strip_empty_hooks_object(&non_empty), None);
    assert_eq!(strip_empty_hooks_object("{\"hooks\": []}"), None);
    assert_eq!(strip_empty_hooks_object("{\"permissions\": {}}"), None);
    assert_eq!(strip_empty_hooks_object("{ not json"), None);
}
