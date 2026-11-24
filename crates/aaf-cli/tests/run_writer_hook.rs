#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression oracle for W5e: non-Claude code-writing stages (`codex`,
//! `gemini`, `antigravity`, `opencode`) MUST require their host `aaf-hook`
//! registration on the base branch before any agent starts, while read-only
//! stages (auditors) remain Claude-only.

#[path = "factory_support.rs"]
mod support;

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::json;
use support::{Fixture, Out, git};
use tempfile::TempDir;

#[path = "../../aaf-fp/tests/exec_support.rs"]
mod exec_support;

const CLAUDE_REGISTERED: &str = r#"{"hooks":{"PreToolUse":[{"matcher":"Bash|Write|Edit|MultiEdit|Read","hooks":[{"type":"command","command":"aaf-hook"}]}]}}"#;

const CODEX_CONFIG: &str = "\
[commands]
test_scoped = \"test -f done.txt\"

[run]
executor = \"codex\"
run_timeout_secs = 60

[unblocker]
global_freeze_consecutive_fails = 1
";

const CLAUDE_WITH_GEMINI_AUDITOR_CONFIG: &str = "\
[commands]
test_scoped = \"test -f done.txt\"

[run]
executor = \"claude\"
run_timeout_secs = 60

[cross_audit]
executor = \"gemini\"

[unblocker]
global_freeze_consecutive_fails = 1
";

fn write_exec(path: &Path, body: &str) {
    exec_support::write_executable(path, body);
}

struct World {
    fx: Fixture,
    /// Separate temp dir outside both repo and data dir so marker writes
    /// cannot be confused with repo state.
    _marker_dir: TempDir,
    marker: PathBuf,
    path: String,
}

impl World {
    fn new(config_toml: &str, commit_claude_hook: bool) -> Self {
        let fx = Fixture::new();
        let dir = fx.repo.join(".autofactory");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.toml"), config_toml).unwrap();
        git(&fx.repo, &["add", ".autofactory/config.toml"]);
        if commit_claude_hook {
            let claude_path = fx.repo.join(".claude/settings.json");
            fs::create_dir_all(claude_path.parent().unwrap()).unwrap();
            fs::write(&claude_path, CLAUDE_REGISTERED).unwrap();
            git(&fx.repo, &["add", ".claude/settings.json"]);
        }
        git(&fx.repo, &["commit", "--quiet", "-m", "configure aaf"]);

        let marker_dir = tempfile::tempdir().unwrap();
        let marker = fs::canonicalize(marker_dir.path())
            .unwrap()
            .join("agent-started");

        let bin = fx.root().join("bin");
        let hook_bin = fx.root().join("hook-bin");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&hook_bin).unwrap();

        let stub = format!(
            "#!/bin/sh\necho started >> '{}'\necho made > done.txt\nexit 0\n",
            marker.display()
        );
        write_exec(&bin.join("codex"), &stub);
        write_exec(&bin.join("claude"), &stub);
        write_exec(&bin.join("gemini"), "#!/bin/sh\nexit 0\n");
        write_exec(&hook_bin.join("aaf-hook"), "#!/bin/sh\nexit 0\n");

