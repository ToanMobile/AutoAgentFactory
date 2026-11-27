#![allow(clippy::unwrap_used, clippy::expect_used)]
//! SEC-138: `aaf sop run` agent stages must enforce Claude's
//! `.claude/settings.json` `aaf-hook` check when `[run] executor = "custom"`
//! invokes `claude` via `executor_cmd`, matching worker behavior (SEC-099),
//! while non-Claude custom commands remain allowed without Claude hooks.

#[path = "factory_support.rs"]
mod support;

#[path = "../../aaf-fp/tests/exec_support.rs"]
mod exec_support;

use std::fs;
use std::path::PathBuf;

use serde_json::json;
use support::{Fixture, Out, git};
use tempfile::TempDir;

const SOP_TOML: &str = r#"
name = "agent-check"
version = 1
description = "Single agent stage for SEC-138 hook check"
profile = "auto"
max_reworks = 0

[[stages]]
id = "agent_step"
title = "Run custom agent"
kind = { type = "agent", role = "implementer", prompt = "Complete the stage." }
"#;

struct SopWorld {
    fx: Fixture,
    /// Separate temp dir passed as `TMPDIR` so the sandboxed stub can write
    /// its marker file without colliding with repo, data, worktree, or PATH.
    _marker_dir: TempDir,
    marker_root: PathBuf,
    marker: PathBuf,
    path: String,
}

impl SopWorld {
    fn new(script_name: &str, extra_args: &[&str]) -> Self {
        let fx = Fixture::new();
        let marker_dir = tempfile::tempdir().unwrap();
        let marker_root = fs::canonicalize(marker_dir.path()).unwrap();
        let marker = marker_root.join("agent-ran");

        let bin = fx.root().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let script_path = bin.join(script_name);
        let stub = format!("#!/bin/sh\necho ran > '{}'\nexit 0\n", marker.display());
        exec_support::write_executable(&script_path, &stub);

        let mut cmd_argv = Vec::with_capacity(1 + extra_args.len());
        cmd_argv.push(script_path.to_string_lossy().into_owned());
        cmd_argv.extend(extra_args.iter().map(|arg| (*arg).to_string()));
        let cmd_json = serde_json::to_string(&cmd_argv).unwrap();

        let config_toml = format!("[run]\nexecutor = \"custom\"\nexecutor_cmd = {cmd_json}\n");
        let af_dir = fx.repo.join(".autofactory");
        let sops_dir = af_dir.join("sops");
        fs::create_dir_all(&sops_dir).unwrap();
        fs::write(af_dir.join("config.toml"), config_toml).unwrap();
        fs::write(sops_dir.join("agent-check.toml"), SOP_TOML).unwrap();
        git(
            &fx.repo,
            &[
                "add",
                ".autofactory/config.toml",
                ".autofactory/sops/agent-check.toml",
            ],
        );
        git(&fx.repo, &["commit", "--quiet", "-m", "configure sop"]);

        assert!(
            !fx.repo.join(".claude/settings.json").exists(),
            "fixture must not have .claude/settings.json"
        );

        let path = format!("{}:/usr/bin:/bin", bin.display());
        Self {
            fx,
            _marker_dir: marker_dir,
            marker_root,
            marker,
            path,
        }
    }

    fn run_sop(&self) -> Out {
        let tmpdir = self.marker_root.to_string_lossy().into_owned();
        self.fx.aaf_in(
            &self.fx.repo,
            &["sop", "run", "agent-check"],
            &[("PATH", &self.path), ("TMPDIR", &tmpdir)],
        )
    }
}

#[test]
fn sop_agent_stage_with_custom_claude_cmd_requires_hook() {
    let world = SopWorld::new("claude", &["-p", "{prompt}"]);
    let out = world.run_sop();

    assert!(
        !world.marker.exists(),
        "SEC-138: custom claude agent executed without .claude/settings.json hook: {}",
        out.stderr
    );
    assert_eq!(
        out.code, 2,
        "expected policy failure exit 2: {}",
        out.stderr
    );
    assert_eq!(out.json["run_status"], json!("failed"), "{}", out.stderr);
    let stage = &out.json["checkpoint"]["stage_states"]["agent_step"];
    assert_eq!(stage["status"], json!("failed"), "{}", out.stderr);
    let summary = stage["summary"].as_str().unwrap_or("");
    assert!(
        summary.contains("without aaf-hook") && summary.contains(".claude/settings.json"),
        "expected hook failure summary, got: {summary:?}\n{}",
        out.stderr
    );
}

#[test]
fn sop_agent_stage_with_custom_non_claude_cmd_runs() {
    let world = SopWorld::new("my-agent", &["{prompt}"]);
    let out = world.run_sop();

    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["run_status"], json!("completed"), "{}", out.stderr);
    let stage = &out.json["checkpoint"]["stage_states"]["agent_step"];
    assert_eq!(stage["status"], json!("passed"), "{}", out.stderr);
    assert!(
        world.marker.exists(),
        "custom non-claude agent did not execute: {}",
        out.stderr
    );
}
