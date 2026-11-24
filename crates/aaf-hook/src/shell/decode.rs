//! Literal payload decoders for `echo`, `printf`, `base64 -d`, and `env -S`.

use super::Tok;

const MAX_PRINTF_OUT: usize = 64 * 1024;

/// Decodes POSIX backslash escapes (`\NNN`, `\0NNN`, `\xHH`, `\n`, `\r`, `\t`,
/// `\f`, `\v`, `\a`, `\b`, `\e`, `\\`, `\"`, `\'`). When `octal_zero_prefix` is
/// true (`%b` and `echo`), `\0NNN` skips the leading `0` before up to 3 octal
/// digits and `\c` terminates output early (returning `stopped = true`).
pub(super) fn decode_escapes_stop(s: &str, octal_zero_prefix: bool) -> (String, bool) {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        let Some(&next) = bytes.get(i + 1) else {
            out.push(b'\\');
            break;
        };
        match next {
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'f' => out.push(0x0C),
            b'v' => out.push(0x0B),
            b'a' => out.push(0x07),
            b'b' => out.push(0x08),
            b'e' | b'E' => out.push(0x1B),
            b'\\' => out.push(b'\\'),
            b'"' => out.push(b'"'),
            b'\'' => out.push(b'\''),
            b'c' if octal_zero_prefix => {
                return (String::from_utf8_lossy(&out).into_owned(), true);
            }
            b'x' => {
                let mut val: u8 = 0;
                let mut n = 0;
                while n < 2
                    && let Some(d) = bytes.get(i + 2 + n).and_then(|&b| match b {
                        b'0'..=b'9' => Some(b - b'0'),
                        b'a'..=b'f' => Some(b - b'a' + 10),
                        b'A'..=b'F' => Some(b - b'A' + 10),
                        _ => None,
                    })
                {
                    val = val.wrapping_mul(16).wrapping_add(d);
                    n += 1;
                }
                if n > 0 {
                    out.push(val);
                    i += 2 + n;
                    continue;
                }
                out.push(b'\\');
                out.push(b'x');
            }
            b'u' | b'U' => {
                let max = if next == b'u' { 4 } else { 8 };
                let mut val: u32 = 0;
                let mut n = 0;
                while n < max
                    && let Some(d) = bytes.get(i + 2 + n).and_then(|&b| match b {
                        b'0'..=b'9' => Some(u32::from(b - b'0')),
                        b'a'..=b'f' => Some(u32::from(b - b'a' + 10)),
                        b'A'..=b'F' => Some(u32::from(b - b'A' + 10)),
                        _ => None,
                    })
                {
                    val = val.saturating_mul(16).saturating_add(d);
                    n += 1;
                }
                if n > 0
                    && let Some(ch) = char::from_u32(val)
                {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    i += 2 + n;
                    continue;
                }
                out.push(b'\\');
                out.push(next);
            }
            b'0'..=b'7' => {
                let mut j = i + 1;
                if octal_zero_prefix && bytes[j] == b'0' {
                    j += 1;
                }
                let mut val: u32 = 0;
                let mut n = 0;
                while n < 3
                    && let Some(&b) = bytes.get(j)
                    && (b'0'..=b'7').contains(&b)
                {
                    val = val * 8 + u32::from(b - b'0');
                    j += 1;
                    n += 1;
                }
                out.push((val & 0xFF) as u8);
                i = j;
                continue;
            }
            other => {
                out.push(b'\\');
                out.push(other);
            }
        }
        i += 2;
    }
    (String::from_utf8_lossy(&out).into_owned(), false)
}

pub(super) fn decode_escapes(s: &str, octal_zero_prefix: bool) -> String {
    decode_escapes_stop(s, octal_zero_prefix).0
}

fn push_with_precision(out: &mut String, s: &str, precision: Option<usize>) {
    match precision {
        Some(p) if p < s.len() => {
            out.push_str(&String::from_utf8_lossy(&s.as_bytes()[..p]));
        }
        _ => out.push_str(s),
    }
}

