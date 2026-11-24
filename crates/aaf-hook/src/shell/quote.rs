//! Quoting: `'…'`, `"…"`, `$…`/`$(…)`, backticks and `$'…'`, read into the
//! word being split.

use super::Lexer;
use super::scan::{decode_ansi_c, find_close};

impl Lexer<'_, '_> {
    pub(super) fn single(&mut self) {
        self.word_mut().quoted = true;
        loop {
            match self.take() {
                None => return self.fail(),
                Some('\'') => return,
                Some(c) => self.word_mut().text.push(c),
            }
        }
    }

    pub(super) fn double(&mut self) {
        self.word_mut().quoted = true;
        while !self.failed {
            match self.peek() {
                None => return self.fail(),
                Some('"') => {
                    self.pos += 1;
                    return;
                }
                Some('\\') => {
                    self.pos += 1;
                    match self.take() {
                        None => return self.fail(),
                        Some('\n') => {}
                        Some(c @ ('"' | '\\' | '$' | '`')) => self.word_mut().text.push(c),
                        Some(c) => {
                            let word = self.word_mut();
                            word.text.push('\\');
                            word.text.push(c);
                        }
                    }
                }
                Some('$') => self.dollar(true),
                Some('`') => self.backtick(),
                Some(c) => {
                    self.pos += 1;
                    self.word_mut().text.push(c);
                }
            }
        }
    }

    pub(super) fn dollar(&mut self, in_double: bool) {
        self.pos += 1;
        match self.peek() {
            Some('(') => {
                let start = self.pos + 1;
                match find_close(&self.chars, start, 0) {
                    Some(end) => {
                        let inner: String = self.chars[start..end].iter().collect();
                        self.pos = end + 1;
                        self.substitute(&inner);
                    }
                    None => self.fail(),
                }
            }
            Some('\'') if !in_double => {
                self.pos += 1;
                self.ansi_c();
            }
            // `<<$"EOF"`: bash ends the body at `EOF` (the text of a
            // locale-translated string, quoted), not at `$EOF`.
            Some('"') if !in_double && matches!(self.pending.as_deref(), Some("<<" | "<<-")) => {}
            next => {
                // `$VAR`, `${VAR}`: the value is unknown, keep the text.
                let word = self.word_mut();
                word.dynamic = true;
                word.text.push('$');
                if next == Some('{') && !in_double {
                    self.pos += 1;
                    self.word_mut().text.push('{');
                    self.param_braces += 1;
                }
            }
        }
    }

    pub(super) fn backtick(&mut self) {
        self.pos += 1;
        let mut inner = String::new();
        loop {
            match self.take() {
                None => return self.fail(),
                Some('`') => break,
                Some('\\') => match self.take() {
                    None => return self.fail(),
                    Some(c @ ('`' | '\\' | '$')) => inner.push(c),
                    Some(c) => {
                        inner.push('\\');
                        inner.push(c);
                    }
                },
                Some(c) => inner.push(c),
            }
        }
        self.substitute(&inner);
    }

    /// `$'…'`: decoded so `$'\x67it'` is seen as `git`.
    pub(super) fn ansi_c(&mut self) {
        self.word_mut().quoted = true;
        match decode_ansi_c(&self.chars, &mut self.pos) {
            Some((text, dynamic)) => {
                let word = self.word_mut();
                word.text.push_str(&text);
                word.dynamic |= dynamic;
            }
            None => self.fail(),
        }
    }
}
