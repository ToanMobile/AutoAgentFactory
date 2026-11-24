//! Shared `sed` flag and script-command inspection used by `git_files`,
//! `delete_guard`, and `secret_guard`.

use crate::git_guard::abbrev;

/// True when a `sed` invocation requests in-place editing (`-i`, `-I`,
/// `--in-place` or its abbreviations, or a short-flag cluster containing `i`/`I`
/// before any `e`/`f`).
pub(crate) fn edits_in_place(args: &[String]) -> bool {
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            break;
        }
        if abbrev(arg, "--in-place", 3) {
            return true;
        }
        if !arg.contains('=') && (abbrev(arg, "--expression", 3) || abbrev(arg, "--file", 3)) {
            i += 2;
            continue;
        }
        if arg.starts_with('-') && !arg.starts_with("--") && arg.len() > 1 {
            let mut chars = arg.chars();
            chars.next();
            while let Some(c) = chars.next() {
                if matches!(c, 'i' | 'I') {
                    return true;
                }
                if matches!(c, 'e' | 'f') {
                    if chars.as_str().is_empty() {
                        i += 1;
                    }
                    break;
                }
            }
        }
        i += 1;
    }
    false
}

fn skip_delimited(chars: &[char], pos: &mut usize, delim: char, brackets: bool) {
    while *pos < chars.len() && chars[*pos] != '\n' {
        let c = chars[*pos];
        if c == '\\' {
            *pos = (*pos + 2).min(chars.len());
            continue;
        }
        if brackets && c == '[' {
            *pos += 1;
            if *pos < chars.len() && chars[*pos] == '^' {
                *pos += 1;
            }
            if *pos < chars.len() && chars[*pos] == ']' {
                *pos += 1;
            }
            while *pos < chars.len() && chars[*pos] != ']' && chars[*pos] != '\n' {
                *pos += 1;
            }
            if *pos < chars.len() && chars[*pos] == ']' {
                *pos += 1;
            }
            continue;
        }
        *pos += 1;
        if c == delim {
            return;
        }
    }
}

fn push_tail(chars: &[char], pos: &mut usize, out: &mut Vec<String>) {
    let start = *pos;
    while *pos < chars.len() && chars[*pos] != '\n' {
        *pos += 1;
    }
    let raw: String = chars[start..*pos].iter().collect();
    let trimmed = raw.trim();
    if !trimmed.is_empty() {
        out.push(trimmed.to_string());
        if let Some(without_brace) = trimmed.strip_suffix('}') {
            let clean = without_brace.trim();
            if !clean.is_empty() {
                out.push(clean.to_string());
            }
        }
    }
}

fn extract_expr_files(
    expr: &str,
    cmds: &[char],
    out: &mut Vec<String>,
    mut bare_exec: Option<&mut bool>,
) {
    let chars: Vec<char> = expr.chars().collect();
    let mut pos = 0;
    while pos < chars.len() {
        while pos < chars.len() && matches!(chars[pos], ' ' | '\t' | '\r' | '\n' | ';' | '{' | '}')
        {
            pos += 1;
        }
        if pos >= chars.len() {
            break;
        }
        if chars[pos] == '#' {
            while pos < chars.len() && chars[pos] != '\n' {
                pos += 1;
            }
            continue;
        }
        // Skip optional address range(s).
        loop {
            while pos < chars.len() && matches!(chars[pos], ' ' | '\t') {
                pos += 1;
            }
            if pos >= chars.len() {
                break;
            }
            match chars[pos] {
                '0'..='9' | '$' | ',' | '~' | '+' | '!' => pos += 1,
                '/' => {
                    pos += 1;
                    skip_delimited(&chars, &mut pos, '/', true);
                }
                '\\' if pos + 1 < chars.len() && chars[pos + 1] != '\n' => {
                    let delim = chars[pos + 1];
                    pos += 2;
                    skip_delimited(&chars, &mut pos, delim, true);
                }
                _ => break,
            }
        }
        while pos < chars.len() && matches!(chars[pos], ' ' | '\t' | '{') {
            pos += 1;
        }
        if pos >= chars.len() {
            break;
        }
        let cmd = chars[pos];
        pos += 1;
        if cmd == 'e' {
            while pos < chars.len() && matches!(chars[pos], ' ' | '\t') {
                pos += 1;
            }
            if pos >= chars.len() || matches!(chars[pos], '\r' | '\n' | ';' | '}') {
                if let Some(flag) = bare_exec.as_deref_mut() {
                    *flag = true;
                }
            } else {
                let start = pos;
                while pos < chars.len() && chars[pos] != '\n' {
                    pos += 1;
                }
                if cmds.contains(&'e') {
                    let raw: String = chars[start..pos].iter().collect();
                    let trimmed = raw.trim();
                    if !trimmed.is_empty() {
                        out.push(trimmed.to_string());
                        if let Some(without_brace) = trimmed.strip_suffix('}') {
                            let clean = without_brace.trim();
                            if !clean.is_empty() {
                                out.push(clean.to_string());
                            }
                        }
                    }
                }
            }
        } else if matches!(cmd, 'r' | 'R' | 'w' | 'W') {
            if cmds.contains(&cmd) {
                push_tail(&chars, &mut pos, out);
            } else {
                while pos < chars.len() && chars[pos] != '\n' {
                    pos += 1;
                }
            }
        } else if cmd == 's' && pos < chars.len() && chars[pos] != '\n' {
            let delim = chars[pos];
            pos += 1;
            skip_delimited(&chars, &mut pos, delim, true);
            let repl_start = pos;
            skip_delimited(&chars, &mut pos, delim, false);
            let repl_end = if pos > repl_start && chars[pos - 1] == delim {
                pos - 1
            } else {
                pos
            };
            let mut has_exec_flag = false;
            while pos < chars.len() && !matches!(chars[pos], '\n' | ';' | '}') {
                let flag = chars[pos];
                pos += 1;
                if flag == 'e' {
                    has_exec_flag = true;
                } else if matches!(flag, 'w' | 'W') {
                    if cmds.contains(&flag) {
                        push_tail(&chars, &mut pos, out);
                    } else {
                        while pos < chars.len() && chars[pos] != '\n' {
                            pos += 1;
                        }
                    }
                    break;
                }
            }
            if has_exec_flag {
                if let Some(flag) = bare_exec.as_deref_mut() {
                    *flag = true;
                }
                if cmds.contains(&'e') {
                    let mut repl = String::new();
                    let mut j = repl_start;
                    while j < repl_end {
                        if chars[j] == '\\' && j + 1 < repl_end && chars[j + 1] == delim {
                            repl.push(delim);
                            j += 2;
                        } else {
                            repl.push(chars[j]);
                            j += 1;
                        }
                    }
                    let trimmed = repl.trim();
                    if !trimmed.is_empty() {
                        out.push(trimmed.to_string());
                    }
                }
            }
        } else if cmd == 'y' && pos < chars.len() && chars[pos] != '\n' {
            let delim = chars[pos];
            pos += 1;
            skip_delimited(&chars, &mut pos, delim, false);
            skip_delimited(&chars, &mut pos, delim, false);
        } else if matches!(cmd, 'a' | 'i' | 'c') {
            loop {
                while pos < chars.len() && chars[pos] != '\n' {
                    pos += 1;
                }
                let cont = pos > 0 && chars[pos - 1] == '\\';
                if pos < chars.len() {
                    pos += 1;
                }
                if !cont || pos >= chars.len() {
                    break;
                }
            }
        } else if matches!(cmd, ':' | 'b' | 't' | 'T') {
            while pos < chars.len() && !matches!(chars[pos], '\n' | ';' | '}') {
                pos += 1;
            }
        }
    }
}