pub(super) fn expand_printf(fmt: &str, args: &[String]) -> Option<String> {
    let bytes = fmt.as_bytes();
    let mut out = String::with_capacity(fmt.len());
    let mut arg_idx = 0;
    let mut expanded = false;
    loop {
        let pass_start = arg_idx;
        let mut i = 0;
        while i < bytes.len() && out.len() < MAX_PRINTF_OUT {
            if bytes[i] != b'%' {
                let start = i;
                while i < bytes.len() && bytes[i] != b'%' {
                    i += 1;
                }
                out.push_str(&decode_escapes(&fmt[start..i], false));
                continue;
            }
            if bytes.get(i + 1) == Some(&b'%') {
                out.push('%');
                i += 2;
                continue;
            }
            let start = i;
            i += 1;
            while i < bytes.len() && matches!(bytes[i], b'-' | b'+' | b' ' | b'#' | b'0' | b'\'') {
                i += 1;
            }
            if bytes.get(i) == Some(&b'*') {
                i += 1;
                if arg_idx < args.len() {
                    arg_idx += 1;
                }
            } else {
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            let mut precision: Option<usize> = None;
            if bytes.get(i) == Some(&b'.') {
                i += 1;
                if bytes.get(i) == Some(&b'*') {
                    i += 1;
                    if let Some(arg) = args.get(arg_idx) {
                        arg_idx += 1;
                        let trimmed = arg.trim();
                        if !trimmed.starts_with('-')
                            && let Ok(p) = trimmed.parse::<usize>()
                        {
                            precision = Some(p);
                        }
                    } else {
                        precision = Some(0);
                    }
                } else {
                    let p_start = i;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                    precision = if i == p_start {
                        Some(0)
                    } else {
                        fmt[p_start..i].parse::<usize>().ok()
                    };
                }
            }
            match bytes.get(i).copied() {
                Some(b's' | b'q') => {
                    i += 1;
                    expanded = true;
                    if let Some(arg) = args.get(arg_idx) {
                        arg_idx += 1;
                        push_with_precision(&mut out, arg, precision);
                    }
                }
                Some(b'b') => {
                    i += 1;
                    expanded = true;
                    if let Some(arg) = args.get(arg_idx) {
                        arg_idx += 1;
                        let (decoded, stopped) = decode_escapes_stop(arg, true);
                        push_with_precision(&mut out, &decoded, precision);
                        if stopped {
                            return Some(out);
                        }
                    }
                }
                Some(b'c') => {
                    i += 1;
                    expanded = true;
                    if let Some(arg) = args.get(arg_idx) {
                        arg_idx += 1;
                        if let Some(ch) = arg.chars().next() {
                            out.push(ch);
                        }
                    }
                }
                Some(
                    b'd' | b'i' | b'u' | b'o' | b'x' | b'X' | b'f' | b'F' | b'e' | b'E' | b'g'
                    | b'G' | b'a' | b'A',
                ) => {
                    i += 1;
                    if let Some(arg) = args.get(arg_idx) {
                        arg_idx += 1;
                        out.push_str(arg);
                    } else {
                        out.push_str(&fmt[start..i]);
                    }
                }
                _ => out.push_str(&fmt[start..i]),
            }
        }
        if arg_idx >= args.len() || arg_idx == pass_start || out.len() >= MAX_PRINTF_OUT {
            break;
        }
    }
    expanded.then_some(out)
}

pub(super) fn decode_base64(input: &str, ignore_garbage: bool) -> Option<String> {
    let mut vals = Vec::with_capacity(input.len());
    let mut saw_pad = false;
    for &b in input.as_bytes() {
        if b.is_ascii_whitespace() {
            continue;
        }
        if b == b'=' {
            saw_pad = true;
            continue;
        }
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ if ignore_garbage => continue,
            _ => return None,
        };
        if saw_pad {
            return None;
        }
        vals.push(v);
    }
    if vals.len() < 2 || vals.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(vals.len() * 3 / 4);
    for chunk in vals.chunks(4) {
        out.push((chunk[0] << 2) | (chunk[1] >> 4));
        if chunk.len() > 2 {
            out.push((chunk[1] << 4) | (chunk[2] >> 2));
        }
        if chunk.len() > 3 {
            out.push((chunk[2] << 6) | chunk[3]);
        }
    }
    String::from_utf8(out).ok()
}

