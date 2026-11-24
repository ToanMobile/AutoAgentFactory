//! `aaf-hook`: the PreToolUse guard for Claude Code.
//!
//! [`decide`] is a pure function of the hook payload and a [`HookEnv`]; the
//! binary only builds the real environment and prints the [`Outcome`].
//! This is a fence against accidents, not a security boundary: the real
//! boundary is the sandbox and server-side branch protection.

use std::cell::OnceCell;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Component, Path, PathBuf};

use serde_json::{Value, json};

pub mod adb_guard;
mod cwd;
pub mod data_guard;
pub mod delete_guard;
pub mod fallback;
pub mod git_alias;
pub mod git_files;
pub mod git_guard;
pub mod git_persist;
pub mod heavy_guard;
pub mod host;
pub mod human_guard;
pub mod interp_guard;
pub mod paths;
pub mod pr_merge_guard;
pub mod secret_guard;
pub(crate) mod sed_script;
pub mod shell;
pub mod store_guard;
mod tool_exec;

pub(crate) use cwd::bash;

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
    /// True when `AAF_RUN_ID` is set and non-empty (autonomous factory worker run).
    /// When `false`, the hook is running in an interactive user session.
    pub in_factory_run: bool,
    /// Trusted absolute paths to the installed `aaf` CLI binary (lexical and
    /// canonical forms of the sibling `aaf` next to `aaf-hook`, plus any PATH
    /// match whose canonical path equals the canonical sibling outside temp dirs).
    pub trusted_aaf_bins: Vec<PathBuf>,
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
    pub(crate) fn of(script: &shell::Script) -> Line {
        let sets = |vars: [&str; 2]| {
            script
                .cmds
                .iter()
                .any(|cmd| vars.iter().any(|var| cmd.sets_var(var)))
        };
        let home_moved = script.cmds.iter().any(|cmd| {
            (cmd.name() != "git" || cmd.dynamic_name)
                && (cmd.sets_var("HOME") || cmd.sets_var("XDG_CONFIG_HOME"))
        });
        Line {
            aaf_untrusted: script.aaf_untrusted(),
            git_dir_moved: sets(["GIT_DIR", "GIT_COMMON_DIR"]),
            home_moved,
        }
    }
}

