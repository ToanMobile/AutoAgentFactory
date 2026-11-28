#![allow(clippy::unwrap_used, clippy::expect_used)]
//! SEC-121: `aaf plan`, `aaf sop run`, and `aaf board split` must enforce the
//! matching host `aaf-hook` check for non-Claude executors (e.g. `codex` ->
//! `.codex/hooks.json`), refusing before spawning the executor when the hook
//! registration is missing, while allowing execution when `.codex/hooks.json`
//! is validly registered and committed.

#[path = "factory_support.rs"]
mod support;

#[path = "../../aaf-fp/tests/exec_support.rs"]
mod exec_support;

use std::fs;
use std::path::PathBuf;

use serde_json::json;
use support::{Fixture, Out, git};
use tempfile::TempDir;

const CODEX_CONFIG: &str = "\
[commands]
test_scoped = \"true\"

[run]
executor = \"codex\"
run_timeout_secs = 60
";

const SOP_TOML: &str = r#"
name = "agent-check"
version = 1
description = "Single agent stage for SEC-121 hook check"
profile = "auto"
max_reworks = 0

[[stages]]
id = "agent_step"
title = "Run codex agent"
kind = { type = "agent", role = "implementer", prompt = "Complete the stage." }
"#;

struct CodexWorld {
    fx: Fixture,
    /// Separate temp dir passed as `TMPDIR` so the sandboxed `codex` stub can
    /// write its marker file without colliding with repo, data, or PATH.
    _marker_dir: TempDir,
    marker_root: PathBuf,
    marker: PathBuf,
    path: String,
}

impl CodexWorld {
    fn new(with_codex_hook: bool) -> Self {
        let fx = Fixture::new();
        let marker_dir = tempfile::tempdir().unwrap();
        let marker_root = fs::canonicalize(marker_dir.path()).unwrap();
        let marker = marker_root.join("codex-ran");

        let bin = fx.root().join("bin");
        let hook_bin = fx.root().join("hook-bin");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&hook_bin).unwrap();

        let stub = format!(
            "#!/bin/sh\necho ran > '{}'\nprintf '[]\\n'\nexit 0\n",
            marker.display()
        );
        exec_support::write_executable(&bin.join("codex"), &stub);
        exec_support::write_executable(&hook_bin.join("aaf-hook"), "#!/bin/sh\nexit 0\n");

        let af_dir = fx.repo.join(".autofactory");
        let sops_dir = af_dir.join("sops");
        fs::create_dir_all(&sops_dir).unwrap();
        fs::write(af_dir.join("config.toml"), CODEX_CONFIG).unwrap();
        fs::write(sops_dir.join("agent-check.toml"), SOP_TOML).unwrap();
        git(
            &fx.repo,
            &[
                "add",
                ".autofactory/config.toml",
                ".autofactory/sops/agent-check.toml",
            ],
        );
        git(&fx.repo, &["commit", "--quiet", "-m", "configure codex"]);

        let path = format!("{}:{}:/usr/bin:/bin", bin.display(), hook_bin.display());
        if with_codex_hook {
            let init = fx.aaf_in(
                &fx.repo,
                &["init", "--hook-host", "codex"],
                &[("PATH", &path)],
            );
            assert_eq!(init.code, 0, "aaf init --hook-host codex: {}", init.stderr);
            assert!(fx.repo.join(".codex/hooks.json").exists());
            git(&fx.repo, &["add", ".codex/hooks.json"]);
            git(
                &fx.repo,
                &["commit", "--quiet", "-m", "register codex hook"],
            );
        } else {
            assert!(!fx.repo.join(".codex/hooks.json").exists());
        }

        Self {
            fx,
            _marker_dir: marker_dir,
            marker_root,
            marker,
            path,
        }
    }

    fn run_aaf(&self, args: &[&str]) -> Out {
        let tmpdir = self.marker_root.to_string_lossy().into_owned();
        self.fx.aaf_in(
            &self.fx.repo,
            args,
            &[("PATH", &self.path), ("TMPDIR", &tmpdir)],
        )
    }

    fn run_plan(&self) -> Out {
        self.run_aaf(&["plan", "Draft a retry plan"])
    }

    fn run_sop(&self) -> Out {
        self.run_aaf(&["sop", "run", "agent-check"])
    }

    fn run_board_split(&self, task_id: &str) -> Out {
        self.run_aaf(&["board", "split", task_id, "--dry-run", "--unsandboxed"])
    }
}