pub(super) fn base64_is_decode(args: &[String]) -> bool {
    for arg in args.iter().map(String::as_str) {
        if arg == "--" {
            break;
        }
        if arg.starts_with("--d") && "--decode".starts_with(arg) {
            return true;
        }
        if arg.starts_with('-') && !arg.starts_with("--") && arg.len() > 1 {
            let prefix = arg[1..]
                .find(['b', 'o', 'w'])
                .map_or(&arg[1..], |p| &arg[1..1 + p]);
            if prefix.contains(['d', 'D']) {
                return true;
            }
        }
    }
    false
}

pub(crate) fn base64_ignores_garbage(args: &[String]) -> bool {
    for arg in args.iter().map(String::as_str) {
        if arg == "--" {
            break;
        }
        if arg.starts_with("--i") && "--ignore-garbage".starts_with(arg) {
            return true;
        }
        if arg.starts_with('-') && !arg.starts_with("--") && arg.len() > 1 {
            let prefix = arg[1..]
                .find(['b', 'o', 'w'])
                .map_or(&arg[1..], |p| &arg[1..1 + p]);
            if prefix.contains('i') {
                return true;
            }
        }
    }
    false
}

fn env_s_escape(esc: char) -> Option<char> {
    match esc {
        'n' => Some('\n'),
        't' => Some('\t'),
        'r' => Some('\r'),
        'f' => Some('\x0C'),
        'v' => Some('\x0B'),
        ch @ ('#' | '$' | '"' | '\'' | '\\' | ' ' | '\t') => Some(ch),
        _ => None,
    }
}

/// Splits an `env -S` string using GNU/BSD `env -S` rules: whitespace, quotes,
/// `#` comments at start of word, and `env -S` backslash escapes (`\_`, `\c`,
/// `\n`, `\t`, `\r`, `\f`, `\v`, `\#`, `\$`, `\"`, `\'`, `\\`, `\ `) — never
/// shell syntax (`;`, `|`, `&`, `(`, `)`). Returns `None` on an unclosed quote,
/// unknown/trailing escape, or `\c` inside double quotes so the caller marks it
/// opaque.
pub(super) fn split_env_s(script: &str) -> Option<Vec<Tok>> {
    let mut toks = Vec::new();
    let mut cur: Option<Tok> = None;
    let mut chars = script.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\n' | '\r' => {
                if let Some(tok) = cur.take() {
                    toks.push(tok);
                }
            }
            '#' if cur.is_none() => {
                for ch in chars.by_ref() {
                    if ch == '\n' {
                        break;
                    }
                }
            }
            '\'' => {
                let tok = cur.get_or_insert_with(Tok::default);
                tok.quoted = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        '\\' => match chars.next()? {
                            ch @ ('\\' | '\'') => tok.text.push(ch),
                            other => {
                                tok.text.push('\\');
                                tok.text.push(other);
                            }
                        },
                        ch => tok.text.push(ch),
                    }
                }
            }
            '"' => {
                let tok = cur.get_or_insert_with(Tok::default);
                tok.quoted = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            '_' => tok.text.push(' '),
                            'c' => return None,
                            esc => tok.text.push(env_s_escape(esc)?),
                        },
                        ch => {
                            tok.dynamic |= ch == '$';
                            tok.text.push(ch);
                        }
                    }
                }
            }
            '\\' => match chars.next()? {
                '_' => {
                    if let Some(tok) = cur.take() {
                        toks.push(tok);
                    }
                }
                'c' => break,
                esc => {
                    let tok = cur.get_or_insert_with(Tok::default);
                    tok.quoted = true;
                    tok.text.push(env_s_escape(esc)?);
                }
            },
            ch => {
                let tok = cur.get_or_insert_with(Tok::default);
                tok.dynamic |= ch == '$';
                tok.text.push(ch);
            }
        }
    }
    if let Some(tok) = cur {
        toks.push(tok);
    }
    Some(toks)
}
