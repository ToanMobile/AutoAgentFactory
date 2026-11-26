//! `aaf init` against throwaway repos, through the built binary.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "cli_support.rs"]
mod support;

use std::fs;
use std::path::Path;

use serde_json::{Value, json};
use support::{Sandbox, write};

const MATCHER: &str = "Bash|Write|Edit|MultiEdit|Read";
const LOCAL: &str = ".autofactory/config.local.toml";
const LOCAL_SETTINGS: &str = ".claude/settings.local.json";

fn settings(repo: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(repo.join(".claude/settings.json")).unwrap()).unwrap()
}

/// Our entries under `hooks.PreToolUse`.
fn our_entries(settings: &Value) -> Vec<&Value> {
    settings["hooks"]["PreToolUse"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| {
            entry["hooks"].as_array().is_some_and(|hooks| {
                hooks.iter().any(|h| {
                    h["command"]
                        .as_str()
                        .is_some_and(|c| c.contains("aaf-hook"))
                })
            })
        })
        .collect()
}

#[test]
fn init_writes_config_registers_hook_and_ignores_the_local_file() {
    let sb = Sandbox::new();
    write(&sb.repo.join("Cargo.toml"), "[package]\nname = \"x\"\n");

    let (code, out) = sb.json(&["init"]);

    assert_eq!(code, 0, "{out}");
    assert_eq!(out["ok"], json!(true));
    assert_eq!(out["command"], json!("init"));
    assert_eq!(out["profile"], json!("rust"));
    assert_eq!(out["config"]["action"], json!("written"));
    assert_eq!(out["hook"]["action"], json!("added"));
    assert_eq!(out["gitignore"]["action"], json!("added"));
    assert_eq!(out["gate_unverified_until_commands_set"], json!(false));

    let config = fs::read_to_string(sb.repo.join(".autofactory/config.toml")).unwrap();
    assert!(
        config.contains("test_scoped = \"cargo test {affected_packages}\""),
        "{config}"
    );

    let settings = settings(&sb.repo);
    let ours = our_entries(&settings);
    assert_eq!(ours.len(), 1);
    assert_eq!(ours[0]["matcher"], json!(MATCHER));
    assert_eq!(ours[0]["hooks"][0]["type"], json!("command"));

    // The reported binary is what the registration runs, and it exists.
    // (Before the hook was registered by name this asserted the absolute
    // path in the command; the sandbox has `aaf-hook` on PATH.)
    let binary = out["hook"]["binary"].as_str().unwrap();
    assert!(Path::new(binary).is_file(), "{binary}");
    assert_eq!(out["hook"]["source"], json!("path"));
    assert_eq!(ours[0]["hooks"][0]["command"], json!("aaf-hook"));

    let ignore = fs::read_to_string(sb.repo.join(".gitignore")).unwrap();
    assert_eq!(ignore.lines().filter(|l| *l == LOCAL).count(), 1);
    assert_eq!(ignore.lines().filter(|l| *l == LOCAL_SETTINGS).count(), 1);
}

