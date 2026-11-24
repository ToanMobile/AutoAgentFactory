#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Unit/integration tests for W5e writer-stage hook predicates and base-branch
//! multi-host hook verification (`writer_requires_hook`, `writer_hook_executors`,
//! and `check_base_for_executor`).

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use aaf_domain::config::RepoConfig;
use aaf_domain::pipeline::StageModelConfig;
use aaf_engine::factory::stage_router::{writer_hook_executors, writer_requires_hook};
use aaf_engine::hook_settings::{self, multi_host};
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_temp_repo() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let repo = root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "--quiet", "-b", "main"]);
    for (k, v) in [
        ("user.name", "aaf test"),
        ("user.email", "aaf@example.invalid"),
        ("commit.gpgsign", "false"),
        ("core.hooksPath", "/dev/null"),
    ] {
        git(&repo, &["config", k, v]);
    }
    fs::write(repo.join("README.md"), "test\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "--quiet", "-m", "init"]);

    let hook_bin = root.join("hook-bin");
    fs::create_dir_all(&hook_bin).unwrap();
    let hook_path = hook_bin.join("aaf-hook");
    fs::write(&hook_path, "#!/bin/sh\nexit 0\n").unwrap();
    let mut perms = fs::metadata(&hook_path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&hook_path, perms).unwrap();
    (tmp, repo, hook_bin)
}

#[test]
fn writer_requires_hook_table() {
    let cases = [
        ("claude", "", true),
        ("CLAUDE", "", true),
        ("claude", "custom-wrapper {prompt}", true),
        ("custom", "/usr/local/bin/claude -p {prompt}", true),
        ("codex", "", true),
        ("  Codex  ", "   ", true),
        ("gemini", "", true),
        ("antigravity", "", true),
        ("opencode", "", true),
        ("codex", "my-codex-wrapper {prompt}", false),
        ("gemini", "my-gemini-wrapper {prompt}", false),
        ("antigravity", "/opt/bin/agy {prompt}", false),
        ("opencode", "oc-wrap {prompt}", false),
        ("cursor", "", false),
        ("custom", "my-tool {prompt}", false),
        ("", "", false),
    ];
    for (exec, cmd, expected) in cases {
        assert_eq!(
            writer_requires_hook(exec, cmd),
            expected,
            "writer_requires_hook({exec:?}, {cmd:?})"
        );
    }
}

#[test]
fn writer_hook_executors_covers_default_implementer_and_escalated() {
    let mut cfg = RepoConfig::default();
    assert!(writer_hook_executors(&cfg).is_empty());

    // Default run.executor = codex with empty executor_cmd -> requires codex.
    cfg.run.executor = "Codex".into();
    assert_eq!(
        writer_hook_executors(&cfg),
        BTreeSet::from(["codex".to_string()])
    );

    // Non-empty run.executor_cmd -> default implementer excluded.
    cfg.run.executor_cmd = vec!["my-wrapper".into(), "{prompt}".into()];
    assert!(writer_hook_executors(&cfg).is_empty());

    // pipeline.implementer with ("", "") inherits run.executor (with empty stage cmd).
    cfg.pipeline.implementer = StageModelConfig {
        model: "gpt-5".into(),
        ..StageModelConfig::default()
    };
    assert_eq!(
        writer_hook_executors(&cfg),
        BTreeSet::from(["codex".to_string()])
    );

    // pipeline.implementer with ("", cmd) -> effective "custom" -> excluded.
    cfg.pipeline.implementer = StageModelConfig {
        executor_cmd: "my-custom {prompt}".into(),
        ..StageModelConfig::default()
    };
    assert!(writer_hook_executors(&cfg).is_empty());

    // pipeline.implementer_escalated adds another hooked host, while read-only
    // stages (plan_author, plan_challenger, cross_audit) are ignored.
    cfg.pipeline.implementer = StageModelConfig {
        executor: "Gemini".into(),
        ..StageModelConfig::default()
    };
    cfg.pipeline.implementer_escalated = StageModelConfig {
        executor: "antigravity".into(),
        ..StageModelConfig::default()
    };
    cfg.pipeline.plan_author = StageModelConfig {
        executor: "opencode".into(),
        ..StageModelConfig::default()
    };
    cfg.cross_audit.executor = "opencode".into();
    assert_eq!(
        writer_hook_executors(&cfg),
        BTreeSet::from(["antigravity".to_string(), "gemini".to_string()])
    );

    // Explicit executor_cmd on escalated excludes it.
    cfg.pipeline.implementer_escalated.executor_cmd = "wrap-agy {prompt}".into();
    assert_eq!(
        writer_hook_executors(&cfg),
        BTreeSet::from(["gemini".to_string()])
    );
}

