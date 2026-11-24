//! Detects shell command execution embedded in `awk` (`system(...)`) and `tar`
//! (`--checkpoint-action=exec=...`, `--to-command=...`,
//! `--use-compress-program=...`, `--rsh-command=...`, `-I <cmd>`).

use crate::shell::{self, Cmd};
use crate::{Ctx, Denial, interp_guard};

pub(crate) fn check_cmd(cmd: &Cmd, ctx: &Ctx) -> Option<Denial> {
    if cmd.dynamic_name {
        return None;
    }
    match cmd.name() {
        "awk" | "gawk" | "mawk" | "nawk" => check_awk(cmd.args(), ctx),
        "tar" => check_tar(cmd.args(), ctx),
        _ => None,
    }
}

fn awk_programs(args: &[String]) -> Vec<&str> {
    let mut programs: Vec<&str> = Vec::new();
    let mut positionals: Vec<&str> = Vec::new();
    let mut has_program_file = false;
    let mut has_explicit_prog = false;
    let mut only_pos = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if only_pos || !arg.starts_with('-') || arg == "-" {
            positionals.push(arg);
            i += 1;
            continue;
        }
        if arg == "--" {
            only_pos = true;
            i += 1;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (flag, glued) = long
                .split_once('=')
                .map_or((long, None), |(f, v)| (f, Some(v)));
            match flag {
                "source" => {
                    has_explicit_prog = true;
                    if let Some(val) = glued {
                        programs.push(val);
                        i += 1;
                    } else {
                        if let Some(val) = args.get(i + 1) {
                            programs.push(val.as_str());
                        }
                        i += 2;
                    }
                }
                "file" | "exec" => {
                    has_program_file = true;
                    i += if glued.is_some() { 1 } else { 2 };
                }
                "include" | "assign" => {
                    i += if glued.is_some() { 1 } else { 2 };
                }
                _ => i += 1,
            }
            continue;
        }
        let mut chars = arg.chars();
        chars.next();
        let mut step = 1;
        while let Some(c) = chars.next() {
            let rest = chars.as_str();
            match c {
                'e' => {
                    has_explicit_prog = true;
                    if !rest.is_empty() {
                        programs.push(rest);
                    } else {
                        if let Some(val) = args.get(i + 1) {
                            programs.push(val.as_str());
                        }
                        step = 2;
                    }
                    break;
                }
                'f' | 'E' => {
                    has_program_file = true;
                    if rest.is_empty() {
                        step = 2;
                    }
                    break;
                }
                'F' | 'v' | 'i' => {
                    if rest.is_empty() {
                        step = 2;
                    }
                    break;
                }
                _ => {}
            }
        }
        i += step;
    }
    if !has_program_file
        && !has_explicit_prog
        && let Some(&first) = positionals.first()
    {
        programs.push(first);
    }
    programs
}

/// Parses an awk `system(...)` argument expression when it consists solely of
/// one or more concatenated double-quoted string literals. Returns `None` if
/// the expression references any variable, field (`$0`), or other dynamic term.
fn parse_awk_string_expr(expr: &str) -> Option<String> {
    let chars: Vec<char> = expr.chars().collect();
    let mut out = String::new();
    let mut saw_lit = false;
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        if chars[i] != '"' {
            return None;
        }
        saw_lit = true;
        i += 1;
        let mut closed = false;
        while i < chars.len() {
            let c = chars[i];
            if c == '\\' {
                i += 1;
                let esc = *chars.get(i)?;
                match esc {
                    'x' => {
                        i += 1;
                        let (mut val, mut n) = (0u32, 0usize);
                        while n < 2
                            && let Some(d) = chars.get(i).and_then(|ch| ch.to_digit(16))
                        {
                            val = val * 16 + d;
                            i += 1;
                            n += 1;
                        }
                        out.push(if n > 0 { char::from_u32(val)? } else { 'x' });
                    }
                    '0'..='7' => {
                        let (mut val, mut n) = (0u32, 0usize);
                        while n < 3
                            && let Some(d) = chars.get(i).and_then(|ch| ch.to_digit(8))
                        {
                            val = val * 8 + d;
                            i += 1;
                            n += 1;
                        }
                        out.push(char::from_u32(val & 0xFF)?);
                    }
                    other => {
                        out.push(match other {
                            'n' => '\n',
                            't' => '\t',
                            'r' => '\r',
                            'a' => '\x07',
                            'b' => '\x08',
                            'f' => '\x0C',
                            'v' => '\x0B',
                            ch => ch,
                        });
                        i += 1;
                    }
                }
                continue;
            }
            if c == '"' {
                i += 1;
                closed = true;
                break;
            }
            out.push(c);
            i += 1;
        }
        if !closed {
            return None;
        }
    }
    saw_lit.then_some(out)
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn skip_awk_regex(bytes: &[u8], i: &mut usize) {
    *i += 1;
    while *i < bytes.len() {
        match bytes[*i] {
            b'\\' => *i = (*i + 2).min(bytes.len()),
            b'[' => {
                *i += 1;
                if *i < bytes.len() && bytes[*i] == b'^' {
                    *i += 1;
                }
                if *i < bytes.len() && bytes[*i] == b']' {
                    *i += 1;
                }
                while *i < bytes.len() && bytes[*i] != b']' && bytes[*i] != b'\n' {
                    if bytes[*i] == b'\\' {
                        *i = (*i + 2).min(bytes.len());
                    } else if bytes[*i] == b'['
                        && let Some(&kind @ (b':' | b'.' | b'=')) = bytes.get(*i + 1)
                    {
                        *i += 2;
                        while *i + 1 < bytes.len()
                            && !(bytes[*i] == kind && bytes[*i + 1] == b']')
                            && bytes[*i] != b'\n'
                        {
                            *i += 1;
                        }
                        if *i + 1 < bytes.len() && bytes[*i] == kind && bytes[*i + 1] == b']' {
                            *i += 2;
                        }
                    } else {
                        *i += 1;
                    }
                }
                if *i < bytes.len() && bytes[*i] == b']' {
                    *i += 1;
                }
            }
            b'/' => {
                *i += 1;
                break;
            }
            b'\n' => break,
            _ => *i += 1,
        }
    }
}

