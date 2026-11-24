//! Minimal Rust lexer for partitioning inline `#[cfg(test)]` items from
//! production code without splitting inside comments, normal strings, or raw strings.

#[derive(Clone, Copy)]
struct RustLexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    i: usize,
    line: usize,
}

impl<'a> RustLexer<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            i: 0,
            line: 1,
        }
    }
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.i).copied()
    }
    fn peek2(&self) -> Option<u8> {
        self.bytes.get(self.i + 1).copied()
    }
    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.i += 1;
        if b == b'\n' {
            self.line += 1;
        }
        Some(b)
    }

    fn skip_comment(&mut self) -> bool {
        match (self.peek(), self.peek2()) {
            (Some(b'/'), Some(b'/')) => {
                self.i += 2;
                while self.peek().is_some_and(|b| b != b'\n') {
                    self.i += 1;
                }
                true
            }
            (Some(b'/'), Some(b'*')) => {
                self.i += 2;
                let mut depth = 1usize;
                while depth > 0 && self.i < self.bytes.len() {
                    match (self.peek(), self.peek2()) {
                        (Some(b'/'), Some(b'*')) => {
                            self.i += 2;
                            depth += 1;
                        }
                        (Some(b'*'), Some(b'/')) => {
                            self.i += 2;
                            depth -= 1;
                        }
                        _ => {
                            let _ = self.bump();
                        }
                    }
                }
                true
            }
            _ => false,
        }
    }

    fn try_skip_raw_string(&mut self) -> bool {
        if self.i > 0 && is_ident_continue(self.bytes[self.i - 1]) {
            return false;
        }
        let r_pos = match (self.peek(), self.peek2()) {
            (Some(b'r'), _) => self.i,
            (Some(b'b' | b'c'), Some(b'r')) => self.i + 1,
            _ => return false,
        };
        let mut j = r_pos + 1;
        while self.bytes.get(j) == Some(&b'#') {
            j += 1;
        }
        if self.bytes.get(j) != Some(&b'"') {
            return false;
        }
        let hashes = j - (r_pos + 1);
        self.i = j + 1;
        while self.i < self.bytes.len() {
            if self.bump() == Some(b'"')
                && (0..hashes).all(|k| self.bytes.get(self.i + k) == Some(&b'#'))
            {
                self.i += hashes;
                return true;
            }
        }
        true
    }

    fn skip_normal_string(&mut self) {
        self.i += 1;
        while let Some(b) = self.bump() {
            if b == b'\\' {
                let _ = self.bump();
            } else if b == b'"' {
                break;
            }
        }
    }

    fn try_skip_char_literal(&mut self) -> bool {
        let rest = &self.src[self.i + 1..];
        if rest.starts_with('\\') {
            self.i += 2;
            while let Some(b) = self.peek().filter(|&b| b != b'\n') {
                self.i += 1;
                if b == b'\'' {
                    break;
                }
            }
            return true;
        }
        if let Some(ch) = rest.chars().next()
            && !matches!(ch, '\n' | '\'')
            && rest.as_bytes().get(ch.len_utf8()) == Some(&b'\'')
        {
            self.i += 2 + ch.len_utf8();
            return true;
        }
        false
    }

    fn skip_trivia_or_literal(&mut self) -> bool {
        if self.skip_comment() {
            return true;
        }
        match self.peek() {
            Some(b'r' | b'b' | b'c') => self.try_skip_raw_string(),
            Some(b'"') => {
                self.skip_normal_string();
                true
            }
            Some(b'\'') => self.try_skip_char_literal(),
            _ => false,
        }
    }

    fn skip_ws_and_comments(&mut self) {
        while self.i < self.bytes.len() {
            if self.peek().is_some_and(|b| b.is_ascii_whitespace()) {
                let _ = self.bump();
            } else if !self.skip_comment() {
                break;
            }
        }
    }

    fn read_ident(&mut self) -> Option<&'a str> {
        let b = self.peek()?;
        if !(b.is_ascii_alphabetic() || b == b'_') {
            return None;
        }
        let start = self.i;
        self.i += 1;
        while self.peek().is_some_and(is_ident_continue) {
            self.i += 1;
        }
        Some(&self.src[start..self.i])
    }

    fn skip_balanced(&mut self, open: u8, close: u8) -> bool {
        let mut depth = 1usize;
        while depth > 0 && self.i < self.bytes.len() {
            if self.skip_trivia_or_literal() {
                continue;
            }
            match self.bump() {
                Some(b) if b == open => depth += 1,
                Some(b) if b == close => depth -= 1,
                _ => {}
            }
        }
        depth == 0
    }

    fn read_attribute(&mut self) -> Option<(bool, &'a str)> {
        let save = *self;
        self.i += 1;
        self.skip_ws_and_comments();
        let is_inner = self.peek() == Some(b'!');
        if is_inner {
            self.i += 1;
            self.skip_ws_and_comments();
        }
        if self.peek() != Some(b'[') {
            *self = save;
            return None;
        }
        self.i += 1;
        let body_start = self.i;
        let closed = self.skip_balanced(b'[', b']');
        let body_end = if closed { self.i - 1 } else { self.i };
        Some((is_inner, &self.src[body_start..body_end]))
    }
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn line_aligned_start(src: &str, pos: usize) -> usize {
    let bytes = src.as_bytes();
    let start = bytes[..pos]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |i| i + 1);
    if bytes[start..pos].iter().all(|b| matches!(b, b' ' | b'\t')) {
        start
    } else {
        pos
    }
}