#[test]
fn check_base_for_executor_codex_missing_committed_and_path_error() {
    let (_tmp, repo, hook_bin) = init_temp_repo();
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let ok_path = format!("{}:/usr/bin:/bin", hook_bin.display());

    // Missing on base -> Err naming .codex/hooks.json and fix command.
    let err = hook_settings::check_base_for_executor(&repo, &base, OsStr::new(&ok_path), "codex")
        .unwrap_err();
    assert!(err.contains(".codex/hooks.json"), "{err}");
    assert!(err.contains("aaf init --hook-host codex"), "{err}");
    assert!(err.contains("no task was touched"), "{err}");
    assert!(!err.contains("  "), "double space in message: {err:?}");

    // Register and commit .codex/hooks.json -> passes with hook on PATH.
    multi_host::register_codex(&repo, "aaf-hook").unwrap();
    git(&repo, &["add", ".codex/hooks.json"]);
    git(&repo, &["commit", "--quiet", "-m", "add codex hook"]);
    let base2 = git(&repo, &["rev-parse", "HEAD"]);

    hook_settings::check_base_for_executor(&repo, &base2, OsStr::new(&ok_path), "codex").unwrap();

    // Missing aaf-hook on PATH -> Err with PATH-specific fix.
    let err_path =
        hook_settings::check_base_for_executor(&repo, &base2, OsStr::new("/usr/bin:/bin"), "codex")
            .unwrap_err();
    assert!(
        err_path.contains("fix: start `aaf run` with aaf-hook on PATH"),
        "{err_path}"
    );
    assert!(err_path.contains(".codex/hooks.json"), "{err_path}");

    // Unknown/custom executor -> Ok(()) even without hook files.
    hook_settings::check_base_for_executor(&repo, &base, OsStr::new(&ok_path), "custom").unwrap();
    hook_settings::check_base_for_executor(&repo, &base, OsStr::new(&ok_path), "cursor").unwrap();
}

#[test]
fn check_base_for_executor_gemini_antigravity_fallback_and_claude_delegation() {
    let (_tmp, repo, hook_bin) = init_temp_repo();
    let ok_path = format!("{}:/usr/bin:/bin", hook_bin.display());

    // Commit antigravity registration in .gemini/settings.json -> both
    // `antigravity` and `gemini` pass via mutual fallback.
    multi_host::register_antigravity(&repo, "aaf-hook").unwrap();
    git(&repo, &["add", ".gemini/settings.json"]);
    git(&repo, &["commit", "--quiet", "-m", "add antigravity hook"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);

    hook_settings::check_base_for_executor(&repo, &base, OsStr::new(&ok_path), "antigravity")
        .unwrap();
    hook_settings::check_base_for_executor(&repo, &base, OsStr::new(&ok_path), "gemini").unwrap();

    // Claude delegation: missing .claude/settings.json fails for both "claude" and "".
    let claude_err =
        hook_settings::check_base_for_executor(&repo, &base, OsStr::new(&ok_path), "claude")
            .unwrap_err();
    assert!(claude_err.contains(".claude/settings.json"), "{claude_err}");
    let empty_err =
        hook_settings::check_base_for_executor(&repo, &base, OsStr::new(&ok_path), "").unwrap_err();
    assert!(empty_err.contains(".claude/settings.json"), "{empty_err}");
}