        let path = format!("{}:{}:/usr/bin:/bin", bin.display(), hook_bin.display());
        Self {
            fx,
            _marker_dir: marker_dir,
            marker,
            path,
        }
    }

    fn init_and_commit_codex_hook(&self) {
        let out = self.fx.aaf_in(
            &self.fx.repo,
            &["init", "--hook-host", "codex"],
            &[("PATH", &self.path)],
        );
        assert_eq!(out.code, 0, "aaf init --hook-host codex: {}", out.stderr);
        assert!(
            self.fx.repo.join(".codex/hooks.json").exists(),
            "aaf init --hook-host codex did not create .codex/hooks.json"
        );
        // Commit ONLY .codex/hooks.json (not git add -A, because `aaf init` may
        // also touch .claude/settings.json).
        git(&self.fx.repo, &["add", ".codex/hooks.json"]);
        git(
            &self.fx.repo,
            &["commit", "--quiet", "-m", "register codex hook"],
        );
    }

    fn run(&self) -> Out {
        self.fx.aaf_in(
            &self.fx.repo,
            &["run", "--unsandboxed"],
            &[("PATH", &self.path)],
        )
    }

    fn assert_not_refused_for_hook(&self, out: &Out) {
        let reason = out.json["reason"].as_str().unwrap_or("");
        assert_ne!(
            out.json["refused_kind"],
            json!("config"),
            "unexpected config refusal (reason={reason:?}): {}",
            out.stderr
        );
        assert!(
            !reason.contains("without aaf-hook")
                && !reason.contains(".codex/hooks.json")
                && !reason.contains(".claude/settings.json")
                && !reason.contains(".gemini/settings.json"),
            "unexpected hook refusal reason: {reason:?}\n{}",
            out.stderr
        );
    }
}

#[test]
fn codex_implementer_without_codex_hook_is_refused_before_any_agent_starts() {
    let world = World::new(CODEX_CONFIG, true);
    let id = world.fx.add("implement with codex");

    let out = world.run();

    assert_eq!(out.code, 2, "{}", out.stderr);
    assert_eq!(out.json["status"], json!("refused"), "{}", out.stderr);
    assert_eq!(out.json["refused_kind"], json!("config"), "{}", out.stderr);
    assert_eq!(out.json["frozen"], json!(false), "{}", out.stderr);
    let reason = out.json["reason"].as_str().unwrap_or("");
    assert!(
        reason.contains(".codex/hooks.json"),
        "reason must name .codex/hooks.json: {reason:?}"
    );
    assert!(
        reason.contains("aaf init --hook-host codex"),
        "reason must name `aaf init --hook-host codex`: {reason:?}"
    );
    assert!(
        !world.marker.exists(),
        "codex stub started unguarded before hook refusal"
    );
    let shown = world.fx.show(&id);
    assert_eq!(shown["task"]["state"], json!("ready"), "{}", out.stderr);
    assert_eq!(shown["runs"], json!([]));
}

#[test]
fn codex_implementer_with_committed_codex_hook_passes_the_hook_precheck() {
    let world = World::new(CODEX_CONFIG, true);
    world.init_and_commit_codex_hook();
    let _id = world.fx.add("implement with codex and hook");

    let out = world.run();

    world.assert_not_refused_for_hook(&out);
    assert!(
        world.marker.exists(),
        "codex stub did not start after hook precheck passed: {}",
        out.stderr
    );
}

#[test]
fn codex_only_repo_with_only_codex_hook_is_not_refused_for_claude_settings() {
    let world = World::new(CODEX_CONFIG, false);
    world.init_and_commit_codex_hook();
    // Confirm .claude/settings.json is NOT committed on HEAD.
    let claude_committed = git(
        &world.fx.repo,
        &[
            "ls-tree",
            "--name-only",
            "HEAD",
            "--",
            ".claude/settings.json",
        ],
    );
    assert!(
        claude_committed.is_empty(),
        "fixture must not commit .claude/settings.json"
    );
    let _id = world.fx.add("implement with codex only");

    let out = world.run();

    world.assert_not_refused_for_hook(&out);
    assert!(
        world.marker.exists(),
        "codex-only repo did not start codex agent: {}",
        out.stderr
    );
}

#[test]
fn read_only_auditor_stays_claude_only() {
    let world = World::new(CLAUDE_WITH_GEMINI_AUDITOR_CONFIG, true);
    let _id = world.fx.add("implement with claude, audit with gemini");

    let out = world.run();

    world.assert_not_refused_for_hook(&out);
    assert!(
        world.marker.exists(),
        "claude implementer did not start when gemini read-only auditor has no hook: {}",
        out.stderr
    );
}
