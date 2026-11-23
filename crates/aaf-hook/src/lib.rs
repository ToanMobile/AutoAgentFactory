//! `aaf-hook`: the PreToolUse guard for Claude Code.
//!
//! [`decide`] is a pure function of the hook payload and a [`HookEnv`]; the
//! binary only builds the real environment and prints the [`Outcome`].
//! This is a fence against accidents, not a security boundary: the real
//! boundary is the sandbox and server-side branch protection.

use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

pub mod adb_guard;
pub mod data_guard;
pub mod delete_guard;
pub mod fallback;
pub mod git_alias;
pub mod git_files;
pub mod git_guard;
pub mod git_persist;
pub mod heavy_guard;
pub mod human_guard;
pub mod interp_guard;
pub mod paths;
pub mod pr_merge_guard;
pub mod secret_guard;
pub mod shell;
pub mod store_guard;

/// Everything the guards need from the outside world.
pub struct HookEnv<'a> {
    /// Work-tree root of the repository containing a directory. Called only
    /// when a guard matched, never on the fast path.
    pub repo_root: &'a dyn Fn(&Path) -> Option<PathBuf>,
    /// `git config --list -z` as git reads it in a directory: every key in
    /// effect there (system, global, repository, worktree, includes). `None`
    /// when git could not say. Called only for a git subcommand that is not
    /// a known builtin, to find the alias it may be.
    pub git_config: &'a dyn Fn(&Path) -> Option<String>,
    /// `AAF_DATA_DIR` (receipts, signing key).
    pub data_dir: Option<PathBuf>,
    /// `AAF_CONFIG_DIR`: global `adb-denylist` / `adb-allowlist`.
    pub config_dir: Option<PathBuf>,
    pub home: Option<PathBuf>,
    /// The hook process's own working directory: the base for relative paths
    /// when the payload carries no `cwd`.
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denial {
    pub rule: &'static str,
    pub reason: String,
    pub fix: String,
}

impl Denial {
    pub fn new(rule: &'static str, reason: impl Into<String>, fix: impl Into<String>) -> Denial {
        Denial {
            rule,
            reason: reason.into(),
            fix: fix.into(),
        }
    }

    /// `<rule>: <reason> — fix: <action>`
    pub fn message(&self) -> String {
        format!("{}: {} — fix: {}", self.rule, self.reason, self.fix)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(Denial),
}

impl Decision {
    pub fn is_deny(&self) -> bool {
        matches!(self, Decision::Deny(_))
    }
}

/// What the process prints and its exit code (0 allow, 2 deny).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

/// What the whole line does that changes how one command on it is read. A
/// variable set by an earlier command (`export GIT_DIR=…; git fp`) is not
/// among the later command's own assignments.
#[derive(Debug, Clone, Copy, Default)]
pub struct Line {
    /// The word `aaf` is rebound: see [`shell::Cmd::aaf_untrusted`].
    pub aaf_untrusted: bool,
    /// `GIT_DIR` or `GIT_COMMON_DIR` is set: git reads another repository's
    /// configuration than the one the hook looks in.
    pub git_dir_moved: bool,
    /// `HOME` or `XDG_CONFIG_HOME` is set: git reads another global
    /// configuration.
    pub home_moved: bool,
}

impl Line {
    fn of(script: &shell::Script) -> Line {
        let sets = |vars: [&str; 2]| {
            script
                .cmds
                .iter()
                .any(|cmd| vars.iter().any(|var| cmd.sets_var(var)))
        };
        Line {
            aaf_untrusted: script.aaf_untrusted(),
            git_dir_moved: sets(["GIT_DIR", "GIT_COMMON_DIR"]),
            home_moved: sets(["HOME", "XDG_CONFIG_HOME"]),
        }
    }
}

/// Per-call state handed to each guard.
pub struct Ctx<'a> {
    pub env: &'a HookEnv<'a>,
    pub cwd: PathBuf,
    /// The data dir as given and with symlinks resolved.
    pub data_dirs: Vec<PathBuf>,
    /// How many git aliases deep the command being judged is.
    pub depth: usize,
    pub line: Line,
}

impl<'a> Ctx<'a> {
    fn new(env: &'a HookEnv<'a>, cwd: PathBuf) -> Ctx<'a> {
        let mut data_dirs = Vec::new();
        if let Some(dir) = &env.data_dir {
            let lexical = paths::normalize(dir);
            let resolved = paths::real(&lexical);
            if resolved != lexical {
                data_dirs.push(resolved);
            }
            data_dirs.push(lexical);
        }
        Ctx {
            env,
            cwd,
            data_dirs,
            depth: 0,
            line: Line::default(),
        }
    }

    pub fn with_cwd(&self, cwd: PathBuf) -> Ctx<'a> {
        Ctx {
            env: self.env,
            cwd,
            data_dirs: self.data_dirs.clone(),
            depth: self.depth,
            line: self.line,
        }
    }

    /// The context for a command found inside a git alias.
    pub fn nested(&self) -> Ctx<'a> {
        Ctx {
            depth: self.depth + 1,
            ..self.with_cwd(self.cwd.clone())
        }
    }

    pub fn expand(&self, raw: &str) -> PathBuf {
        paths::expand(raw, &self.cwd, self.env.home.as_deref())
    }

    pub fn root(&self) -> Option<PathBuf> {
        (self.env.repo_root)(&self.cwd)
    }

    /// Outside any repository the working directory stands in for the root.
    pub fn in_repo(&self, path: &Path) -> bool {
        // Still relative after `expand`: the working directory is unknown,
        // so the path cannot be shown to lie outside the repository.
        if path.is_relative() {
            return true;
        }
        let root = self.root().unwrap_or_else(|| self.cwd.clone());
        paths::is_inside(path, &root)
    }
}