/// Per-call state handed to each guard.
pub struct Ctx<'a> {
    pub env: &'a HookEnv<'a>,
    /// Original workspace working directory from the hook payload/env before
    /// any `cd`, `env -C`, or tool `workdir` override.
    pub workspace_cwd: PathBuf,
    pub cwd: PathBuf,
    /// Cached `paths::real(&self.cwd)` so non-existent tokens under `cwd` avoid
    /// repeated ancestor `canonicalize` syscalls.
    real_cwd: OnceCell<PathBuf>,
    /// The data dir as given and with symlinks resolved.
    pub data_dirs: Vec<PathBuf>,
    /// How many git aliases deep the command being judged is.
    pub depth: usize,
    pub line: Line,
    /// Shell variables assigned directly from `mktemp -d` earlier in the same
    /// script and never reassigned or unset before the current command.
    pub mktemp_vars: Vec<String>,
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
            workspace_cwd: cwd.clone(),
            cwd,
            real_cwd: OnceCell::new(),
            data_dirs,
            depth: 0,
            line: Line::default(),
            mktemp_vars: Vec::new(),
        }
    }

    pub fn with_cwd(&self, cwd: PathBuf) -> Ctx<'a> {
        let real_cwd = if cwd == self.cwd {
            self.real_cwd.clone()
        } else {
            OnceCell::new()
        };
        Ctx {
            env: self.env,
            workspace_cwd: self.workspace_cwd.clone(),
            cwd,
            real_cwd,
            data_dirs: self.data_dirs.clone(),
            depth: self.depth,
            line: self.line,
            mktemp_vars: self.mktemp_vars.clone(),
        }
    }

    /// The context for a command found inside a git alias.
    pub fn nested(&self) -> Ctx<'a> {
        Ctx {
            depth: self.depth + 1,
            ..self.with_cwd(self.cwd.clone())
        }
    }

    pub(crate) fn set_cwd(&mut self, cwd: PathBuf) {
        if cwd != self.cwd {
            self.cwd = cwd;
            self.real_cwd = OnceCell::new();
        }
    }

    pub fn expand(&self, raw: &str) -> PathBuf {
        paths::expand(raw, &self.cwd, self.env.home.as_deref())
    }

    pub fn expand_real_dotdot(&self, raw: &str) -> PathBuf {
        paths::expand_real_dotdot(raw, &self.cwd, self.env.home.as_deref())
    }

    fn real_cwd(&self) -> &Path {
        self.real_cwd.get_or_init(|| paths::real(&self.cwd))
    }

    /// Symlink-free form of `path`, using a cached `real(cwd)` fast path when
    /// `path` is under `self.cwd` and its first relative component does not exist.
    pub fn real_of(&self, path: &Path) -> PathBuf {
        if !self.cwd.as_os_str().is_empty()
            && let Ok(rel) = path.strip_prefix(&self.cwd)
        {
            if rel.as_os_str().is_empty() {
                return self.real_cwd().to_path_buf();
            }
            let mut comps = rel.components();
            if let Some(Component::Normal(first)) = comps.next()
                && !comps.any(|c| matches!(c, Component::ParentDir))
                && self.cwd.join(first).symlink_metadata().is_err()
            {
                return self.real_cwd().join(rel);
            }
        }
        paths::real(path)
    }

    pub fn root(&self) -> Option<PathBuf> {
        (self.env.repo_root)(&self.cwd)
    }

    pub fn workspace_root(&self) -> Option<PathBuf> {
        (self.env.repo_root)(&self.workspace_cwd)
    }

    /// Outside any repository the working directory stands in for the root.
    pub fn in_repo(&self, path: &Path) -> bool {
        // Still relative after `expand`: the working directory is unknown,
        // so the path cannot be shown to lie outside the repository.
        if path.is_relative() {
            return true;
        }
        let root = self.root().unwrap_or_else(|| self.cwd.clone());
        if paths::is_inside(path, &root) {
            return true;
        }
        // A `cd` or `env -C` out of the repository does not take its files
        // out of reach: the repository the call started in still counts.
        if self.workspace_cwd == self.cwd || self.workspace_cwd.as_os_str().is_empty() {
            return false;
        }
        let workspace = self
            .workspace_root()
            .unwrap_or_else(|| self.workspace_cwd.clone());
        paths::is_inside(path, &workspace)
    }

    /// `path` is the repository the call started in, or a directory holding
    /// it: removing it removes the whole repository.
    pub fn holds_workspace_repo(&self, path: &Path) -> bool {
        if self.workspace_cwd.as_os_str().is_empty() || path.as_os_str().is_empty() {
            return false;
        }
        // Only a path at or above the starting directory can hold its
        // repository, so the root lookup stays off the common path.
        let above = |cwd: &Path| cwd.starts_with(path);
        if !above(&self.workspace_cwd) && !above(&paths::real(&self.workspace_cwd)) {
            return false;
        }
        self.workspace_root()
            .is_some_and(|root| paths::is_inside(&root, path))
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
    let _resolving = paths::Resolving::start();
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
                let check_one = |path: &Path| {
                    data_guard::check_file(raw, path, &ctx).or_else(|| {
                        if tool == "Read" {
                            secret_guard::check_read_target(raw, path, &ctx)
                        } else {
                            delete_guard::check_file(path, &ctx)
                                .or_else(|| git_files::check_file(path))
                        }
                    })
                };
                let path = ctx.expand(raw);
                check_one(&path).or_else(|| {
                    if !raw.contains("..") {
                        return None;
                    }
                    let real_dd = ctx.expand_real_dotdot(raw);
                    (real_dd != path).then(|| check_one(&real_dd)).flatten()
                })
            }
            None => return bad_input("file tool call without tool_input.file_path"),
        },
        _ => None,
    };
    denial.map_or(Decision::Allow, Decision::Deny)
}