fn check_awk_program(prog: &str, ctx: &Ctx) -> Option<Denial> {
    let bytes = prog.as_bytes();
    let mut i = 0;
    let mut regex_ok = true;
    let mut ctrl_kw = false;
    let mut paren_ctrl: Vec<bool> = Vec::new();
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(bytes.len());
                } else if bytes[i] == b'"' {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            regex_ok = false;
            ctrl_kw = false;
            continue;
        }
        if b == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if matches!(b, b' ' | b'\t' | b'\r') {
            i += 1;
            continue;
        }
        if b == b'\n' {
            i += 1;
            regex_ok = true;
            continue;
        }
        if b == b'\\' && bytes.get(i + 1) == Some(&b'\n') {
            i += 2;
            continue;
        }
        if b == b'/' {
            if regex_ok {
                skip_awk_regex(bytes, &mut i);
                regex_ok = false;
            } else {
                i += 1;
                regex_ok = true;
            }
            ctrl_kw = false;
            continue;
        }
        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            let mut end = i + 1;
            while end < bytes.len() && is_ident_byte(bytes[end]) {
                end += 1;
            }
            let ident = &bytes[start..end];
            if ident == b"system" {
                let mut j = end;
                while j < bytes.len() && matches!(bytes[j], b' ' | b'\t') {
                    j += 1;
                }
                if bytes.get(j) == Some(&b'(') {
                    let open_end = j + 1;
                    let mut depth = 1usize;
                    let mut k = open_end;
                    let mut q = false;
                    let mut esc = false;
                    while k < bytes.len() && depth > 0 {
                        let ch = bytes[k];
                        if q {
                            if esc {
                                esc = false;
                            } else if ch == b'\\' {
                                esc = true;
                            } else if ch == b'"' {
                                q = false;
                            }
                        } else if ch == b'"' {
                            q = true;
                        } else if ch == b'(' {
                            depth += 1;
                        } else if ch == b')' {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        k += 1;
                    }
                    if depth != 0 {
                        return Some(Denial::new(
                            interp_guard::RULE,
                            "awk `system()` call could not be parsed safely",
                            "avoid dynamic or malformed system() calls in awk",
                        ));
                    }
                    let inner = &prog[open_end..k];
                    if let Some(cmd_str) = parse_awk_string_expr(inner) {
                        if ctx.depth >= shell::MAX_DEPTH {
                            return Some(Denial::new(
                                interp_guard::RULE,
                                "awk `system()` shell execution is nested too deeply to analyse",
                                "rewrite as plain shell commands",
                            ));
                        }
                        if let Some(denial) = crate::bash(&cmd_str, ctx.nested()) {
                            return Some(denial);
                        }
                    } else {
                        return Some(Denial::new(
                            interp_guard::RULE,
                            "awk `system()` executes a dynamic shell command",
                            "avoid dynamic system() in awk; run explicit shell commands instead",
                        ));
                    }
                    i = k + 1;
                    regex_ok = false;
                    ctrl_kw = false;
                    continue;
                }
            }
            i = end;
            ctrl_kw = matches!(ident, b"if" | b"while" | b"for");
            regex_ok = matches!(
                ident,
                b"return"
                    | b"print"
                    | b"printf"
                    | b"else"
                    | b"do"
                    | b"in"
                    | b"delete"
                    | b"exit"
                    | b"getline"
            );
            continue;
        }
        if b.is_ascii_digit() {
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'.') {
                i += 1;
            }
            regex_ok = false;
            ctrl_kw = false;
            continue;
        }
        if (b == b'+' && bytes.get(i + 1) == Some(&b'+'))
            || (b == b'-' && bytes.get(i + 1) == Some(&b'-'))
        {
            ctrl_kw = false;
            regex_ok = false;
            i += 2;
            continue;
        }
        if b == b'(' {
            paren_ctrl.push(ctrl_kw);
            ctrl_kw = false;
            regex_ok = true;
        } else if b == b')' {
            ctrl_kw = false;
            regex_ok = paren_ctrl.pop().unwrap_or(false);
        } else if b == b']' {
            ctrl_kw = false;
            regex_ok = false;
        } else if b"~{};,?:&|!=<>+-*%^[".contains(&b) {
            ctrl_kw = false;
            regex_ok = true;
        } else {
            ctrl_kw = false;
        }
        i += 1;
    }
    None
}