/// True when an attribute body (between `[` and `]`) is `cfg(test)` or
/// `cfg(all(..., test, ...))` with `test` as a top-level argument of `all(...)`.
fn is_cfg_test_attr_body(body: &str) -> bool {
    let compact: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    if compact == "cfg(test)" {
        return true;
    }
    let Some(inner) = compact
        .strip_prefix("cfg(all(")
        .and_then(|s| s.strip_suffix("))"))
    else {
        return false;
    };
    let (mut depth, mut start) = (0usize, 0usize);
    for (idx, ch) in inner.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                if &inner[start..idx] == "test" {
                    return true;
                }
                start = idx + 1;
            }
            _ => {}
        }
    }
    &inner[start..] == "test"
}

fn item_is_semicolon_terminated(mut lex: RustLexer<'_>) -> bool {
    loop {
        lex.skip_ws_and_comments();
        match lex.read_ident() {
            Some("pub") => {
                lex.skip_ws_and_comments();
                if lex.peek() == Some(b'(') {
                    lex.i += 1;
                    let _ = lex.skip_balanced(b'(', b')');
                }
            }
            Some("default" | "unsafe" | "async") => {}
            Some("extern") => {
                lex.skip_ws_and_comments();
                if lex.peek() == Some(b'"') {
                    lex.skip_normal_string();
                }
            }
            Some("use" | "static" | "type") => return true,
            Some("const") => {
                lex.skip_ws_and_comments();
                return !matches!(lex.read_ident(), Some("fn" | "unsafe" | "extern"));
            }
            _ => return false,
        }
    }
}

fn consume_cfg_test_item(lex: &mut RustLexer<'_>) -> usize {
    loop {
        lex.skip_ws_and_comments();
        if lex.peek() == Some(b'#') && lex.read_attribute().is_some() {
            continue;
        }
        break;
    }
    let semi_term = item_is_semicolon_terminated(*lex);
    let (mut depth, mut saw_brace) = (0usize, false);
    while lex.i < lex.bytes.len() {
        if lex.skip_trivia_or_literal() {
            continue;
        }
        match lex.bump() {
            Some(b'{') => {
                depth += 1;
                saw_brace = true;
            }
            Some(b'}') if depth > 0 => {
                depth -= 1;
                if depth == 0 && !semi_term {
                    break;
                }
            }
            Some(b';') if depth == 0 && (semi_term || !saw_brace) => break,
            _ => {}
        }
    }
    let end_line = lex.line;
    let mut probe = *lex;
    while matches!(probe.peek(), Some(b' ' | b'\t')) {
        probe.i += 1;
    }
    if probe.peek() == Some(b'\r') && probe.peek2() == Some(b'\n') {
        probe.i += 2;
        probe.line += 1;
        *lex = probe;
    } else if probe.peek() == Some(b'\n') {
        probe.i += 1;
        probe.line += 1;
        *lex = probe;
    }
    end_line
}