#[test]
fn init_twice_changes_nothing_and_keeps_an_edited_config_without_force() {
    let sb = Sandbox::new();
    write(&sb.repo.join(".gitignore"), "target");
    assert_eq!(sb.json(&["init"]).0, 0);
    let config_path = sb.repo.join(".autofactory/config.toml");
    let edited = fs::read_to_string(&config_path)
        .unwrap()
        .replace("test_scoped = \"\"", "test_scoped = \"make test\"");
    fs::write(&config_path, &edited).unwrap();
    let settings_before = fs::read(sb.repo.join(".claude/settings.json")).unwrap();
    let ignore_before = fs::read_to_string(sb.repo.join(".gitignore")).unwrap();
    assert_eq!(
        ignore_before,
        format!("target\n{LOCAL}\n{LOCAL_SETTINGS}\n")
    );

    let (code, out) = sb.json(&["init"]);

    assert_eq!(code, 0, "{out}");
    assert_eq!(out["config"]["action"], json!("kept"));
    assert_eq!(out["hook"]["action"], json!("unchanged"));
    assert_eq!(out["gitignore"]["action"], json!("unchanged"));
    assert_eq!(fs::read_to_string(&config_path).unwrap(), edited);
    assert_eq!(
        fs::read(sb.repo.join(".claude/settings.json")).unwrap(),
        settings_before
    );
    assert_eq!(
        fs::read_to_string(sb.repo.join(".gitignore")).unwrap(),
        ignore_before
    );

    let (code, out) = sb.json(&["init", "--force"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["config"]["action"], json!("overwritten"));
    assert!(
        !fs::read_to_string(&config_path)
            .unwrap()
            .contains("make test")
    );
}

#[test]
fn init_merges_into_existing_settings_without_losing_anything() {
    let sb = Sandbox::new();
    let existing = json!({
        "permissions": {"allow": ["Bash(ls:*)"], "deny": []},
        "env": {"FOO": "bar"},
        "hooks": {
            "PreToolUse": [
                {"matcher": "Bash", "hooks": [{"type": "command", "command": "/usr/local/bin/other-guard"}]}
            ],
            "Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]
        }
    });
    write(
        &sb.repo.join(".claude/settings.json"),
        &serde_json::to_string_pretty(&existing).unwrap(),
    );

    let (code, out) = sb.json(&["init"]);

    assert_eq!(code, 0, "{out}");
    let merged = settings(&sb.repo);
    assert_eq!(merged["permissions"], existing["permissions"]);
    assert_eq!(merged["env"], existing["env"]);
    assert_eq!(merged["hooks"]["Stop"], existing["hooks"]["Stop"]);
    let pre = merged["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre.len(), 2);
    assert_eq!(pre[0], existing["hooks"]["PreToolUse"][0]);
    assert_eq!(our_entries(&merged).len(), 1);
}

#[test]
fn init_refuses_unparsable_settings_and_writes_nothing() {
    let sb = Sandbox::new();
    let broken = "{ \"permissions\": { not json";
    write(&sb.repo.join(".claude/settings.json"), broken);

    let (code, out) = sb.json(&["init"]);

    assert_eq!(code, 1, "{out}");
    assert_eq!(out["ok"], json!(false));
    assert!(
        out["error"].as_str().unwrap().contains("settings.json"),
        "{out}"
    );
    assert_eq!(
        fs::read_to_string(sb.repo.join(".claude/settings.json")).unwrap(),
        broken
    );
    assert!(!sb.repo.join(".autofactory/config.toml").exists());
    assert!(!sb.repo.join(".gitignore").exists());
}

#[test]
fn init_refuses_settings_whose_hooks_have_an_unexpected_shape() {
    let sb = Sandbox::new();
    let odd = "{\"hooks\": {\"PreToolUse\": \"nope\"}}";
    write(&sb.repo.join(".claude/settings.json"), odd);

    let (code, out) = sb.json(&["init"]);

    assert_eq!(code, 1, "{out}");
    assert_eq!(
        fs::read_to_string(sb.repo.join(".claude/settings.json")).unwrap(),
        odd
    );
}

#[test]
fn init_on_unity_writes_empty_commands_and_says_the_gate_is_unverified() {
    let sb = Sandbox::new();
    write(
        &sb.repo.join("ProjectSettings/ProjectVersion.txt"),
        "m_EditorVersion: 6000.0.1f1\n",
    );

    let out = sb.aaf(&["init"]);

    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("unity"), "{text}");
    assert!(text.contains("Unverified"), "{text}");
    let config = fs::read_to_string(sb.repo.join(".autofactory/config.toml")).unwrap();
    assert!(config.contains("test_scoped = \"\""), "{config}");

    let (_, json_out) = sb.json(&["init"]);
    assert_eq!(json_out["profile"], json!("unity"));
}

#[test]
fn init_outside_a_git_repo_is_a_runtime_error() {
    let sb = Sandbox::without_git();

    let (code, out) = sb.json(&["init"]);

    assert_eq!(code, 1, "{out}");
    assert_eq!(out["status"], json!("error"));
    assert!(!sb.repo.join(".autofactory").exists());
}

/// The settings file is committed, so the hook is registered in the form
/// that works on another machine and in a task worktree: by name, when
/// `aaf-hook` is on PATH (the sandbox puts one there).
#[test]
fn init_registers_the_hook_by_name_when_it_is_on_path_and_says_so() {
    let sb = Sandbox::new();

    let (code, out) = sb.json(&["init"]);

    assert_eq!(code, 0, "{out}");
    let settings = settings(&sb.repo);
    let ours = our_entries(&settings);
    assert_eq!(ours[0]["hooks"][0]["command"], json!("aaf-hook"));
    assert_eq!(out["hook"]["command"], json!("aaf-hook"));
    assert_eq!(out["hook"]["source"], json!("path"));
    assert_eq!(out["hook"]["registered_as"], json!("name"));
    // What the name resolves to on this machine, now.
    assert_eq!(
        out["hook"]["binary"],
        json!(sb.bin.join("aaf-hook").to_str().unwrap())
    );
    let text = String::from_utf8(sb.aaf(&["init"]).stdout).unwrap();
    assert!(text.contains("registered by name"), "{text}");
}

/// A task worktree is cut from a commit: a hook registered only in the
/// working tree does not exist for the agents of `aaf run`.
#[test]
fn init_says_that_the_settings_file_has_to_be_committed_until_it_is() {
    let sb = Sandbox::new();

    let (_, out) = sb.json(&["init"]);
    assert_eq!(out["hook"]["tracking"], json!("untracked"), "{out}");
    assert_eq!(out["hook"]["commit_required"], json!(true));
    let text = String::from_utf8(sb.aaf(&["init"]).stdout).unwrap();
    assert!(
        text.contains("git add .claude/settings.json .autofactory/config.toml .gitignore"),
        "{text}"
    );

    sb.commit_all("aaf init");
    let (_, out) = sb.json(&["init"]);
    assert_eq!(out["hook"]["tracking"], json!("committed"), "{out}");
    assert_eq!(out["hook"]["commit_required"], json!(false));
    let text = String::from_utf8(sb.aaf(&["init"]).stdout).unwrap();
    assert!(!text.contains("git add"), "{text}");
}

#[test]
fn init_says_when_git_ignores_the_settings_file() {
    let sb = Sandbox::new();
    write(&sb.repo.join(".gitignore"), ".claude/\n");
    sb.commit_all("ignore claude");

    let (code, out) = sb.json(&["init"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["hook"]["tracking"], json!("ignored"), "{out}");
    assert_eq!(out["hook"]["commit_required"], json!(true));
    let text = String::from_utf8(sb.aaf(&["init"]).stdout).unwrap();
    assert!(text.contains("ignored by git"), "{text}");
}

fn configure_git_identity(sb: &Sandbox) {
    sb.git(&["config", "user.name", "fixture"]);
    sb.git(&["config", "user.email", "f@example.invalid"]);
}

fn git_stdout(sb: &Sandbox, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&sb.repo)
        .args(args)
        .env("HOME", &sb.home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn init_cleans_empty_hooks_object_in_local_settings_without_touching_non_empty() {
    let sb = Sandbox::new();
    let local_path = sb.repo.join(LOCAL_SETTINGS);
    write(
        &local_path,
        &json!({
            "permissions": {"allow": ["Bash(ls:*)"]},
            "hooks": {}
        })
        .to_string(),
    );

    let (code, out) = sb.json(&["init"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["local_settings_cleaned"], json!(true), "{out}");
    let cleaned: Value = serde_json::from_str(&fs::read_to_string(&local_path).unwrap()).unwrap();
    assert!(cleaned.get("hooks").is_none(), "{cleaned}");
    assert_eq!(cleaned["permissions"]["allow"], json!(["Bash(ls:*)"]));

    let (code, out) = sb.json(&["init"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["local_settings_cleaned"], json!(false), "{out}");

    // Non-empty hooks object in local settings is left untouched.
    let non_empty =
        json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "true"}]}]}})
            .to_string();
    write(&local_path, &non_empty);
    let (code, out) = sb.json(&["init"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["local_settings_cleaned"], json!(false), "{out}");
    assert_eq!(fs::read_to_string(&local_path).unwrap(), non_empty);
}