fn assert_refused_for_codex_hook(cmd: &str, world: &CodexWorld, out: &Out) {
    assert!(
        !world.marker.exists(),
        "SEC-121: `{cmd}` executed codex without .codex/hooks.json hook: {}",
        out.stderr
    );
    assert_eq!(
        out.code, 2,
        "expected policy refusal exit 2: {}",
        out.stderr
    );
    assert_eq!(out.json["status"], json!("refused"), "{}", out.stderr);
    let reason = out.json["reason"].as_str().unwrap_or("");
    assert!(
        reason.contains("without aaf-hook") && reason.contains(".codex/hooks.json"),
        "expected codex hook refusal reason, got {reason:?}\n{}",
        out.stderr
    );
}

fn assert_ran_without_hook_refusal(cmd: &str, world: &CodexWorld, out: &Out) {
    let reason = out.json["reason"].as_str().unwrap_or("");
    let summary = out.json["checkpoint"]["stage_states"]["agent_step"]["summary"]
        .as_str()
        .unwrap_or("");
    for text in [reason, summary] {
        assert!(
            !text.contains("without aaf-hook") && !text.contains(".codex/hooks.json"),
            "unexpected hook refusal for `{cmd}` ({text:?}): {}",
            out.stderr
        );
    }
    assert!(
        world.marker.exists(),
        "codex stub did not run for `{cmd}` with valid hook: {}",
        out.stderr
    );
}

#[test]
fn plan_refuses_codex_without_hook() {
    let world = CodexWorld::new(false);
    let out = world.run_plan();
    assert_refused_for_codex_hook("aaf plan", &world, &out);
}

#[test]
fn sop_run_refuses_codex_stage_without_hook() {
    let world = CodexWorld::new(false);
    let out = world.run_sop();

    assert!(
        !world.marker.exists(),
        "SEC-121: `aaf sop run` executed codex stage without .codex/hooks.json hook: {}",
        out.stderr
    );
    assert_eq!(
        out.code, 2,
        "expected policy failure exit 2: {}",
        out.stderr
    );
    let stage = &out.json["checkpoint"]["stage_states"]["agent_step"];
    assert!(
        out.json["status"] == json!("refused")
            || (out.json["run_status"] == json!("failed") && stage["status"] == json!("failed")),
        "expected policy refusal or failed stage: {}",
        out.stderr
    );
    let msg = stage["summary"]
        .as_str()
        .or_else(|| out.json["reason"].as_str())
        .unwrap_or("");
    assert!(
        msg.contains("without aaf-hook") && msg.contains(".codex/hooks.json"),
        "expected codex hook failure summary, got {msg:?}\n{}",
        out.stderr
    );
}

#[test]
fn board_split_refuses_codex_without_hook() {
    let world = CodexWorld::new(false);
    let id = world.fx.add("split with codex");
    let out = world.run_board_split(&id);
    assert_refused_for_codex_hook("aaf board split", &world, &out);
}

#[test]
fn plan_allows_codex_with_valid_hook() {
    let world = CodexWorld::new(true);
    let out = world.run_plan();
    assert_ran_without_hook_refusal("aaf plan", &world, &out);
}

#[test]
fn sop_run_allows_codex_stage_with_valid_hook() {
    let world = CodexWorld::new(true);
    let out = world.run_sop();
    assert_ran_without_hook_refusal("aaf sop run", &world, &out);
}

#[test]
fn board_split_allows_codex_with_valid_hook() {
    let world = CodexWorld::new(true);
    let id = world.fx.add("split with codex and hook");
    let out = world.run_board_split(&id);
    assert_ran_without_hook_refusal("aaf board split", &world, &out);
}