struct CfgTestSpan(usize, usize, usize, usize);

/// Returns `(byte_start, byte_end, start_line, end_line)` spans for top-level
/// `#[cfg(test)]` / `#[cfg(all(test, ...))]` items (and whole-file `#![cfg(test)]`).
fn find_cfg_test_spans(src: &str) -> Vec<CfgTestSpan> {
    let mut lex = RustLexer::new(src);
    let mut spans = Vec::new();
    let mut brace_depth = 0usize;
    let mut pending_attr: Option<(usize, usize)> = None;
    while lex.i < lex.bytes.len() {
        if lex.peek().is_some_and(|b| b.is_ascii_whitespace()) {
            let _ = lex.bump();
            continue;
        }
        if lex.skip_trivia_or_literal() {
            continue;
        }
        if lex.peek() == Some(b'#') {
            let attr_pos = (line_aligned_start(src, lex.i), lex.line);
            if let Some((is_inner, body)) = lex.read_attribute() {
                if is_inner && brace_depth == 0 && is_cfg_test_attr_body(body) {
                    let lines = if src.is_empty() {
                        0
                    } else {
                        src.bytes().filter(|&b| b == b'\n').count()
                            + usize::from(!src.ends_with('\n'))
                    };
                    return vec![CfgTestSpan(0, src.len(), 1, lines)];
                }
                if !is_inner && brace_depth == 0 {
                    if is_cfg_test_attr_body(body) {
                        let (byte_start, start_line) = pending_attr.take().unwrap_or(attr_pos);
                        let end_line = consume_cfg_test_item(&mut lex);
                        spans.push(CfgTestSpan(byte_start, lex.i, start_line, end_line));
                    } else {
                        pending_attr.get_or_insert(attr_pos);
                    }
                }
                continue;
            }
        }
        pending_attr = None;
        match lex.bump() {
            Some(b'{') => brace_depth += 1,
            Some(b'}') => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }
    }
    spans
}

/// Returns 1-based inclusive `(start_line, end_line)` spans for all top-level
/// `#[cfg(test)]`, `#[cfg(all(test, ...))]`, and whole-file `#![cfg(test)]`
/// items in `src`, ignoring comments, normal strings, and raw strings.
pub(crate) fn inline_cfg_test_spans(src: &str) -> Vec<(usize, usize)> {
    find_cfg_test_spans(src)
        .into_iter()
        .map(|CfgTestSpan(_, _, s, e)| (s, e))
        .collect()
}

/// Partitions a Rust source file into `(prod_code, Option<test_code>)` by
/// extracting all top-level `#[cfg(test)]` / `#[cfg(all(test, ...))]` items
/// (and whole-file `#![cfg(test)]`), preserving prod code before, between, and
/// after test blocks and never splitting inside comments or raw strings.
pub(crate) fn split_inline_cfg_test(source: &str) -> (String, Option<String>) {
    let spans = find_cfg_test_spans(source);
    if spans.is_empty() {
        return (source.to_string(), None);
    }
    let (mut prod, mut tests, mut cursor) = (String::with_capacity(source.len()), String::new(), 0);
    for CfgTestSpan(byte_start, byte_end, _, _) in spans {
        prod.push_str(&source[cursor..byte_start]);
        if !tests.is_empty() && !tests.ends_with('\n') {
            tests.push('\n');
        }
        tests.push_str(&source[byte_start..byte_end]);
        cursor = byte_end;
    }
    prod.push_str(&source[cursor..]);
    (prod, Some(tests))
}
