//! Argument parsing, literal extraction, and API-haystack normalization for
//! `interp_guard`.

use crate::shell;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Interp {
    Python,
    Node,
    Perl,
    Ruby,
    Php,
}

fn is_version_suffix(rest: &str) -> bool {
    rest.is_empty()
        || (rest.starts_with(|c: char| c.is_ascii_digit())
            && rest.chars().all(|c| c.is_ascii_digit() || c == '.'))
}

pub(super) fn classify_interpreter(name: &str) -> Option<Interp> {
    let lower = name.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("python")
        && is_version_suffix(rest)
    {
        return Some(Interp::Python);
    }
    if let Some(rest) = lower
        .strip_prefix("nodejs")
        .or_else(|| lower.strip_prefix("node"))
        && is_version_suffix(rest)
    {
        return Some(Interp::Node);
    }
    if let Some(rest) = lower.strip_prefix("perl")
        && is_version_suffix(rest)
    {
        return Some(Interp::Perl);
    }
    if let Some(rest) = lower.strip_prefix("ruby")
        && is_version_suffix(rest)
    {
        return Some(Interp::Ruby);
    }
    if let Some(rest) = lower.strip_prefix("php")
        && is_version_suffix(rest)
    {
        return Some(Interp::Php);
    }
    None
}

pub(super) struct InterpSpec {
    pub(super) code_short: &'static str,
    pub(super) code_long: &'static [&'static str],
    pub(super) other_valued_short: &'static str,
    pub(super) attached_only_short: &'static str,
    pub(super) other_valued_long: &'static [&'static str],
    pub(super) node_print: bool,
}

pub(super) fn interp_spec(interp: Interp) -> InterpSpec {
    match interp {
        Interp::Python => InterpSpec {
            code_short: "c",
            code_long: &[],
            other_valued_short: "WXm",
            attached_only_short: "",
            other_valued_long: &["check-hash-based-pycs"],
            node_print: false,
        },
        Interp::Node => InterpSpec {
            code_short: "e",
            code_long: &["eval", "print"],
            other_valued_short: "rC",
            attached_only_short: "",
            other_valued_long: &[
                "require",
                "import",
                "loader",
                "conditions",
                "input-type",
                "env-file",
                "max-old-space-size",
            ],
            node_print: true,
        },
        Interp::Perl => InterpSpec {
            code_short: "eE",
            code_long: &[],
            other_valued_short: "IFMm",
            attached_only_short: "i",
            other_valued_long: &[],
            node_print: false,
        },
        Interp::Ruby => InterpSpec {
            code_short: "e",
            code_long: &[],
            other_valued_short: "IrCFK",
            attached_only_short: "i",
            other_valued_long: &["encoding", "external-encoding", "internal-encoding"],
            node_print: false,
        },
        Interp::Php => InterpSpec {
            code_short: "rRBE",
            code_long: &["run", "process-begin", "process-code", "process-end"],
            other_valued_short: "cdfFz",
            attached_only_short: "",
            other_valued_long: &[
                "php-ini",
                "define",
                "file",
                "process-file",
                "zend-extension",
            ],
            node_print: false,
        },
    }
}

/// Extracts inline code payloads passed via `-c`, `-e`, `-E`, `--eval`, `-r`, `-p`, `--print`,
/// including attached forms (`perl -E...`, `php -r...`, `node -p...`, `node --print=...`)
/// and short-flag clusters (`python3 -uc`, `node -pe`, `perl -nle`, `ruby -we`, `php -nr`).
pub(super) fn inline_payloads(interp: Interp, args: &[String]) -> Vec<String> {
    let spec = interp_spec(interp);
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            break;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (flag, glued) = long
                .split_once('=')
                .map_or((long, None), |(f, v)| (f, Some(v)));
            if spec.code_long.contains(&flag) {
                if let Some(code) = glued {
                    out.push(code.to_string());
                    i += 1;
                } else if spec.node_print && flag == "print" {
                    if let Some(next) = args.get(i + 1)
                        && !next.starts_with('-')
                    {
                        out.push(next.clone());
                        i += 2;
                    } else {
                        i += 1;
                    }
                } else {
                    if let Some(code) = args.get(i + 1) {
                        out.push(code.clone());
                    }
                    i += 2;
                }
                continue;
            }
            if glued.is_none() && spec.other_valued_long.contains(&flag) {
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if arg.starts_with('-') && arg.len() > 1 {
            let mut chars = arg.chars();
            chars.next();
            let mut step = 1;
            while let Some(c) = chars.next() {
                let rest = chars.as_str();
                if spec.node_print && c == 'p' {
                    if rest.is_empty() {
                        if let Some(next) = args.get(i + 1)
                            && !next.starts_with('-')
                        {
                            out.push(next.clone());
                            step = 2;
                        }
                        break;
                    }
                    if rest == "e" {
                        continue;
                    }
                    out.push(rest.to_string());
                    if let Some(after_e) = rest.strip_prefix('e')
                        && !after_e.is_empty()
                    {
                        out.push(after_e.to_string());
                    }
                    break;
                }
                if spec.code_short.contains(c) {
                    if !rest.is_empty() {
                        out.push(rest.to_string());
                    } else {
                        if let Some(code) = args.get(i + 1) {
                            out.push(code.clone());
                        }
                        step = 2;
                    }
                    break;
                }
                if spec.other_valued_short.contains(c) {
                    if rest.is_empty() {
                        step = 2;
                    }
                    break;
                }
                if spec.attached_only_short.contains(c) {
                    break;
                }
            }
            i += step;
            continue;
        }
        i += 1;
    }
    out
}

