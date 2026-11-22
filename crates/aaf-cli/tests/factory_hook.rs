#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `aaf run` with the Claude executor never starts an agent where the
//! `aaf-hook` registration is missing, neutered or cannot run (Wave 3:
//! `.claude/settings.json` was untracked on the base, so the task worktree
//! had none and the agent ran unguarded). A registration that is wrong on
//! the base refuses the whole run before any task is taken; one a task
//! changed blocks that task without counting towards the factory freeze.
//! Real binary, real git worktrees, real sqlite; `claude` and `aaf-hook`
//! are shell scripts on PATH, no real agent is ever started.

#[path = "factory_support.rs"]
mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::json;
use support::{Fixture, Out, git};

const REGISTERED: &str = r#"{"hooks":{"PreToolUse":[{"matcher":"Bash|Write|Edit|MultiEdit|Read","hooks":[{"type":"command","command":"aaf-hook"}]}]}}"#;
const NARROW: &str = r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"aaf-hook"}]}]}}"#;
/// Registered for every tool, but the shell throws the verdict away.
const NEUTERED: &str = r#"{"hooks":{"PreToolUse":[{"matcher":"Bash|Write|Edit|MultiEdit|Read","hooks":[{"type":"command","command":"aaf-hook >/dev/null 2>&1 || true"}]}]}}"#;
const DISABLED: &str = r#"{"disableAllHooks":true,"hooks":{"PreToolUse":[{"matcher":"Bash|Write|Edit|MultiEdit|Read","hooks":[{"type":"command","command":"aaf-hook"}]}]}}"#;

/// One failed root freezes the factory: a block that counts shows at once.
const CONFIG: &str = "[commands]\ntest_scoped = \"test -f done.txt\"\n\n[run]\nrun_timeout_secs = 60\n\n[unblocker]\nglobal_freeze_consecutive_fails = 1\n";

fn executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

struct Claude {
    fx: Fixture,
    /// Written by the stand-in `claude` every time it is started.
    started: PathBuf,
    /// PATH with the stand-in `claude` and the stand-in `aaf-hook`.
    path: String,
    /// The same PATH without the directory that holds `aaf-hook`.
    path_without_hook: String,
}

impl Claude {
    /// A repository whose committed config uses the default executor
    /// (`claude`), and a stand-in `claude` that makes the gate pass.
    fn new() -> Claude {
        Claude::with_agent("echo made > done.txt\n")
    }

    /// `agent` is what the stand-in `claude` does in its worktree.
    fn with_agent(agent: &str) -> Claude {
        let fx = Fixture::new();
        let dir = fx.repo.join(".autofactory");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.toml"), CONFIG).unwrap();
        git(&fx.repo, &["add", ".autofactory/config.toml"]);
        git(&fx.repo, &["commit", "--quiet", "-m", "configure aaf"]);

        let bin = fx.root().join("bin");
        let hook_bin = fx.root().join("hook-bin");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&hook_bin).unwrap();
        let started = fx.root().join("claude-started");
        executable(
            &bin.join("claude"),
            &format!(
                "#!/bin/sh\necho started >> '{}'\n{agent}",
                started.display()
            ),
        );
        executable(&hook_bin.join("aaf-hook"), "#!/bin/sh\nexit 0\n");
        // Only the system directories behind the stand-ins: the developer's
        // own PATH may hold a real `claude` or `aaf-hook`.
        let path_without_hook = format!("{}:/usr/bin:/bin", bin.display());
        let path = format!("{}:{}:/usr/bin:/bin", bin.display(), hook_bin.display());
        Claude {
            fx,
            started,
            path,
            path_without_hook,
        }
    }

    fn run(&self) -> Out {
        self.run_with_path(&self.path)
    }

    fn run_with_path(&self, path: &str) -> Out {
        self.fx
            .aaf_in(&self.fx.repo, &["run", "--unsandboxed"], &[("PATH", path)])
    }

    fn write_settings(&self, text: &str) {
        let path = self.fx.repo.join(".claude/settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn commit_settings(&self) {
        git(&self.fx.repo, &["add", ".claude/settings.json"]);
        git(&self.fx.repo, &["commit", "--quiet", "-m", "register hook"]);
    }

    fn starts(&self) -> usize {
        fs::read_to_string(&self.started).map_or(0, |text| text.lines().count())
    }

    /// The run was refused as a whole: exit 2, no agent, the task untouched
    /// and the factory not frozen. Returns the message.
    fn assert_refused(&self, out: &Out, id: &str) -> String {
        assert_eq!(out.code, 2, "{}", out.stderr);
        assert_eq!(out.json["status"], json!("refused"), "{}", out.stderr);
        assert_eq!(self.starts(), 0, "the agent was started unguarded");
        let shown = self.fx.show(id);
        assert_eq!(shown["task"]["state"], json!("ready"), "{}", out.stderr);
        assert_eq!(shown["runs"], json!([]));
        assert_ne!(out.json["frozen"], json!(true), "{}", out.stderr);
        out.json["reason"].as_str().unwrap().to_string()
    }
}

// Changed with A3: this test used to assert that the task ends Blocked. A
// registration missing on the base is one configuration problem of the
// repository, not a failure of each task: it blocked every Ready task in
// turn and froze the factory for every repository after three.
#[test]
fn an_untracked_registration_refuses_the_run_before_any_task_is_taken() {
    let world = Claude::new();
    // Registered in the main checkout, as `aaf init` leaves it, but never
    // committed: a task worktree is cut from the commit and has no file.
    world.write_settings(REGISTERED);
    let id = world.fx.add("make the file");

    let out = world.run();

    let why = world.assert_refused(&out, &id);
    assert!(why.contains("without aaf-hook"), "{why}");
    assert!(
        why.contains("commit .claude/settings.json on the base branch"),
        "{why}"
    );
    assert!(!why.contains("  "), "a lost line continuation: {why:?}");

    // The fix the message names: commit the file. Nothing to unblock.
    world.commit_settings();
    let out = world.run();
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["review"], json!([id]), "{}", out.stderr);
    assert_eq!(world.starts(), 1);
}

