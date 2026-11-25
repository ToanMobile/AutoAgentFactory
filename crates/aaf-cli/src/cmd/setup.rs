//! `aaf setup`: one command from a fresh checkout to a working factory.
//!
//! Detects which coding-agent CLIs are installed, registers the hook for each
//! of them and syncs the bundled skills (`aaf init`, which keeps an existing
//! config), then runs `aaf doctor` and says what, if anything, is left.
//! Safe to re-run: a second run repairs a hook that went missing and changes
//! nothing that is already right.

use std::fmt::Write as _;
use std::path::Path;

use aaf_engine::executors::find_on_path;
use serde_json::{Value, json};

use super::doctor::{DoctorArgs, run as run_doctor};
use super::init::{InitArgs, init};
use super::skills::SkillHost;
use crate::ctx::{self, Ctx};
use crate::output::{CmdOutput, CmdResult};

/// `(hook host, binaries that mean it is installed)`, in registration order.
const AGENT_HOSTS: [(&str, &[&str]); 5] = [
    ("claude", &["claude"]),
    ("codex", &["codex"]),
    ("gemini", &["gemini"]),
    ("antigravity", &["agy", "antigravity"]),
    ("opencode", &["opencode"]),
];

#[derive(Debug, clap::Args)]
pub struct SetupArgs {
    /// Hook hosts to register instead of the detected ones (comma-separated).
    #[arg(long, value_name = "HOSTS")]
    pub hook_host: Option<String>,
    /// Skip syncing the bundled skills.
    #[arg(long)]
    pub no_skills: bool,
}

/// Agent hosts whose CLI is on `path_var`; `claude` when none is.
/// The Antigravity entry replaces the Gemini one in `.gemini/settings.json`
/// and covers Gemini tools too, so `gemini` is dropped when both are found.
pub(crate) fn detect_hosts(path_var: &std::ffi::OsStr) -> Vec<&'static str> {
    let mut found: Vec<&'static str> = AGENT_HOSTS
        .iter()
        .filter(|(_, bins)| bins.iter().any(|b| find_on_path(path_var, b).is_some()))
        .map(|(host, _)| *host)
        .collect();
    if found.contains(&"antigravity") {
        found.retain(|host| *host != "gemini");
    }
    if found.is_empty() {
        vec!["claude"]
    } else {
        found
    }
}

pub fn run(args: &SetupArgs, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = super::board::store::refuse_inside_agent_run(ctx, "aaf setup") {
        return Ok(refused);
    }
    let hosts = match &args.hook_host {
        Some(list) => list.clone(),
        None => detect_hosts(&ctx.path_var).join(","),
    };
    let init_args = InitArgs {
        force: false,
        with_skills: !args.no_skills,
        host: SkillHost::All,
        hook_host: hosts.clone(),
        commit: false,
    };
    let init_out = init(&init_args, ctx, ctx::current_exe().as_deref())?;
    let doctor_out = run_doctor(&DoctorArgs {}, ctx)?;
    let checks = doctor_out
        .fields
        .get("checks")
        .cloned()
        .unwrap_or(Value::Array(Vec::new()));
    let summary = summarize(&checks);
    let ready = summary.failed_required.is_empty();
    let text = render(&hosts, &init_out.text, &summary, ctx.cwd.as_path());
    let out = if ready {
        CmdOutput::ok("ready", text)
    } else {
        CmdOutput::policy("needs_attention", text)
    };
    Ok(out
        .field("hook_hosts", json!(hosts.split(',').collect::<Vec<_>>()))
        .field("init", Value::Object(init_out.fields))
        .field("checks", checks)
        .field("failed_required", json!(summary.failed_required))
        .field("advice", json!(summary.advice)))
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Summary {
    pub passed: usize,
    /// `name: detail` of each failed required check.
    pub failed_required: Vec<String>,
    /// `name: detail` of each failed advisory check.
    pub advice: Vec<String>,
}

pub(crate) fn summarize(checks: &Value) -> Summary {
    let mut summary = Summary::default();
    for check in checks.as_array().into_iter().flatten() {
        if check["applicable"] == json!(false) {
            continue;
        }
        let line = format!(
            "{}: {}",
            check["name"].as_str().unwrap_or("?"),
            check["detail"].as_str().unwrap_or("")
        );
        match (check["ok"].as_bool(), check["required"].as_bool()) {
            (Some(true), _) => summary.passed += 1,
            (_, Some(true)) => summary.failed_required.push(line),
            _ => summary.advice.push(line),
        }
    }
    summary
}

fn render(hosts: &str, init_text: &str, summary: &Summary, cwd: &Path) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Setting up AutoAgentFactory in {}", cwd.display());
    let _ = writeln!(out, "Agent hosts: {hosts}\n");
    let _ = writeln!(out, "{}\n", init_text.trim_end());
    let _ = writeln!(out, "Checks: {} passed", summary.passed);
    for line in &summary.failed_required {
        let _ = writeln!(out, "  ✗ {line}");
    }
    for line in &summary.advice {
        let _ = writeln!(out, "  ! {line}");
    }
    if summary.failed_required.is_empty() {
        let _ = writeln!(
            out,
            "\nReady. Next:\n  aaf board add --title \"…\" --spec \"…\" --accept \"…\"\n  aaf run\n  aaf ui   (then open the Studio link it prints: http://127.0.0.1:4317/studio)"
        );
    } else {
        let _ = writeln!(
            out,
            "\nFix the ✗ items above, then run aaf setup again (it is safe to re-run)."
        );
    }
    out.trim_end().to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn no_agent_on_path_falls_back_to_claude() {
        assert_eq!(detect_hosts(&OsString::new()), vec!["claude"]);
    }

    #[test]
    fn detects_each_installed_agent_once_in_order() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        for bin in ["opencode", "agy", "claude", "gemini"] {
            let path = dir.path().join(bin);
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let hosts = detect_hosts(dir.path().as_os_str());
        // gemini is covered by the antigravity entry.
        assert_eq!(hosts, vec!["claude", "antigravity", "opencode"]);
    }

    #[test]
    fn summary_splits_required_failures_from_advice() {
        let checks = json!([
            {"name": "git", "ok": true, "required": true, "applicable": true, "detail": "ok"},
            {"name": "hook", "ok": false, "required": true, "applicable": true, "detail": "missing"},
            {"name": "sandbox", "ok": false, "required": false, "applicable": true, "detail": "none"},
            {"name": "repo", "ok": true, "required": false, "applicable": false, "detail": "n/a"},
        ]);
        let s = summarize(&checks);
        assert_eq!(s.passed, 1);
        assert_eq!(s.failed_required, vec!["hook: missing"]);
        assert_eq!(s.advice, vec!["sandbox: none"]);
        let text = render("claude", "init ok", &s, Path::new("/r"));
        assert!(text.contains("✗ hook: missing"));
        assert!(text.contains("run aaf setup again"));
        let ok = render("claude", "init ok", &Summary::default(), Path::new("/r"));
        assert!(ok.contains("Ready. Next:"));
        assert!(ok.contains("/studio"));
    }
}