/// Collects `(expressions, file_operands)` for a `sed` invocation.
pub(crate) fn parse_args(args: &[String]) -> (Vec<&str>, Vec<&str>) {
    let mut exprs: Vec<&str> = Vec::new();
    let mut positionals: Vec<&str> = Vec::new();
    let mut has_explicit = false;
    let mut bare_inplace = false;
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
        if abbrev(arg, "--expression", 3) {
            has_explicit = true;
            if let Some((_, val)) = arg.split_once('=') {
                exprs.push(val);
                i += 1;
            } else {
                if let Some(val) = args.get(i + 1) {
                    exprs.push(val.as_str());
                }
                i += 2;
            }
            continue;
        }
        if abbrev(arg, "--file", 3) {
            has_explicit = true;
            i += if arg.contains('=') { 1 } else { 2 };
            continue;
        }
        if arg.starts_with("--") {
            i += 1;
            continue;
        }
        if arg.starts_with('-') && arg.len() > 1 {
            let mut chars = arg.chars();
            chars.next();
            let mut step = 1;
            while let Some(c) = chars.next() {
                let rest = chars.as_str();
                if c == 'e' {
                    has_explicit = true;
                    if !rest.is_empty() {
                        exprs.push(rest);
                    } else {
                        if let Some(val) = args.get(i + 1) {
                            exprs.push(val.as_str());
                        }
                        step = 2;
                    }
                    break;
                }
                if c == 'f' {
                    has_explicit = true;
                    if rest.is_empty() {
                        step = 2;
                    }
                    break;
                }
                if matches!(c, 'i' | 'I') {
                    if rest.is_empty() {
                        bare_inplace = true;
                    }
                    break;
                }
            }
            i += step;
            continue;
        }
        positionals.push(arg);
        i += 1;
    }
    let files = if has_explicit {
        positionals
    } else {
        if let Some(&first) = positionals.first() {
            if !first.is_empty() {
                exprs.push(first);
            }
            if bare_inplace
                && (positionals.len() >= 3 || positionals[0].is_empty())
                && let Some(&second) = positionals.get(1)
            {
                exprs.push(second);
            }
        }
        let skip = if bare_inplace && positionals.first() == Some(&"") {
            2
        } else {
            1
        };
        positionals.get(skip..).unwrap_or(&[]).to_vec()
    };
    (exprs, files)
}

/// Extracts file paths named by `sed` script commands in `cmds` (e.g. `w`/`W`
/// for writes or `r`/`R` for reads) across `-e`/`--expression` and inline scripts.
pub(crate) fn script_files(args: &[String], cmds: &[char]) -> Vec<String> {
    let (exprs, _) = parse_args(args);
    let mut out = Vec::new();
    for expr in exprs {
        extract_expr_files(expr, cmds, &mut out, None);
    }
    out
}

/// Extracts GNU `sed` shell commands from `e <cmd>` and `s/.../<cmd>/e` expressions.
pub(crate) fn exec_commands(args: &[String]) -> Vec<String> {
    script_files(args, &['e'])
}

/// True when any GNU `sed` expression invokes bare `e` or `s/.../.../e`,
/// which executes the current pattern space as a shell command.
pub(crate) fn has_bare_exec(args: &[String]) -> bool {
    let (exprs, _) = parse_args(args);
    let mut bare = false;
    let mut sink = Vec::new();
    for expr in exprs {
        extract_expr_files(expr, &[], &mut sink, Some(&mut bare));
        if bare {
            return true;
        }
    }
    false
}