// Changed with A3, for the same reason as the test above.
#[test]
fn a_committed_registration_that_misses_a_tool_refuses_the_run() {
    let world = Claude::new();
    world.write_settings(NARROW);
    world.commit_settings();
    let id = world.fx.add("make the file");

    let why = world.assert_refused(&world.run(), &id);

    assert!(why.contains("does not cover"), "{why}");
}

#[test]
fn a_registration_whose_command_discards_the_verdict_refuses_the_run() {
    let world = Claude::new();
    world.write_settings(NEUTERED);
    world.commit_settings();
    let id = world.fx.add("make the file");

    let why = world.assert_refused(&world.run(), &id);

    assert!(why.contains("only the program"), "{why}");
}

#[test]
fn disable_all_hooks_refuses_the_run() {
    let world = Claude::new();
    world.write_settings(DISABLED);
    world.commit_settings();
    let id = world.fx.add("make the file");

    let why = world.assert_refused(&world.run(), &id);

    assert!(why.contains("disableAllHooks"), "{why}");
}

#[test]
fn a_hook_that_is_not_on_the_path_the_agent_gets_refuses_the_run() {
    let world = Claude::new();
    world.write_settings(REGISTERED);
    world.commit_settings();
    let id = world.fx.add("make the file");

    // As from cron or launchd: the directory of `aaf-hook` is not on PATH,
    // the hook would end with exit 127 and Claude Code would go on.
    let out = world.run_with_path(&world.path_without_hook);

    let why = world.assert_refused(&out, &id);
    assert!(why.contains("not found on the PATH"), "{why}");

    // With the directory on PATH the same repository runs.
    let out = world.run();
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["review"], json!([id]), "{}", out.stderr);
}

#[test]
fn a_committed_registration_lets_the_agent_run() {
    let world = Claude::new();
    world.write_settings(REGISTERED);
    world.commit_settings();
    let id = world.fx.add("make the file");

    let out = world.run();

    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["review"], json!([id]), "{}", out.stderr);
    assert!(world.started.exists());
}

/// An agent run that did `tamper` in its worktree and ended with `exit`:
/// no second agent (retry, subtask proposer, auditor) starts there, nothing
/// of it reaches the gate, and the task waits for a person.
fn tampered_worktree_blocks(tamper: &str, exit: u8, expected: &str) {
    let world = Claude::with_agent(&format!("{tamper}\necho made > done.txt\nexit {exit}\n"));
    world.write_settings(REGISTERED);
    world.commit_settings();
    let id = world.fx.add("make the file");

    let out = world.run();

    assert_eq!(world.starts(), 1, "{}", out.stderr);
    assert_eq!(out.json["blocked"], json!([id]), "{}", out.stderr);
    assert_eq!(out.json["gate_runs"], json!(0), "{}", out.stderr);
    assert_eq!(out.code, 2, "{}", out.stderr);
    let shown = world.fx.show(&id);
    let why = shown["task"]["blocked_reason"]["why"].as_str().unwrap();
    assert!(why.contains(expected), "{why}");
    assert!(why.contains("by a person"), "{why}");
    // One task's changed guard is not a failed root: with a freeze
    // threshold of 1 a counted block would have frozen the factory.
    assert_eq!(out.json["frozen"], json!(false), "{}", out.stderr);
    let again = world.run();
    assert_ne!(again.json["status"], json!("frozen"), "{}", again.stderr);

    // Sent back without the change being undone, it is stopped again
    // before an agent starts.
    assert_eq!(world.fx.aaf(&["board", "unblock", &id]).code, 0);
    let out = world.run();
    assert_eq!(world.starts(), 1, "an agent started on a changed hook");
    assert_eq!(out.json["blocked"], json!([id]), "{}", out.stderr);
    assert_eq!(out.json["frozen"], json!(false), "{}", out.stderr);
}

#[test]
fn a_failed_run_that_rewrote_the_registration_is_blocked_without_counting_as_a_failed_root() {
    let neutered = NEUTERED.replace('\'', "'\\''");
    tampered_worktree_blocks(
        &format!("printf '%s' '{neutered}' > .claude/settings.json"),
        1,
        "only the program",
    );
}

#[test]
fn a_successful_run_that_rewrote_the_registration_never_reaches_the_gate() {
    let neutered = NEUTERED.replace('\'', "'\\''");
    tampered_worktree_blocks(
        &format!("printf '%s' '{neutered}' > .claude/settings.json"),
        0,
        "only the program",
    );
}

#[test]
fn a_run_that_changed_the_settings_file_without_touching_the_hook_is_blocked() {
    // Still a valid registration, but not the base's file any more.
    let changed = REGISTERED.replacen('{', r#"{"model":"other","#, 1);
    tampered_worktree_blocks(
        &format!("printf '%s' '{changed}' > .claude/settings.json"),
        0,
        "not what the base branch has",
    );
}

#[test]
fn a_run_that_added_a_local_settings_file_with_hooks_is_blocked() {
    for exit in [0, 1] {
        tampered_worktree_blocks(
            r#"printf '%s' '{"disableAllHooks":true}' > .claude/settings.local.json"#,
            exit,
            "settings.local.json",
        );
    }
}
