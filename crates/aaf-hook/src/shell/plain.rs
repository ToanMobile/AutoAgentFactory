//! Which commands run in this shell whatever came before them
//! (`Script::plain_steps`): the `if`/`case`/loop bodies, `{ … }` groups,
//! `coproc` and `time` a command opens or closes.

use super::{Lexer, RESERVED, Tok};

impl Lexer<'_, '_> {
    /// Ends the command at a list separator other than `|`/`||`.
    pub(super) fn end_step(&mut self) -> Option<usize> {
        let stored = self.end_cmd();
        self.after_or = false;
        self.after_and = false;
        self.piped = false;
        stored
    }

    /// Follows the `if`/`case`/loop bodies a command opens or closes by
    /// the words it starts with; whether it runs in this shell whatever
    /// came before it (a pipe or `&` after it is seen later).
    /// A `{ … }` group takes this from where it opens: each command in it
    /// is plain only when the group is.
    pub(super) fn plain_step(&mut self, toks: &[Tok]) -> bool {
        self.closed_group = None;
        // `coproc` runs what it starts in the background.
        let mut coproc = false;
        let mut iter = toks.iter().take_while(|tok| !tok.quoted).peekable();
        while let Some(tok) = iter.next() {
            match tok.text.as_str() {
                "if" | "case" | "while" | "until" | "for" | "select" => self.compound += 1,
                "fi" | "esac" | "done" => self.compound = self.compound.saturating_sub(1),
                "{" => {
                    let plain = !coproc && self.plain_here();
                    self.groups.push((plain, self.out.cmds.len()));
                }
                "}" => self.closed_group = self.groups.pop().map(|(_, start)| start),
                "time" => {
                    while iter
                        .peek()
                        .is_some_and(|tok| tok.text.len() > 1 && tok.text.starts_with('-'))
                    {
                        iter.next();
                    }
                }
                "coproc" => {
                    coproc = true;
                    // `coproc NAME { … }`; a NAME is a word, never `{`.
                    let name = |tok: &&Tok| {
                        tok.text
                            .starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                            && tok
                                .text
                                .chars()
                                .all(|c| c.is_ascii_alphanumeric() || c == '_')
                    };
                    if iter.peek().is_some_and(name)
                        && iter.clone().nth(1).is_some_and(|tok| tok.text == "{")
                    {
                        iter.next();
                    }
                }
                word if RESERVED.contains(&word) => {}
                _ => break,
            }
        }
        !coproc && self.plain_here()
    }

    fn plain_here(&self) -> bool {
        !self.after_or
            && !self.after_and
            && !self.piped
            && self.compound == 0
            && self.groups.iter().all(|(plain, _)| *plain)
    }

    /// A `{ … }` group just closed is piped or put in the background:
    /// none of its commands runs in this shell.
    pub(super) fn unplain_group(&mut self) {
        if let Some(start) = self.closed_group.take() {
            self.out.plain_steps.retain(|idx| *idx < start);
        }
    }
}