/// Extracts single-, double-, and backtick-quoted string literals from a script.
pub(super) fn extract_literals(code: &str) -> Vec<String> {
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

/// Extracts shell-like argv invocations inside balanced `open..close` groups
/// (such as `['git', 'reset', '--hard']` or `('rm', ['-rf', '/'])`), single-
/// quoting each literal so embedded spaces stay within one argument.
pub(super) fn extract_delimited_commands(code: &str, open: char, close: char) -> Vec<String> {
    let mut cmds = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (idx, c) in code.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        if matches!(c, '\'' | '"' | '`') {
            quote = Some(c);
        } else if c == open {
            stack.push(idx + c.len_utf8());
        } else if c == close
            && let Some(start) = stack.pop()
        {
            let lits = extract_literals(&code[start..idx]);
            if lits.len() >= 2 {
                cmds.push(
                    lits.iter()
                        .map(|s| shell::shell_quote(s))
                        .collect::<Vec<_>>()
                        .join(" "),
                );
            }
        }
    }
    cmds
}

/// Collapses horizontal whitespace immediately before `(` (and optionally
/// ASCII-lowercases for case-insensitive PHP function names).
pub(super) fn normalize_api_haystack(code: &str, lowercase: bool) -> String {
    let mut out = String::with_capacity(code.len());
    for c in code.chars() {
        if c == '(' {
            while out.ends_with([' ', '\t']) {
                out.pop();
            }
            out.push('(');
        } else if lowercase {
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Finds unquoted occurrences of `names` at an identifier boundary followed by
/// horizontal whitespace (`' '` or `'\t'`), used for Perl/Ruby space calls such
/// as `system 'rm -rf /'` or `File.read '.env'`.
pub(super) fn has_space_call(code: &str, names: &[&str]) -> bool {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut prev: Option<char> = None;
    for (idx, c) in code.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            prev = Some(c);
            continue;
        }
        if matches!(c, '\'' | '"' | '`') {
            quote = Some(c);
            prev = Some(c);
            continue;
        }
        let boundary_ok = prev.is_none_or(|p| !is_ident_char(p));
        if boundary_ok {
            let tail = &code[idx..];
            for name in names {
                if let Some(after) = tail.strip_prefix(name)
                    && after.starts_with([' ', '\t'])
                {
                    return true;
                }
            }
        }
        prev = Some(c);
    }
    false
}

/// Extracts multi-argument Perl/Ruby space-separated `system` / `exec` calls
/// (e.g. `system 'rm', '-rf', '/'`), joining their literal arguments with shell
/// quoting so `bash()` evaluates the assembled command.
pub(super) fn extract_space_call_commands(code: &str) -> Vec<String> {
    const NAMES: &[&str] = &["system", "exec"];
    let mut cmds = Vec::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut prev: Option<char> = None;
    for (idx, c) in code.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            prev = Some(c);
            continue;
        }
        if matches!(c, '\'' | '"' | '`') {
            quote = Some(c);
            prev = Some(c);
            continue;
        }
        let boundary_ok = prev.is_none_or(|p| !is_ident_char(p) && p != '.');
        if boundary_ok {
            let tail = &code[idx..];
            if let Some(name) = NAMES.iter().find(|n| {
                tail.strip_prefix(**n)
                    .is_some_and(|after| after.starts_with([' ', '\t']))
            }) {
                let arg_start = idx + name.len();
                let mut end = code.len();
                let mut inner_q: Option<char> = None;
                let mut inner_esc = false;
                for (off, ch) in code[arg_start..].char_indices() {
                    if let Some(q) = inner_q {
                        if inner_esc {
                            inner_esc = false;
                        } else if ch == '\\' {
                            inner_esc = true;
                        } else if ch == q {
                            inner_q = None;
                        }
                    } else if matches!(ch, '\'' | '"' | '`') {
                        inner_q = Some(ch);
                    } else if matches!(ch, ';' | '\n' | ')' | '}') {
                        end = arg_start + off;
                        break;
                    }
                }
                let lits = extract_literals(&code[arg_start..end]);
                if lits.len() >= 2 {
                    cmds.push(
                        lits.iter()
                            .map(|s| shell::shell_quote(s))
                            .collect::<Vec<_>>()
                            .join(" "),
                    );
                }
            }
        }
        prev = Some(c);
    }
    cmds
}