const INPUT_RULE: &str = "hook_input";

fn bad_input(reason: &str) -> Decision {
    Decision::Deny(Denial::new(
        INPUT_RULE,
        reason,
        "send the Claude Code PreToolUse JSON on stdin",
    ))
}

/// Judges one PreToolUse payload. Unknown tools are allowed; a payload that
/// cannot be read is denied.
pub fn decide(input: &[u8], env: &HookEnv) -> Decision {
    let value: Value = match serde_json::from_slice(input) {
        Ok(value) => value,
        Err(_) => return bad_input("stdin is not valid JSON"),
    };
    let Some(payload) = value.as_object() else {
        return bad_input("the payload is not a JSON object");
    };
    let tool = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let field = |name: &str| {
        payload
            .get("tool_input")
            .and_then(|input| input.get(name))
            .and_then(Value::as_str)
    };
    let given = payload
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.is_empty());
    let cwd = match (given, &env.cwd) {
        (Some(given), Some(base)) => base.join(given),
        (Some(given), None) => PathBuf::from(given),
        (None, Some(base)) => base.clone(),
        // Unknown: relative paths stay relative and the guards fail closed
        // on the ones that matter.
        (None, None) => PathBuf::new(),
    };
    let ctx = Ctx::new(env, cwd);
    let denial = match tool {
        "Bash" => match field("command") {
            Some(command) => bash(command, ctx),
            None => return bad_input("Bash call without tool_input.command"),
        },
        "Write" | "Edit" | "MultiEdit" | "Read" => match field("file_path") {
            Some(raw) => {
                let path = ctx.expand(raw);
                data_guard::check_file(raw, &path, &ctx).or_else(|| {
                    if tool == "Read" {
                        secret_guard::check_read(&path)
                    } else {
                        delete_guard::check_file(&path, &ctx)
                            .or_else(|| git_files::check_file(&path))
                    }
                })
            }
            None => return bad_input("file tool call without tool_input.file_path"),
        },
        _ => None,
    };
    denial.map_or(Decision::Allow, Decision::Deny)
}

/// Every guard on one simple command.
pub(crate) fn judge(cmd: &shell::Cmd, ctx: &Ctx) -> Option<Denial> {
    data_guard::check_cmd(cmd, ctx)
        .or_else(|| human_guard::check(cmd))
        .or_else(|| git_guard::check(cmd, ctx))
        .or_else(|| git_persist::check(cmd, ctx))
        .or_else(|| git_files::check_cmd(cmd, ctx))
        .or_else(|| adb_guard::check(cmd, ctx))
        .or_else(|| delete_guard::check_cmd(cmd, ctx))
        .or_else(|| secret_guard::check_cmd(cmd, ctx))
        .or_else(|| interp_guard::check_cmd(cmd, ctx))
        .or_else(|| pr_merge_guard::check(cmd))
        .or_else(|| store_guard::check(cmd, ctx))
        .or_else(|| heavy_guard::check(cmd))
}

fn bash(command: &str, mut ctx: Ctx) -> Option<Denial> {
    let script = shell::parse(command);
    ctx.line = Line::of(&script);
    for text in &script.opaque {
        if let Some(denial) = fallback::check(text, &ctx) {
            return Some(denial);
        }
    }
    if let Some(denial) = heavy_guard::check_script(&script.cmds) {
        return Some(denial);
    }
    for cmd in &script.cmds {
        let denial = judge(cmd, &ctx);
        if denial.is_some() {
            return denial;
        }
        // Later relative paths resolve against the new directory.
        if matches!(cmd.name(), "cd" | "pushd")
            && let Some(dir) = cmd.args().iter().find(|arg| !arg.starts_with('-'))
        {
            ctx.cwd = ctx.expand(dir);
        }
    }
    None
}

pub fn render(decision: &Decision) -> Outcome {
    match decision {
        Decision::Allow => Outcome {
            stdout: "{}".to_string(),
            stderr: String::new(),
            code: 0,
        },
        Decision::Deny(denial) => {
            let message = denial.message();
            let body = json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": message,
                }
            });
            Outcome {
                stdout: body.to_string(),
                stderr: message,
                code: 2,
            }
        }
    }
}

/// Runs `work`; a panic becomes a deny so the process never exits with
/// anything but 0 or 2.
pub fn guarded(work: impl FnOnce() -> Outcome) -> Outcome {
    panic::catch_unwind(AssertUnwindSafe(work)).unwrap_or_else(|_| {
        render(&Decision::Deny(Denial::new(
            "hook_internal",
            "the guard crashed while judging this call",
            "report the command to the AutoAgentFactory maintainers",
        )))
    })
}

/// `decide` + `render`, panic-safe.
pub fn run(input: &[u8], env: &HookEnv) -> Outcome {
    guarded(|| render(&decide(input, env)))
}
