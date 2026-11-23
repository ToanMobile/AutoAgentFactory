//! `aaf doctor` through the built binary.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "cli_support.rs"]
mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;

use serde_json::{Value, json};
use support::{Sandbox, write};

fn check<'a>(out: &'a Value, name: &str) -> &'a Value {
    out["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == json!(name))
        .unwrap_or_else(|| panic!("no check {name}: {out}"))
}

#[test]
fn doctor_passes_after_init_and_every_check_has_the_three_keys() {
    let sb = Sandbox::new();
    assert_eq!(sb.json(&["init"]).0, 0);

    let (code, out) = sb.json(&["doctor"]);

    assert_eq!(code, 0, "{out}");
    assert_eq!(out["status"], json!("ok"));
    for c in out["checks"].as_array().unwrap() {
        assert!(c["name"].is_string() && c["ok"].is_boolean() && c["detail"].is_string());
    }
    for name in [
        "git",
        "repo_root",
        "config",
        "hook_registered",
        "hook_binary",
        "data_dir",
        "receipt_key",
        "claude",
        "sandbox",
        "accounts",
        "machine",
    ] {
        check(&out, name);
    }
    for name in [
        "git",
        "repo_root",
        "config",
        "hook_registered",
        "hook_binary",
    ] {
        assert_eq!(check(&out, name)["ok"], json!(true), "{name}: {out}");
    }
    assert_eq!(out["machine"]["max_load_percent"], json!(150));
    // Doctor only looks: it must not create the data dir.
    assert!(!sb.data.exists());
}

#[test]
fn doctor_exits_2_when_the_hook_is_not_registered() {
    let sb = Sandbox::new();

    let (code, out) = sb.json(&["doctor"]);

    assert_eq!(code, 2, "{out}");
    assert_eq!(out["ok"], json!(false));
    assert_eq!(check(&out, "hook_registered")["ok"], json!(false));
    assert_eq!(check(&out, "hook_registered")["required"], json!(true));
}

#[test]
fn doctor_exits_2_when_the_registered_hook_binary_is_gone() {
    let sb = Sandbox::new();
    write(
        &sb.repo.join(".claude/settings.json"),
        &json!({"hooks": {"PreToolUse": [{
            "matcher": "Bash|Write|Edit|MultiEdit|Read",
            "hooks": [{"type": "command", "command": "/nonexistent/dir/aaf-hook"}]
        }]}})
        .to_string(),
    );

    let (code, out) = sb.json(&["doctor"]);

    assert_eq!(code, 2, "{out}");
    assert_eq!(check(&out, "hook_registered")["ok"], json!(true));
    assert_eq!(check(&out, "hook_binary")["ok"], json!(false));
}

#[test]
fn doctor_rejects_a_matcher_that_leaves_a_tool_unguarded() {
    let sb = Sandbox::new();
    let hook = sb.bin.join("aaf-hook");
    write(
        &sb.repo.join(".claude/settings.json"),
        &json!({"hooks": {"PreToolUse": [{
            "matcher": "Bash",
            "hooks": [{"type": "command", "command": hook.to_str().unwrap()}]
        }]}})
        .to_string(),
    );

    let (code, out) = sb.json(&["doctor"]);

    assert_eq!(code, 2, "{out}");
    assert_eq!(check(&out, "hook_registered")["ok"], json!(false));
}