/// Applies any leading `-C <dir>` prefix before handing `git` commands to
/// `delete_guard`, so chained `-C` flags (e.g. `git -C .claude -C . rm ...`)
/// resolve cumulatively while redirects stay relative to the shell's `cwd`.
fn check_delete_guard(cmd: &shell::Cmd, ctx: &Ctx) -> Option<Denial> {
    if cmd.name() == "git" && !cmd.dynamic_name {
        let call = git_guard::split(cmd.args());
        if let Some(prefix) = call.resolve_prefix_dir(ctx) {
            if !cmd.redirects.is_empty() {
                let redirects_only = shell::Cmd {
                    redirects: cmd.redirects.clone(),
                    ..shell::Cmd::default()
                };
                if let Some(denial) = delete_guard::check_cmd(&redirects_only, ctx) {
                    return Some(denial);
                }
                let no_redirects = shell::Cmd {
                    redirects: Vec::new(),
                    ..cmd.clone()
                };
                let moved = ctx.with_cwd(prefix);
                return delete_guard::check_cmd(&no_redirects, &moved);
            }
            let moved = ctx.with_cwd(prefix);
            return delete_guard::check_cmd(cmd, &moved);
        }
    }
    delete_guard::check_cmd(cmd, ctx)
}

fn judge_direct(cmd: &shell::Cmd, ctx: &Ctx) -> Option<Denial> {
    data_guard::check_cmd(cmd, ctx)
        .or_else(|| human_guard::check(cmd, ctx))
        .or_else(|| git_guard::check(cmd, ctx))
        .or_else(|| git_persist::check(cmd, ctx))
        .or_else(|| git_files::check_cmd(cmd, ctx))
        .or_else(|| adb_guard::check(cmd, ctx))
        .or_else(|| check_delete_guard(cmd, ctx))
        .or_else(|| secret_guard::check_cmd(cmd, ctx))
        .or_else(|| interp_guard::check_cmd(cmd, ctx))
        .or_else(|| tool_exec::check_cmd(cmd, ctx))
        .or_else(|| pr_merge_guard::check(cmd))
        .or_else(|| store_guard::check(cmd, ctx))
        .or_else(|| heavy_guard::check(cmd, ctx))
}

/// Every guard on one simple command. When a wrapper changes the working
/// directory (`env -C` / `sudo -D`), file redirections are opened by the outer
/// shell in the original `ctx.cwd` and judged there first, while here-strings
/// (`<<<`) remain with the command in the moved directory.
pub(crate) fn judge(cmd: &shell::Cmd, ctx: &Ctx) -> Option<Denial> {
    if let Some(cwd) = cwd::wrapped_cwd(&cmd.wrapped, ctx) {
        let has_file_redirects = cmd.redirects.iter().any(|r| r.op != "<<<");
        if has_file_redirects {
            let redirects_only = shell::Cmd {
                redirects: cmd
                    .redirects
                    .iter()
                    .filter(|r| r.op != "<<<")
                    .cloned()
                    .collect(),
                ..shell::Cmd::default()
            };
            if let Some(denial) = judge_direct(&redirects_only, ctx) {
                return Some(denial);
            }
            let no_file_redirects = shell::Cmd {
                redirects: cmd
                    .redirects
                    .iter()
                    .filter(|r| r.op == "<<<")
                    .cloned()
                    .collect(),
                ..cmd.clone()
            };
            let moved = ctx.with_cwd(cwd);
            return judge_direct(&no_file_redirects, &moved);
        }
        let moved = ctx.with_cwd(cwd);
        return judge_direct(cmd, &moved);
    }
    judge_direct(cmd, ctx)
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
