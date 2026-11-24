//! Inspects inline scripts passed to interpreters (`python`/`python3 -c`,
//! `node -e`, `perl -e`, `ruby -e`, `php -r`) so dangerous shell/delete/git
//! operations and secret file reads cannot bypass `aaf-hook` by wrapping them
//! inside an interpreter one-liner.

use std::path::Path;

use crate::shell::{self, Cmd};
use crate::{Ctx, Denial, data_guard, delete_guard, git_files, paths, secret_guard};

mod parse;

use parse::{
    Interp, classify_interpreter, extract_delimited_commands, extract_literals,
    extract_space_call_commands, has_space_call, inline_payloads, normalize_api_haystack,
};

pub const RULE: &str = "interp_guard";

/// True when `name` is a supported interpreter (including versioned forms like `python3.12`).
pub(crate) fn is_interpreter(name: &str) -> bool {
    classify_interpreter(name).is_some()
}

fn deny(reason: String, fix: &str) -> Denial {
    Denial::new(RULE, reason, fix)
}

fn extract_bracket_commands(code: &str) -> Vec<String> {
    extract_delimited_commands(code, '[', ']')
}

fn extract_call_commands(code: &str) -> Vec<String> {
    extract_delimited_commands(code, '(', ')')
}

fn has_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

const SHELL_EXEC_APIS: &[&str] = &[
    "os.system",
    "os.popen",
    "subprocess.",
    "execSync",
    "execFileSync",
    "spawnSync",
    "child_process",
    "system(",
    "exec(",
    "passthru(",
    "qx/",
    "qx{",
    "qx(",
];

const RMTREE_APIS: &[&str] = &[
    "shutil.rmtree",
    "rmtree(",
    "rmSync(",
    "fs.rm(",
    "rmdirSync(",
    "FileUtils.rm_rf",
    "FileUtils.rm_r",
    "remove_tree(",
];

const FILE_DELETE_APIS: &[&str] = &[
    "os.unlink(",
    "os.remove(",
    "os.rmdir(",
    "os.rename(",
    "os.replace(",
    ".unlink(",
];

const FILE_READ_APIS: &[&str] = &[
    "open(",
    "Path(",
    "readFileSync(",
    "readFile(",
    "File.read(",
    "File.readlines(",
    "File.binread(",
    "IO.read(",
    "IO.readlines(",
    "IO.binread(",
    "file_get_contents(",
    "readfile(",
    "file(",
    "parse_ini_file(",
    "highlight_file(",
    "show_source(",
];

const PERL_RUBY_SPACE_READ_APIS: &[&str] = &[
    "open",
    "File.read",
    "File.open",
    "File.readlines",
    "File.binread",
    "IO.read",
    "IO.readlines",
    "IO.binread",
];

const PHP_INCLUDE_APIS: &[&str] = &[
    "include(",
    "include_once(",
    "require(",
    "require_once(",
    "include ",
    "include_once ",
    "require ",
    "require_once ",
    "include'",
    "include\"",
    "include_once'",
    "include_once\"",
    "require'",
    "require\"",
    "require_once'",
    "require_once\"",
];

fn check_protected_path(
    trimmed: &str,
    takes_dirs: bool,
    interp: &str,
    ctx: &Ctx,
) -> Option<Denial> {
    if let Some(d) = delete_guard::target(trimmed, takes_dirs, interp, ctx) {
        return Some(deny(
            format!("{interp} delete of protected path blocked: {}", d.reason),
            &d.fix,
        ));
    }
    let check_git_and_data = |expanded: &Path| {
        if let Some(d) = git_files::check_file(expanded) {
            return Some(deny(
                format!("{interp} delete of git file blocked: {}", d.reason),
                &d.fix,
            ));
        }
        if let Some(d) = data_guard::check_file(trimmed, expanded, ctx) {
            return Some(deny(
                format!("{interp} delete of data dir blocked: {}", d.reason),
                &d.fix,
            ));
        }
        None
    };
    let expanded = ctx.expand(trimmed);
    if let Some(d) = check_git_and_data(&expanded) {
        return Some(d);
    }
    if trimmed.contains("..") {
        let real_dd = ctx.expand_real_dotdot(trimmed);
        if real_dd != expanded
            && let Some(d) = check_git_and_data(&real_dd)
        {
            return Some(d);
        }
    }
    None
}

