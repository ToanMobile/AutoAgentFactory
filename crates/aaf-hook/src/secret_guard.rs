//! Blocks reading or exfiltrating secret files (`.env`, `.env.*` except public
//! templates, keystores, `keystore.properties`, `google-services.json`, PEM/P12,
//! SSH private keys) through both the `Read` file tool and `Bash` file-reading
//! commands or input redirections.

use std::fs;
use std::path::Path;

use crate::shell::Cmd;
use crate::{Ctx, Denial, paths};

pub const RULE: &str = "secret_guard";
const FIX: &str =
    "do not read secret/credential files; use .env.example or ask the user for non-secret config";

fn deny(reason: String) -> Denial {
    Denial::new(RULE, reason, FIX)
}

/// Checks `path` and its symlink target against the secret file patterns.
pub fn secret_pattern(path: &Path) -> Option<&'static str> {
    paths::is_secret_file(path).or_else(|| paths::is_secret_file(&paths::real(path)))
}

/// Guard for the `Read` file tool.
pub fn check_read(path: &Path) -> Option<Denial> {
    let pattern = secret_pattern(path)?;
    Some(deny(format!(
        "reading {} is blocked (secret file pattern {pattern})",
        path.display()
    )))
}

/// Commands whose non-option operands are file paths being read, copied, or printed.
const READERS: &[&str] = &[
    "cat", "head", "tail", "less", "more", "bat", "sed", "awk", "gawk", "mawk", "grep", "egrep",
    "fgrep", "rg", "strings", "base64", "xxd", "hexdump", "od", "nl", "tac", "rev", "pbcopy",
    "tee", "cp", "dd", "cut", "sort", "uniq", "wc", "diff", "cmp", "source", ".",
];

/// Commands whose first positional operand is a pattern/script rather than a file
/// unless `-e`/`-f`/`--regexp`/`--file` was supplied.
const FIRST_POS_IS_EXPR: &[&str] = &["grep", "egrep", "fgrep", "rg", "sed", "awk", "gawk", "mawk"];

/// Options of file-reading commands that take a separate non-file argument value.
fn option_takes_value(cmd: &str, opt: &str) -> bool {
    match cmd {
        "head" | "tail" => matches!(opt, "-n" | "-c" | "--lines" | "--bytes"),
        "grep" | "egrep" | "fgrep" | "rg" => matches!(
            opt,
            "-e" | "--regexp"
                | "-m"
                | "--max-count"
                | "-A"
                | "-B"
                | "-C"
                | "--after-context"
                | "--before-context"
                | "--context"
                | "-g"
                | "--glob"
                | "-t"
                | "--type"
                | "--include"
                | "--exclude"
        ),
        "sed" => matches!(opt, "-e" | "--expression" | "-i"),
        "awk" | "gawk" | "mawk" => matches!(opt, "-F" | "-v"),
        "cut" => matches!(opt, "-d" | "-f" | "-c" | "-b"),
        "sort" => matches!(opt, "-t" | "-k" | "-o"),
        "base64" => matches!(opt, "-i" | "-o" | "-b" | "-w"),
        _ => false,
    }
}

fn check_target(raw: &str, verb: &str, ctx: &Ctx) -> Option<Denial> {
    if raw.is_empty() || raw == "-" {
        return None;
    }
    let path = ctx.expand(raw);
    if paths::has_glob(raw) {
        if let Some(parent) = path.parent()
            && let Some(last) = path.file_name().and_then(|n| n.to_str())
            && let Ok(entries) = fs::read_dir(parent)
        {
            for entry in entries.flatten() {
                if let Ok(name) = entry.file_name().into_string()
                    && paths::glob_match(last, &name)
                    && let Some(pat) = secret_pattern(&parent.join(&name))
                {
                    return Some(deny(format!(
                        "{verb} {raw}: glob matches secret file {name} (pattern {pat})"
                    )));
                }
            }
        }
        return None;
    }
    let pat = secret_pattern(&path)?;
    Some(deny(format!(
        "{verb} {raw}: reading secret file is blocked (pattern {pat})"
    )))
}

/// Guard for `Bash` commands reading secret files via operands or `<` redirections.
pub fn check_cmd(cmd: &Cmd, ctx: &Ctx) -> Option<Denial> {
    // 1. Check input redirections (`< .env`) on ANY command (e.g. `pbcopy < .env`).
    for redirect in &cmd.redirects {
        if redirect.op == "<"
            && let Some(denial) = check_target(&redirect.target, "redirect from", ctx)
        {
            return Some(denial);
        }
    }

    let name = cmd.name();
    if !READERS.contains(&name) {
        return None;
    }

    let args = cmd.args();
    // Special case: `dd if=<path>`
    if name == "dd" {
        for arg in args {
            if let Some(target) = arg.strip_prefix("if=")
                && let Some(denial) = check_target(target, "dd", ctx)
            {
                return Some(denial);
            }
        }
        return None;
    }

    let mut has_explicit_expr = false;
    let mut positionals: Vec<&str> = Vec::new();
    let mut only_pos = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if only_pos {
            positionals.push(arg);
            i += 1;
            continue;
        }
        if arg == "--" {
            only_pos = true;
            i += 1;
            continue;
        }
        if arg == "-e" || arg == "--regexp" || arg == "--expression" {
            has_explicit_expr = true;
            i += 2;
            continue;
        }
        if arg == "-f" || arg == "--file" {
            has_explicit_expr = true;
            if let Some(file_arg) = args.get(i + 1)
                && let Some(denial) = check_target(file_arg, name, ctx)
            {
                return Some(denial);
            }
            i += 2;
            continue;
        }
        if arg.starts_with('-') && arg.len() > 1 {
            if !arg.contains('=') && option_takes_value(name, arg) {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        positionals.push(arg);
        i += 1;
    }

    let skip_first = FIRST_POS_IS_EXPR.contains(&name) && !has_explicit_expr;
    let file_args = if skip_first && !positionals.is_empty() {
        &positionals[1..]
    } else {
        &positionals[..]
    };

    file_args
        .iter()
        .find_map(|arg| check_target(arg, name, ctx))
}
