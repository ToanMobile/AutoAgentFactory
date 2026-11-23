//! Inspects inline scripts passed to interpreters (`python`/`python3 -c`,
//! `node -e`, `perl -e`, `ruby -e`, `php -r`) so dangerous shell/delete/git
//! operations and secret file reads cannot bypass `aaf-hook` by wrapping them
//! inside an interpreter one-liner.

use std::path::Path;

use crate::shell::{self, Cmd};
use crate::{Ctx, Denial, delete_guard, paths, secret_guard};

pub const RULE: &str = "interp_guard";

const INTERPRETERS: &[&str] = &[
    "python", "python3", "python2", "node", "nodejs", "perl", "ruby", "php",
];

fn deny(reason: String, fix: &str) -> Denial {
    Denial::new(RULE, reason, fix)
}

/// Extracts inline code payloads passed via `-c`, `-e`, `--eval`, `-r`, `-p`, `-n`.
fn inline_payloads(name: &str, args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            break;
        }
        let is_code_flag = match name {
            "python" | "python3" | "python2" => arg == "-c",
            "node" | "nodejs" => matches!(arg, "-e" | "--eval" | "-p" | "--print"),
            "perl" => matches!(arg, "-e" | "-E"),
            "ruby" => arg == "-e",
            "php" => arg == "-r",
            _ => false,
        };
        if is_code_flag {
            if let Some(code) = args.get(i + 1) {
                out.push(code.clone());
            }
            i += 2;
            continue;
        }
        // Handle glued `-c...` or `-e...` or `--eval=...`.
        if let Some(rest) = arg.strip_prefix("--eval=") {
            out.push(rest.to_string());
        } else if matches!(name, "python" | "python3" | "python2")
            && let Some(rest) = arg.strip_prefix("-c")
            && !rest.is_empty()
        {
            out.push(rest.to_string());
        } else if matches!(name, "node" | "nodejs" | "perl" | "ruby")
            && let Some(rest) = arg.strip_prefix("-e")
            && !rest.is_empty()
        {
            out.push(rest.to_string());
        }
        i += 1;
    }
    out
}

/// Extracts single- and double-quoted string literals from an inline script.
fn extract_literals(code: &str) -> Vec<String> {
    let mut out = Vec::new();
    let chars: Vec<char> = code.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let q = chars[i];
        if q != '\'' && q != '"' && q != '`' {
            i += 1;
            continue;
        }
        i += 1;
        let mut buf = String::new();
        while i < chars.len() {
            let c = chars[i];
            if c == '\\' && i + 1 < chars.len() {
                buf.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == q {
                i += 1;
                break;
            }
            buf.push(c);
            i += 1;
        }
        if !buf.is_empty() {
            out.push(buf);
        }
    }
    out
}

/// Extracts inline shell-like list invocations such as `['git', 'reset', '--hard']`.
fn extract_bracket_commands(code: &str) -> Vec<String> {
    let mut cmds = Vec::new();
    let mut start = 0;
    while let Some(open) = code[start..].find('[') {
        let abs_open = start + open;
        let Some(close) = code[abs_open + 1..].find(']') else {
            break;
        };
        let inside = &code[abs_open + 1..abs_open + 1 + close];
        let lits = extract_literals(inside);
        if lits.len() >= 2 {
            cmds.push(lits.join(" "));
        }
        start = abs_open + 1 + close + 1;
    }
    cmds
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

const FILE_READ_APIS: &[&str] = &[
    "open(",
    "Path(",
    "readFileSync(",
    "readFile(",
    "File.read(",
    "IO.read(",
    "file_get_contents(",
];

fn inspect_payload(interp: &str, code: &str, ctx: &Ctx) -> Option<Denial> {
    // Strip pure comment lines so comments inside multiline scripts do not trip.
    let active: String = code
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with('#') && !t.starts_with("//")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let literals = extract_literals(&active);

    // 1. Check recursive delete APIs (`shutil.rmtree`, `fs.rmSync`, `FileUtils.rm_rf`, ...).
    if has_any(&active, RMTREE_APIS) {
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
            let expanded = ctx.expand(trimmed);
            if let Some(d) = delete_guard::check_file(&expanded, ctx) {
                return Some(deny(
                    format!("{interp} delete of protected path blocked: {}", d.reason),
                    &d.fix,
                ));
            }
        }
        if !saw_path_literal {
            return Some(deny(
                format!("{interp} calls a recursive delete API on an unresolved dynamic path"),
                "pass an explicit repo-local or /tmp/... path, or delete files with explicit commands",
            ));
        }
    }

    // 2. Check shell-execution APIs (`os.system`, `subprocess.*`, `execSync`, `system(...)`, ...).
    if has_any(&active, SHELL_EXEC_APIS) {
        let mut candidates: Vec<String> = literals.clone();
        candidates.extend(extract_bracket_commands(&active));
        for snippet in &candidates {
            let script = shell::parse(snippet);
            for sub in &script.cmds {
                if let Some(d) = crate::judge(sub, ctx) {
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
    }

    // 3. Check secret file reads (`open('.env')`, `readFileSync('.env')`, ...).
    if has_any(&active, FILE_READ_APIS) {
        for lit in &literals {
            let trimmed = lit.trim();
            if trimmed.is_empty() || trimmed.contains('\n') {
                continue;
            }
            let expanded = ctx.expand(trimmed);
            if let Some(pat) = secret_guard::secret_pattern(&expanded)
                .or_else(|| paths::is_secret_file(Path::new(trimmed)))
            {
                return Some(deny(
                    format!("{interp} attempts to read secret file `{trimmed}` (pattern {pat})"),
                    "do not read secret/credential files from inline scripts; use .env.example",
                ));
            }
        }
    }

    None
}

pub fn check_cmd(cmd: &Cmd, ctx: &Ctx) -> Option<Denial> {
    let name = cmd.name();
    if !INTERPRETERS.contains(&name) {
        return None;
    }
    for payload in inline_payloads(name, cmd.args()) {
        if let Some(denial) = inspect_payload(name, &payload, ctx) {
            return Some(denial);
        }
    }
    None
}