#[test]
fn doctor_flags_a_receipt_key_that_others_can_read() {
    let sb = Sandbox::new();
    assert_eq!(sb.json(&["init"]).0, 0);
    let key = sb.data.join("receipt.key");
    write(&key, &"k".repeat(32));

    fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
    let (code, out) = sb.json(&["doctor"]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(check(&out, "receipt_key")["ok"], json!(false));

    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    let (code, out) = sb.json(&["doctor"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(check(&out, "receipt_key")["ok"], json!(true));
    assert_eq!(check(&out, "data_dir")["ok"], json!(true));
}

#[test]
fn doctor_lists_config_warnings_and_fails_on_a_config_that_does_not_load() {
    let sb = Sandbox::new();
    assert_eq!(sb.json(&["init"]).0, 0);
    let local = sb.repo.join(".autofactory/config.local.toml");
    write(&local, "[commands]\ntest_scoped = \"true\"\n");

    let (code, out) = sb.json(&["doctor"]);
    assert_eq!(code, 0, "{out}");
    let warnings = out["config_warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1, "{out}");
    assert!(warnings[0].as_str().unwrap().contains("[commands]"));
    assert!(
        check(&out, "config")["detail"]
            .as_str()
            .unwrap()
            .contains("[commands]")
    );

    write(&local, "[gate]\nno_such_key = 1\n");
    let (code, out) = sb.json(&["doctor"]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(check(&out, "config")["ok"], json!(false));
}

/// Writes an executable script.
fn script(path: &std::path::Path, text: &str) {
    write(path, text);
    support::make_executable(path);
}

#[test]
fn doctor_warns_about_a_git_hook_that_starts_gradle_outside_the_queue() {
    let sb = Sandbox::new();
    assert_eq!(sb.json(&["init"]).0, 0);
    let hook = sb.repo.join(".git/hooks/pre-commit");

    // No hook at all.
    let (_, out) = sb.json(&["doctor"]);
    let none = check(&out, "git_hooks_heavy");
    assert_eq!(none["ok"], json!(true), "{none}");
    assert_eq!(none["required"], json!(false));
    assert!(none["detail"].as_str().unwrap().contains("no pre-commit"));

    script(
        &hook,
        "#!/bin/sh\nset -e\n./gradlew :app:lintDebug --no-daemon\n",
    );
    let (code, out) = sb.json(&["doctor"]);
    // Advisory: it warns, the exit code stays 0.
    assert_eq!(code, 0, "{out}");
    let bare = check(&out, "git_hooks_heavy");
    assert_eq!(bare["ok"], json!(false), "{bare}");
    let detail = bare["detail"].as_str().unwrap();
    assert!(
        detail.contains(".git/hooks/pre-commit:3 starts gradle"),
        "{detail}"
    );
    assert!(
        detail.contains("aaf slot -- ./gradlew :app:lintDebug --no-daemon"),
        "{detail}"
    );

    script(
        &hook,
        "#!/bin/sh\nset -e\n# ./gradlew lint\necho gradle\naaf slot -- ./gradlew :app:lintDebug\n",
    );
    let (_, out) = sb.json(&["doctor"]);
    let wrapped = check(&out, "git_hooks_heavy");
    assert_eq!(wrapped["ok"], json!(true), "{wrapped}");
    assert!(
        wrapped["detail"]
            .as_str()
            .unwrap()
            .starts_with("pre-commit:")
    );
}

#[test]
fn doctor_reads_a_command_continued_over_lines_as_one_command() {
    let sb = Sandbox::new();
    assert_eq!(sb.json(&["init"]).0, 0);
    let hook = sb.repo.join(".git/hooks/pre-commit");

    // Queued: the build tool on the second line belongs to `aaf slot`.
    script(
        &hook,
        "#!/bin/sh\nset -e\naaf slot -- \\\n  ./gradlew :app:lintDebug\n",
    );
    let (_, out) = sb.json(&["doctor"]);
    let queued = check(&out, "git_hooks_heavy");
    assert_eq!(queued["ok"], json!(true), "{queued}");

    // Also over more than two lines.
    script(
        &hook,
        "#!/bin/sh\naaf slot --label lint \\\n  --timeout 600 -- \\\n  ./gradlew lint\n",
    );
    let (_, out) = sb.json(&["doctor"]);
    let queued = check(&out, "git_hooks_heavy");
    assert_eq!(queued["ok"], json!(true), "{queued}");

    // A continuation is no excuse of its own: nothing here is queued, and
    // the finding names the line the command starts on.
    script(&hook, "#!/bin/sh\nset -e\necho hi \\\n./gradlew lint\n");
    let (_, out) = sb.json(&["doctor"]);
    let bare = check(&out, "git_hooks_heavy");
    assert_eq!(bare["ok"], json!(false), "{bare}");
    let detail = bare["detail"].as_str().unwrap();
    assert!(
        detail.contains(".git/hooks/pre-commit:3 starts gradle"),
        "{detail}"
    );
    assert!(!detail.contains("pre-commit:4"), "{detail}");

    // The build tool at the start of a continued command.
    script(
        &hook,
        "#!/bin/sh\n./gradlew \\\n  :app:lintDebug \\\n  --no-daemon\ntrue\n",
    );
    let (_, out) = sb.json(&["doctor"]);
    let bare = check(&out, "git_hooks_heavy");
    let detail = bare["detail"].as_str().unwrap();
    assert!(
        detail.contains(".git/hooks/pre-commit:2 starts gradle")
            && detail.contains("aaf slot -- ./gradlew :app:lintDebug --no-daemon"),
        "{detail}"
    );

    // Only the queued command is excused, not the one on the line after it.
    script(
        &hook,
        "#!/bin/sh\naaf slot -- \\\n  ./gradlew lint\n./gradlew test\n",
    );
    let (_, out) = sb.json(&["doctor"]);
    let bare = check(&out, "git_hooks_heavy");
    let detail = bare["detail"].as_str().unwrap();
    assert!(
        detail.contains("pre-commit:4 starts gradle") && !detail.contains("pre-commit:2"),
        "{detail}"
    );
}

#[test]
fn doctor_follows_core_hooks_path_and_reads_the_scripts_a_hook_names() {
    let sb = Sandbox::new();
    assert_eq!(sb.json(&["init"]).0, 0);
    sb.git(&["config", "core.hooksPath", ".githooks"]);
    // A hook in the default directory is not the one git runs now.
    script(
        &sb.repo.join(".git/hooks/pre-commit"),
        "#!/bin/sh\nxcodebuild test\n",
    );
    script(
        &sb.repo.join(".githooks/pre-push"),
        "#!/bin/sh\nexec \"$(git rev-parse --show-toplevel)/scripts/check.sh\" \"$@\"\n",
    );
    script(
        &sb.repo.join("scripts/check.sh"),
        "#!/bin/sh\ncd android\n./gradlew test\n",
    );
    // The wrapper itself is the build tool, not a script to read.
    script(&sb.repo.join("gradlew"), "#!/bin/sh\nexec gradle \"$@\"\n");

    let (_, out) = sb.json(&["doctor"]);
    let found = check(&out, "git_hooks_heavy");
    assert_eq!(found["ok"], json!(false), "{found}");
    let detail = found["detail"].as_str().unwrap();
    assert!(
        detail.contains("scripts/check.sh:3 starts gradle"),
        "{detail}"
    );
    assert!(!detail.contains("xcodebuild"), "{detail}");
    assert!(!detail.contains("gradlew:2"), "{detail}");

    write(
        &sb.repo.join("scripts/check.sh"),
        "#!/bin/sh\ncd android\naaf slot -- ./gradlew test\n",
    );
    let (_, out) = sb.json(&["doctor"]);
    assert_eq!(check(&out, "git_hooks_heavy")["ok"], json!(true), "{out}");
}

#[test]
fn doctor_warns_when_a_gate_command_runs_gradle_with_the_daemon_on() {
    let sb = Sandbox::new();
    assert_eq!(sb.json(&["init"]).0, 0);
    let config = sb.repo.join(".autofactory/config.toml");
    let name = "gradle_daemon_config";

    // Whatever `init` detected here, it is not Gradle.
    let (_, out) = sb.json(&["doctor"]);
    assert_eq!(check(&out, name)["ok"], json!(true), "{out}");
    // The list of live daemons is there too; what it says depends on the
    // machine.
    assert_eq!(check(&out, "gradle_daemon")["required"], json!(false));

    write(&config, "[commands]\ntest_full = \"./gradlew test\"\n");
    let (code, out) = sb.json(&["doctor"]);
    assert_eq!(code, 0, "{out}");
    let on = check(&out, name);
    assert_eq!(on["ok"], json!(false), "{on}");
    assert!(
        on["detail"]
            .as_str()
            .unwrap()
            .contains("commands.test_full")
    );

    write(
        &sb.repo.join("gradle.properties"),
        "org.gradle.jvmargs=-Xmx4g\norg.gradle.daemon = false\n",
    );
    let (_, out) = sb.json(&["doctor"]);
    assert_eq!(check(&out, name)["ok"], json!(true), "{out}");

    write(
        &sb.repo.join("gradle.properties"),
        "#org.gradle.daemon=false\norg.gradle.daemon=true\n",
    );
    let (_, out) = sb.json(&["doctor"]);
    assert_eq!(check(&out, name)["ok"], json!(false), "{out}");

    write(
        &config,
        "[commands]\ntest_full = \"./gradlew test --no-daemon\"\n",
    );
    let (_, out) = sb.json(&["doctor"]);
    assert_eq!(check(&out, name)["ok"], json!(true), "{out}");
}

/// Was `doctor_outside_a_repo_fails_the_repo_check_in_text_mode_too` (exit 2,
/// "FAIL repo_root"): reversed on purpose for the machine-level doctor of
/// docs/PLAN.md E.5.
#[test]
fn doctor_outside_a_repo_marks_the_repo_checks_not_applicable_in_text_mode_too() {
    let sb = Sandbox::without_git();

    let out = sb.aaf(&["doctor"]);

    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("n/a  repo_root"), "{text}");
    assert!(text.contains("n/a  hook_registered"), "{text}");
    assert!(!text.contains("FAIL"), "{text}");
}

/// PLAN E.5: the doctor of a machine, not only of a repository. Outside a
/// git work tree the repository checks do not apply and must not fail it.
#[test]
fn doctor_outside_a_repository_checks_the_machine_and_exits_0() {
    let sb = Sandbox::without_git();
    let (code, out) = sb.json(&["doctor"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["status"], json!("ok"));
    for name in ["repo_root", "hook_registered", "hook_binary"] {
        let c = check(&out, name);
        assert_eq!(c["applicable"], json!(false), "{name}: {out}");
        assert_eq!(c["required"], json!(false), "{name}: {out}");
        let detail = c["detail"].as_str().unwrap();
        assert!(detail.starts_with("not applicable"), "{name}: {detail}");
    }
    for name in ["git", "config", "data_dir", "receipt_key", "machine"] {
        let c = check(&out, name);
        assert_eq!(c["applicable"], json!(true), "{name}: {out}");
        assert_eq!(c["ok"], json!(true), "{name}: {out}");
    }
    let text = String::from_utf8(sb.aaf(&["doctor"]).stdout).unwrap();
    assert!(text.contains("n/a  repo_root: not applicable"), "{text}");
}

#[test]
fn doctor_outside_a_repository_still_fails_on_a_machine_level_problem() {
    let sb = Sandbox::without_git();
    let key = sb.data.join("receipt.key");
    write(&key, &"k".repeat(32));
    fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
    let (code, out) = sb.json(&["doctor"]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(check(&out, "receipt_key")["ok"], json!(false));
}

#[test]
fn doctor_inside_a_repository_still_requires_the_hook() {
    let sb = Sandbox::new();
    let (code, out) = sb.json(&["doctor"]);
    assert_eq!(code, 2, "{out}");
    let c = check(&out, "hook_registered");
    assert_eq!(c["applicable"], json!(true));
    assert_eq!(c["required"], json!(true));
    assert_eq!(c["ok"], json!(false));
}

#[test]
fn doctor_counts_a_gradle_lint_among_the_commands_that_need_the_daemon_off() {
    let sb = Sandbox::new();
    assert_eq!(sb.json(&["init"]).0, 0);
    write(
        &sb.repo.join(".autofactory/config.toml"),
        "[commands]\nlint = \"./gradlew lint\"\n",
    );
    let (_, out) = sb.json(&["doctor"]);
    let c = check(&out, "gradle_daemon_config");
    assert_eq!(c["ok"], json!(false), "{out}");
    assert!(
        c["detail"].as_str().unwrap().contains("commands.lint"),
        "{out}"
    );
}

/// `hook_registered` looks at the working tree. An agent of `aaf run` works
/// in a worktree cut from a commit, where only a committed settings file
/// exists.
#[test]
fn doctor_warns_while_the_hook_settings_are_not_what_is_committed() {
    let sb = Sandbox::new();
    assert_eq!(sb.json(&["init"]).0, 0);

    // Untracked: registered here, absent in every task worktree.
    let (code, out) = sb.json(&["doctor"]);
    assert_eq!(code, 0, "{out}");
    let committed = check(&out, "hook_committed");
    assert_eq!(committed["ok"], json!(false), "{out}");
    assert_eq!(committed["required"], json!(false));
    assert!(
        committed["detail"].as_str().unwrap().contains("untracked"),
        "{out}"
    );
    let registered = check(&out, "hook_registered");
    assert_eq!(registered["ok"], json!(true));
    assert!(
        registered["detail"]
            .as_str()
            .unwrap()
            .contains("warning: not committed"),
        "{out}"
    );

    sb.commit_all("aaf init");
    let (_, out) = sb.json(&["doctor"]);
    assert_eq!(check(&out, "hook_committed")["ok"], json!(true), "{out}");
    assert!(
        !check(&out, "hook_registered")["detail"]
            .as_str()
            .unwrap()
            .contains("warning")
    );

    // Edited after the commit: the task worktrees still get the old one.
    let path = sb.repo.join(".claude/settings.json");
    let edited = fs::read_to_string(&path)
        .unwrap()
        .replace("\"hooks\"", "\"note\": 1, \"hooks\"");
    write(&path, &edited);
    let (_, out) = sb.json(&["doctor"]);
    let committed = check(&out, "hook_committed");
    assert_eq!(committed["ok"], json!(false), "{out}");
    assert!(
        committed["detail"]
            .as_str()
            .unwrap()
            .contains("differs from HEAD"),
        "{out}"
    );
}

#[test]
fn doctor_warns_when_git_ignores_the_hook_settings() {
    let sb = Sandbox::new();
    write(&sb.repo.join(".gitignore"), ".claude/\n");
    sb.commit_all("ignore claude");
    assert_eq!(sb.json(&["init"]).0, 0);
    let (_, out) = sb.json(&["doctor"]);
    let committed = check(&out, "hook_committed");
    assert_eq!(committed["ok"], json!(false), "{out}");
    assert!(
        committed["detail"].as_str().unwrap().contains("ignored"),
        "{out}"
    );
}

#[test]
fn doctor_warns_when_generic_executor_has_positive_token_budget() {
    let sb = Sandbox::new();
    assert_eq!(sb.json(&["init"]).0, 0);
    let (code, out) = sb.json(&["doctor"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(check(&out, "executor_token_budget")["ok"], json!(true));

    let cfg_path = sb.repo.join(".autofactory/config.toml");
    write(
        &cfg_path,
        "[run]\nexecutor = \"gemini\"\nexecutor_cmd = [\"gemini\", \"-p\", \"{prompt}\"]\n\
         [unblocker]\nmax_token_budget = 500000\n",
    );
    let (code, out) = sb.json(&["doctor"]);
    assert_eq!(code, 0, "{out}");
    let c = check(&out, "executor_token_budget");
    assert_eq!((&c["ok"], &c["required"]), (&json!(false), &json!(false)));
    let detail = c["detail"].as_str().unwrap();
    assert!(
        detail.contains("max_token_budget=500000") && detail.contains("not enforced"),
        "{detail}"
    );

    write(
        &cfg_path,
        "[run]\nexecutor = \"claude\"\n[cross_audit]\nexecutor = \"gemini\"\n\
         [unblocker]\nmax_token_budget = 100000\n",
    );
    let (code, out) = sb.json(&["doctor"]);
    assert_eq!(code, 0, "{out}");
    let c = check(&out, "executor_token_budget");
    assert_eq!(c["ok"], json!(false), "{out}");
    assert!(
        c["detail"]
            .as_str()
            .unwrap()
            .contains("cross_audit.executor")
    );

    write(
        &cfg_path,
        "[run]\nexecutor = \"gemini\"\nexecutor_cmd = [\"gemini\", \"-p\", \"{prompt}\"]\n\
         [unblocker]\nmax_token_budget = 0\n",
    );
    let (code, out) = sb.json(&["doctor"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(check(&out, "executor_token_budget")["ok"], json!(true));

    let outside = Sandbox::without_git();
    let (_, out) = outside.json(&["doctor"]);
    assert_eq!(
        check(&out, "executor_token_budget")["applicable"],
        json!(false)
    );
}