fn check_awk(args: &[String], ctx: &Ctx) -> Option<Denial> {
    for prog in awk_programs(args) {
        if let Some(denial) = check_awk_program(prog, ctx) {
            return Some(denial);
        }
    }
    None
}

fn tar_exec_payloads(args: &[String]) -> Vec<&str> {
    let mut payloads = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            break;
        }
        if i == 0 && !arg.starts_with('-') && !arg.is_empty() {
            let mut next_arg = 1;
            for c in arg.chars() {
                if "fCTXbLVFHgKNI".contains(c)
                    && let Some(val) = args.get(next_arg)
                {
                    if c == 'I' {
                        payloads.push(val.as_str());
                    }
                    next_arg += 1;
                }
            }
            i = next_arg;
            continue;
        }
        if crate::git_guard::abbrev(arg, "--checkpoint-action", 13) {
            if let Some((_, rest)) = arg.split_once('=') {
                if let Some(cmd) = rest.strip_prefix("exec=") {
                    payloads.push(cmd);
                }
                i += 1;
            } else {
                if let Some(next) = args.get(i + 1)
                    && let Some(cmd) = next.strip_prefix("exec=")
                {
                    payloads.push(cmd);
                }
                i += 2;
            }
            continue;
        }
        if crate::git_guard::abbrev(arg, "--to-command", 6)
            || crate::git_guard::abbrev(arg, "--use-compress-program", 4)
            || crate::git_guard::abbrev(arg, "--rsh-command", 4)
        {
            if let Some((_, cmd)) = arg.split_once('=') {
                payloads.push(cmd);
                i += 1;
            } else {
                if let Some(next) = args.get(i + 1) {
                    payloads.push(next.as_str());
                }
                i += 2;
            }
            continue;
        }
        if arg.starts_with('-') && !arg.starts_with("--") && arg.len() > 1 {
            let mut chars = arg.chars();
            chars.next();
            let mut step = 1;
            while let Some(c) = chars.next() {
                let rest = chars.as_str();
                if c == 'I' {
                    if !rest.is_empty() {
                        payloads.push(rest);
                    } else {
                        if let Some(next) = args.get(i + 1) {
                            payloads.push(next.as_str());
                        }
                        step = 2;
                    }
                    break;
                }
                if matches!(
                    c,
                    'f' | 'C' | 'T' | 'X' | 'b' | 'L' | 'V' | 'F' | 'H' | 'g' | 'K' | 'N'
                ) {
                    if rest.is_empty() {
                        step = 2;
                    }
                    break;
                }
            }
            i += step;
            continue;
        }
        i += 1;
    }
    payloads
}

fn check_tar(args: &[String], ctx: &Ctx) -> Option<Denial> {
    for payload in tar_exec_payloads(args) {
        if payload.trim().is_empty() {
            continue;
        }
        if ctx.depth >= shell::MAX_DEPTH {
            return Some(Denial::new(
                interp_guard::RULE,
                "tar shell execution option is nested too deeply to analyse",
                "rewrite as plain shell commands",
            ));
        }
        if let Some(denial) = crate::bash(payload, ctx.nested()) {
            return Some(denial);
        }
    }
    None
}