#[test]
fn init_commit_stages_and_commits_only_aaf_files_in_normal_repo() {
    let sb = Sandbox::new();
    configure_git_identity(&sb);
    // Unrelated modified file and unrelated staged file must NOT be committed.
    write(&sb.repo.join("README.md"), "dirty-readme\n");
    write(&sb.repo.join("unrelated.txt"), "staged-unrelated\n");
    sb.git(&["add", "unrelated.txt"]);

    let (code, out) = sb.json(&["init", "--commit"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["hook"]["tracking"], json!("committed"), "{out}");
    assert_eq!(out["hook"]["commit_required"], json!(false), "{out}");
    assert_eq!(out["commit"]["committed"], json!(true), "{out}");

    let subject = git_stdout(&sb, &["log", "-1", "--pretty=%s"]);
    assert_eq!(
        subject.trim(),
        "chore(aaf): initialize AutoAgentFactory config and hooks"
    );
    let committed_files: Vec<String> = git_stdout(
        &sb,
        &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"],
    )
    .lines()
    .map(str::to_string)
    .collect();
    assert_eq!(
        committed_files,
        vec![
            ".autofactory/config.toml",
            ".claude/settings.json",
            ".gitignore",
        ]
    );
    let status = git_stdout(&sb, &["status", "--porcelain"]);
    assert!(status.contains("A  unrelated.txt"), "{status}");
    assert!(status.contains(" M README.md"), "{status}");

    // Second `aaf init --commit` is a clean no-op.
    let (code, out) = sb.json(&["init", "--commit"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["committed"], json!(false), "{out}");
    assert_eq!(out["hook"]["tracking"], json!("committed"), "{out}");
}

#[test]
fn init_commit_unignores_claude_dir_and_keeps_local_settings_ignored() {
    let sb = Sandbox::new();
    configure_git_identity(&sb);
    write(&sb.repo.join(".gitignore"), ".claude/\n");
    sb.commit_all("ignore claude");
    write(&sb.repo.join(LOCAL_SETTINGS), r#"{"permissions": {}}"#);

    let (code, out) = sb.json(&["init", "--commit"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["hook"]["tracking"], json!("committed"), "{out}");
    assert_eq!(out["hook"]["commit_required"], json!(false), "{out}");
    assert_eq!(out["commit"]["committed"], json!(true), "{out}");

    let ignore = fs::read_to_string(sb.repo.join(".gitignore")).unwrap();
    assert!(ignore.contains("!.claude/settings.json"), "{ignore}");
    assert!(ignore.contains(LOCAL_SETTINGS), "{ignore}");

    let committed_files = git_stdout(
        &sb,
        &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"],
    );
    assert!(
        committed_files.contains(".claude/settings.json"),
        "{committed_files}"
    );
    assert!(
        !committed_files.contains(LOCAL_SETTINGS),
        "local settings must never be committed: {committed_files}"
    );
    let status = git_stdout(&sb, &["status", "--porcelain", "--ignored"]);
    assert!(
        status.contains("!! .claude/settings.local.json"),
        "{status}"
    );
}

#[test]
fn init_commit_works_cleanly_on_unborn_head_repo() {
    let sb = Sandbox::without_git();
    sb.git(&["init", "-q", "-b", "main"]);
    configure_git_identity(&sb);

    let (code, out) = sb.json(&["init", "--commit"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["hook"]["tracking"], json!("committed"), "{out}");
    assert_eq!(out["hook"]["commit_required"], json!(false), "{out}");
    assert_eq!(out["commit"]["committed"], json!(true), "{out}");
    assert!(out["commit"]["sha"].as_str().is_some_and(|s| s.len() == 40));

    let count = git_stdout(&sb, &["rev-list", "--count", "HEAD"]);
    assert_eq!(count.trim(), "1");
    let subject = git_stdout(&sb, &["log", "-1", "--pretty=%s"]);
    assert_eq!(
        subject.trim(),
        "chore(aaf): initialize AutoAgentFactory config and hooks"
    );
}

#[test]
fn init_detects_gradle_no_daemon_and_ignores_commented_product_flavors() {
    let sb = Sandbox::new();
    write(&sb.repo.join("build.gradle.kts"), "");
    write(&sb.repo.join("gradlew"), "#!/bin/sh\n");
    write(
        &sb.repo.join("app/src/main/AndroidManifest.xml"),
        "<manifest/>",
    );
    write(
        &sb.repo.join("app/build.gradle.kts"),
        "// productFlavors { create(\"free\") {} }\n\
         /* nested /* productFlavors { create(\"paid\") {} } */ */\n\
         android { buildTypes { release {} } }\n",
    );

    let (code, out) = sb.json(&["init"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["profile"], json!("android"), "{out}");
    assert_eq!(
        out["commands"]["build"],
        json!("./gradlew assembleDebug --no-daemon")
    );
    assert_eq!(
        out["commands"]["test_scoped"],
        json!("./gradlew testDebugUnitTest --no-daemon")
    );
    assert_eq!(
        out["commands"]["test_full"],
        json!("./gradlew testDebugUnitTest --no-daemon")
    );
    assert_eq!(out["commands"]["lint"], json!(""));
    assert_eq!(
        out["commands"]["lint_full"],
        json!("./gradlew lint --no-daemon")
    );
    assert_eq!(out["gate_unverified_until_commands_set"], json!(false));
}

#[test]
fn init_detects_package_json_scripts_and_package_managers() {
    let full_pkg = r#"{"scripts":{"build":"vite build","test":"vitest run","lint":"eslint ."}}"#;

    for (lock, expected_build, expected_test, expected_lint) in [
        ("pnpm-lock.yaml", "pnpm build", "pnpm test", "pnpm lint"),
        ("yarn.lock", "yarn build", "yarn test", "yarn lint"),
        ("bun.lock", "bun run build", "bun run test", "bun run lint"),
        (
            "package-lock.json",
            "npm run build",
            "npm run test",
            "npm run lint",
        ),
    ] {
        let sb = Sandbox::new();
        write(&sb.repo.join("package.json"), full_pkg);
        write(&sb.repo.join(lock), "");
        let (code, out) = sb.json(&["init"]);
        assert_eq!(code, 0, "{lock}: {out}");
        assert_eq!(out["profile"], json!("web"), "{lock}");
        assert_eq!(out["commands"]["build"], json!(expected_build), "{lock}");
        assert_eq!(
            out["commands"]["test_scoped"],
            json!(expected_test),
            "{lock}"
        );
        assert_eq!(out["commands"]["test_full"], json!(expected_test), "{lock}");
        assert_eq!(out["commands"]["lint"], json!(expected_lint), "{lock}");
    }

    // Default npm stub test script is skipped; typecheck is used when lint is absent.
    let sb = Sandbox::new();
    write(
        &sb.repo.join("package.json"),
        r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1","typecheck":"tsc --noEmit","build":"tsc"}}"#,
    );
    let (code, out) = sb.json(&["init"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commands"]["test_scoped"], json!(""));
    assert_eq!(out["commands"]["test_full"], json!(""));
    assert_eq!(out["commands"]["lint"], json!("npm run typecheck"));
    assert_eq!(out["commands"]["build"], json!("npm run build"));
}

#[test]
fn init_detects_pyproject_pytest_ruff_and_swift_package() {
    let sb = Sandbox::new();
    write(
        &sb.repo.join("pyproject.toml"),
        "[project]\nname = \"demo\"\n[tool.pytest.ini_options]\naddopts = \"-q\"\n[tool.ruff]\nline-length = 100\n",
    );
    let (code, out) = sb.json(&["init"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["profile"], json!("backend"), "{out}");
    assert_eq!(out["commands"]["test_scoped"], json!("pytest"));
    assert_eq!(out["commands"]["test_full"], json!("pytest"));
    assert_eq!(out["commands"]["lint"], json!("ruff check ."));

    // pytest via optional dependency.
    let sb = Sandbox::new();
    write(
        &sb.repo.join("pyproject.toml"),
        "[project]\nname = \"demo\"\n[project.optional-dependencies]\ndev = [\"pytest>=8.0\"]\n",
    );
    let (code, out) = sb.json(&["init"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commands"]["test_scoped"], json!("pytest"));
    assert_eq!(out["commands"]["lint"], json!(""));

    // pytest via tests/ directory.
    let sb = Sandbox::new();
    write(
        &sb.repo.join("pyproject.toml"),
        "[project]\nname = \"demo\"\n",
    );
    write(
        &sb.repo.join("tests/test_smoke.py"),
        "def test_ok(): assert True\n",
    );
    let (code, out) = sb.json(&["init"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commands"]["test_scoped"], json!("pytest"));

    // Swift Package.swift.
    let sb = Sandbox::new();
    write(
        &sb.repo.join("Package.swift"),
        "// swift-tools-version: 5.9\n",
    );
    let (code, out) = sb.json(&["init"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["profile"], json!("ios"), "{out}");
    assert_eq!(out["commands"]["build"], json!("swift build"));
    assert_eq!(out["commands"]["test_scoped"], json!("swift test"));
    assert_eq!(out["commands"]["test_full"], json!("swift test"));
}