fn inspect_payload(interp_kind: Interp, interp: &str, code: &str, ctx: &Ctx) -> Option<Denial> {
    // Strip pure comment lines so comments inside multiline scripts do not trip.
    let active: String = code
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with('#') && !t.starts_with("//")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let api_haystack = normalize_api_haystack(&active, interp_kind == Interp::Php);
    let literals = extract_literals(&active);

    // 1. Check recursive delete APIs (`shutil.rmtree`, `fs.rmSync`, `FileUtils.rm_rf`, ...).
    if has_any(&api_haystack, RMTREE_APIS) {
        let mut saw_path_literal = false;
        for lit in &literals {
            let trimmed = lit.trim();
            if trimmed.is_empty()
                || matches!(
                    trimmed,
                    "utf8" | "utf-8" | "r" | "w" | "rb" | "fs" | "fileutils"
                )
            {
                continue;
            }
            saw_path_literal = true;
            if let Some(d) = delete_guard::check_recursive_rm_target(trimmed, interp, ctx) {
                return Some(deny(
                    format!("{interp} recursive delete blocked: {}", d.reason),
                    &d.fix,
                ));
            }
            if let Some(d) = check_protected_path(trimmed, true, interp, ctx) {
                return Some(d);
            }
        }
        if !saw_path_literal {
            return Some(deny(
                format!("{interp} calls a recursive delete API on an unresolved dynamic path"),
                "pass an explicit repo-local or /tmp/... path, or delete files with explicit commands",
            ));
        }
    }

    // 2. Check single-file delete/rename APIs (`os.unlink`, `os.remove`, `os.rmdir`, `os.rename`, `os.replace`).
    if has_any(&api_haystack, FILE_DELETE_APIS) {
        let takes_dirs = has_any(&api_haystack, &["os.rmdir(", "os.rename(", "os.replace("]);
        for lit in &literals {
            let trimmed = lit.trim();
            if trimmed.is_empty() || trimmed.contains('\n') {
                continue;
            }
            if let Some(d) = check_protected_path(trimmed, takes_dirs, interp, ctx) {
                return Some(d);
            }
        }
    }

    // 3. Check shell-execution APIs (`os.system`, `subprocess.*`, `execSync`, `system(...)`, Perl/Ruby `system '...'`, ...).
    let perl_ruby = matches!(interp_kind, Interp::Perl | Interp::Ruby);
    let has_shell_exec = has_any(&api_haystack, SHELL_EXEC_APIS)
        || (perl_ruby && has_space_call(&active, &["system", "exec"]));
    if has_shell_exec {
        if ctx.depth >= shell::MAX_DEPTH {
            return Some(deny(
                format!("{interp} shell execution is nested too deeply to analyse"),
                "rewrite as plain commands without nested interpreters",
            ));
        }
        let mut candidates: Vec<String> = literals.clone();
        candidates.extend(extract_bracket_commands(&active));
        candidates.extend(extract_call_commands(&active));
        if perl_ruby {
            candidates.extend(extract_space_call_commands(&active));
        }
        for snippet in &candidates {
            if let Some(d) = crate::bash(snippet, ctx.nested()) {
                return Some(deny(
                    format!(
                        "{interp} wrapped blocked command ({}): {}",
                        d.rule, d.reason
                    ),
                    &d.fix,
                ));
            }
        }
    }

    // 4. Check secret file reads (`open('.env')`, `readFileSync('.env')`, PHP `include`/`require`, Ruby `File.read '.env'`, ...).
    let php_include = interp_kind == Interp::Php && has_any(&api_haystack, PHP_INCLUDE_APIS);
    let ruby_space_read = perl_ruby && has_space_call(&active, PERL_RUBY_SPACE_READ_APIS);
    if has_any(&api_haystack, FILE_READ_APIS) || php_include || ruby_space_read {
        for lit in &literals {
            let trimmed = lit.trim();
            if trimmed.is_empty() || trimmed.contains('\n') {
                continue;
            }
            let expanded = ctx.expand(trimmed);
            let pat = secret_guard::secret_pattern(&expanded)
                .or_else(|| {
                    if !trimmed.contains("..") {
                        return None;
                    }
                    let real_dd = ctx.expand_real_dotdot(trimmed);
                    (real_dd != expanded)
                        .then(|| secret_guard::secret_pattern(&real_dd))
                        .flatten()
                })
                .or_else(|| paths::is_secret_file(Path::new(trimmed)));
            if let Some(pat) = pat {
                return Some(deny(
                    format!("{interp} attempts to read secret file `{trimmed}` (pattern {pat})"),
                    "do not read secret/credential files from inline scripts; use .env.example",
                ));
            }
        }
    }

    None
}

/// True when `code` mentions any shell-exec, recursive-delete, file-delete, or
/// file-read API guarded by [`inspect_payload`].
pub(crate) fn has_dangerous_api(code: &str) -> bool {
    let norm = normalize_api_haystack(code, false);
    let lower = normalize_api_haystack(code, true);
    let check = |hay: &str| {
        has_any(hay, SHELL_EXEC_APIS)
            || has_any(hay, RMTREE_APIS)
            || has_any(hay, FILE_DELETE_APIS)
            || has_any(hay, FILE_READ_APIS)
    };
    check(&norm)
        || check(&lower)
        || has_space_call(code, &["system", "exec"])
        || has_space_call(code, PERL_RUBY_SPACE_READ_APIS)
}

pub fn check_cmd(cmd: &Cmd, ctx: &Ctx) -> Option<Denial> {
    let name = cmd.name();
    let interp = classify_interpreter(name)?;
    for payload in inline_payloads(interp, cmd.args()) {
        if let Some(denial) = inspect_payload(interp, name, &payload, ctx) {
            return Some(denial);
        }
    }
    for payload in shell::stdin_scripts(&cmd.redirects, cmd.feeder.as_deref()) {
        if let Some(denial) = inspect_payload(interp, name, &payload, ctx) {
            return Some(denial);
        }
    }
    for payload in &cmd.heredocs {
        if let Some(denial) = inspect_payload(interp, name, payload, ctx) {
            return Some(denial);
        }
    }
    None
}
