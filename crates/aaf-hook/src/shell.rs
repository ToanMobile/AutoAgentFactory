//! Shell splitter shared by every guard.
//!
//! It is not a shell: it only finds the simple commands a line would run so
//! the guards can look at the real argv instead of raw text. Anything it
//! cannot split is reported in [`Script::opaque`] and judged conservatively.

mod command;
mod decode;
mod scan;
mod stdin;

pub use command::{DECLARERS, NAME_SETTERS};
use scan::{decode_ansi_c, find_close};
pub(crate) use stdin::{shell_quote, stdin_scripts};

/// Nesting of `$(…)`, backticks, `sh -c` and `eval` that is still analysed.
pub const MAX_DEPTH: usize = 8;
/// Bound on recursion while skipping quotes; deeper input is unparsable, not
/// a stack overflow (the hook may only exit 0 or 2).
const MAX_QUOTE_NESTING: usize = 64;

const RESERVED: &[&str] = &[
    "if", "then", "else", "elif", "fi", "do", "done", "while", "until", "{", "}", "!",
];
pub const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh"];
/// Commands that treat an argument as plain data, so the body of
/// `$(cat <<'EOF' … EOF)` passed to them cannot run.
const DATA_CONSUMERS: &[&str] = &["git", "gh"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeEvent {
    Enter(Vec<String>),
    Exit,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Redirect {
    pub op: String,
    pub target: String,
}

/// One simple command with quotes removed and wrappers (`env`, `command`,
/// `sudo`, …) stripped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cmd {
    pub assigns: Vec<(String, String)>,
    pub words: Vec<String>,
    /// The command word contains an expansion or glob: it could be anything.
    pub dynamic_name: bool,
    /// Per-word dynamic flag parallel to `words` (`true` on `$VAR`, `$(…)`, backtick, or unquoted glob).
    pub arg_dynamic: Vec<bool>,
    pub redirects: Vec<Redirect>,
    /// Raw heredoc bodies attached to this command (e.g. `python3 - <<'EOF'`).
    pub heredocs: Vec<String>,
    /// The wrapper words stripped from the front (`env -C dir`, `xargs -a
    /// file`): no longer the command, but still paths the line names.
    pub wrapped: Vec<String>,
    /// Run through `xargs`: its input supplies further arguments.
    pub xargs: bool,
    /// The simple command whose output is piped into this one. `None` also
    /// when the pipe comes from something that is not one simple command.
    pub feeder: Option<Box<Cmd>>,
    /// Run by `aaf slot -- …`, directly or through a shell it starts: the
    /// command holds a slot of the machine queue.
    pub slotted: bool,
    /// The line rebinds the word `aaf` (a function, an alias, `hash`, the
    /// search path): a bare `aaf` on it is some other program.
    pub aaf_untrusted: bool,
    /// Started under a wrapper (`env -i`, `env -`, `exec -c`, `sudo`, `doas`)
    /// that clears or resets the environment, directly or through a nested
    /// shell/eval/exec.
    pub env_cleared: bool,
}

impl Cmd {
    /// Command name without its directory (`/usr/bin/git` is `git`).
    pub fn name(&self) -> &str {
        self.words.first().map_or("", |w| basename(w))
    }

    pub fn args(&self) -> &[String] {
        self.words.get(1..).unwrap_or(&[])
    }
}

#[derive(Debug, Default)]
pub struct Script {
    pub cmds: Vec<Cmd>,
    /// Subshell scope boundaries `(cmd_index, event)` for `(...)`, `$(...)`,
    /// backticks, and nested shells so `bash()` restores `cwd` on exit.
    pub scopes: Vec<(usize, ScopeEvent)>,
    /// Text that could not be split into commands.
    pub opaque: Vec<String>,
    /// Names of the shell functions the line defines.
    pub functions: Vec<String>,
    /// The commands being split right now are the payload of an `aaf slot`.
    in_slot: bool,
    /// An `aaf slot` was started with a search path of its own.
    rebound: bool,
    /// Commands being split right now run under an environment-clearing wrapper.
    env_cleared: bool,
}

impl Script {
    pub(super) fn enter_scope(&mut self) {
        self.scopes
            .push((self.cmds.len(), ScopeEvent::Enter(Vec::new())));
    }

    pub(super) fn enter_scope_wrapped(&mut self, wrapped: Vec<String>) {
        self.scopes
            .push((self.cmds.len(), ScopeEvent::Enter(wrapped)));
    }

    pub(super) fn exit_scope(&mut self) {
        self.scopes.push((self.cmds.len(), ScopeEvent::Exit));
    }
}

pub fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

pub fn parse(src: &str) -> Script {
    let mut out = Script::default();
    lex(src, 0, &mut out, None);
    out.settle();
    out
}

/// The commands an argv handed to another program (`find -exec …`) runs:
/// wrappers stripped and nested interpreters followed, as for a typed line.
pub fn parse_argv(words: &[String]) -> Script {
    let mut out = Script::default();
    let toks = words
        .iter()
        .map(|word| Tok {
            text: word.clone(),
            // The expansion flag is gone with the lexer's token; a word that
            // is empty or still holds a `$` was not typed literally.
            dynamic: word.is_empty() || word.contains('$'),
            quoted: true,
        })
        .collect();
    Lexer::new("", 0, &mut out, None).emit(Vec::new(), toks, Vec::new(), None);
    out.settle();
    out
}

#[derive(Clone, Default)]
struct Tok {
    text: String,
    dynamic: bool,
    quoted: bool,
}

struct Heredoc {
    delim: String,
    strip_tabs: bool,
    quoted: bool,
    /// The command reading it runs with a cleared environment; `None` until
    /// that command ends.
    cleared: Option<bool>,
    /// Index of the command in `out.cmds` that owns this heredoc.
    cmd_idx: Option<usize>,
    /// Downstream pipeline consumer command indices fed by the owning command, in order.
    pipeline: Vec<usize>,
}

struct HeredocBody {
    body: String,
    quoted: bool,
    cleared: bool,
    is_interp: bool,
    wrapped: Vec<String>,
}

struct Lexer<'o, 'd> {
    chars: Vec<char>,
    pos: usize,
    depth: usize,
    out: &'o mut Script,
    /// `Some` inside a substitution: quoted `cat` heredoc bodies go to the
    /// caller, which knows what consumes the substitution.
    defer: Option<&'d mut Vec<String>>,
    first_cmd: usize,
    word: Option<Tok>,
    toks: Vec<Tok>,
    redirects: Vec<Redirect>,
    pending: Option<String>,
    heredocs: Vec<Heredoc>,
    /// Deferred heredoc bodies (potential lone `cat <<'EOF'`) resolved in `finish_bodies`.
    bodies: Vec<HeredocBody>,
    /// Top-level quoted heredocs to allowlisted data sinks, resolved after scanning the full line.
    data_sink_bodies: Vec<HeredocBody>,
    cmd_deferred: Vec<String>,
    /// The command before the `|` just passed, for the next command.
    feeder: Option<Box<Cmd>>,
    feeder_idx: Option<usize>,
    /// Any pipe, command chaining (`;`, `&&`, `||`, `&`, `(`, `)`), process
    /// substitution (`<(` / `>(`), or `$(…)` / backtick appeared at this level.
    compound: bool,
    failed: bool,
}

fn lex(src: &str, depth: usize, out: &mut Script, defer: Option<&mut Vec<String>>) {
    if depth > MAX_DEPTH {
        out.opaque.push(src.to_string());
        return;
    }
    let mut lexer = Lexer::new(src, depth, out, defer);
    lexer.run();
    lexer.finish_all_bodies();
}

impl<'o, 'd> Lexer<'o, 'd> {
    fn new(
        src: &str,
        depth: usize,
        out: &'o mut Script,
        defer: Option<&'d mut Vec<String>>,
    ) -> Lexer<'o, 'd> {
        Lexer {
            chars: src.chars().collect(),
            pos: 0,
            depth,
            first_cmd: out.cmds.len(),
            out,
            defer,
            word: None,
            toks: Vec::new(),
            redirects: Vec::new(),
            pending: None,
            heredocs: Vec::new(),
            bodies: Vec::new(),
            data_sink_bodies: Vec::new(),
            cmd_deferred: Vec::new(),
            feeder: None,
            feeder_idx: None,
            compound: false,
            failed: false,
        }
    }
}

impl Lexer<'_, '_> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn take(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn word_mut(&mut self) -> &mut Tok {
        self.word.get_or_insert_with(Tok::default)
    }

    fn fail(&mut self) {
        self.failed = true;
        self.out.opaque.push(self.chars.iter().collect());
    }

    fn run(&mut self) {
        while !self.failed {
            let Some(c) = self.peek() else { break };
            match c {
                ' ' | '\t' | '\r' => {
                    self.pos += 1;
                    self.end_word();
                }
                '\n' => {
                    self.pos += 1;
                    self.end_cmd();
                    self.read_heredocs_or_defer();
                }
                ';' => {
                    self.compound = true;
                    self.pos += 1;
                    self.end_cmd();
                }
                '(' => {
                    self.compound = true;
                    self.note_function();
                    self.pos += 1;
                    self.end_cmd();
                    self.out.enter_scope();
                }
                ')' => {
                    self.compound = true;
                    self.pos += 1;
                    self.end_cmd();
                    self.out.exit_scope();
                }
                '|' => self.pipe(),
                '&' if self.chars.get(self.pos + 1) == Some(&'>') => self.redirect(),
                '&' => {
                    self.compound = true;
                    self.pos += 1;
                    self.end_cmd();
                }
                '<' | '>' => self.redirect(),
                '#' if self.word.is_none() => {
                    while self.peek().is_some_and(|c| c != '\n') {
                        self.pos += 1;
                    }
                }
                '\\' => {
                    self.pos += 1;
                    match self.take() {
                        // Line continuation, or a trailing backslash.
                        Some('\n') | None => {}
                        Some(ch) => {
                            let word = self.word_mut();
                            word.quoted = true;
                            word.text.push(ch);
                        }
                    }
                }
                '\'' => {
                    self.pos += 1;
                    self.single();
                }
                '"' => {
                    self.pos += 1;
                    self.double();
                }
                '$' => self.dollar(false),
                '`' => self.backtick(),
                '*' | '?' | '[' => {
                    self.pos += 1;
                    let word = self.word_mut();
                    word.dynamic = true;
                    word.text.push(c);
                }
                _ => {
                    self.pos += 1;
                    self.word_mut().text.push(c);
                }
            }
        }
        // Also after a failure: what was split so far is still checked.
        self.end_cmd();
    }

    /// `|` and `|&` hand the command just ended to the next one as its
    /// feeder; `||` starts an unrelated command.
    fn pipe(&mut self) {
        self.compound = true;
        self.pos += 1;
        let before = self.out.cmds.len();
        self.end_cmd();
        match self.peek() {
            Some('|') => self.pos += 1,
            next => {
                if next == Some('&') {
                    self.pos += 1;
                }
                // The command itself is stored last, after any it nests.
                // Nothing stored: the pipe comes out of a `( … )` group.
                let ended = self.out.cmds.len() > before;
                self.feeder_idx = ended.then(|| self.out.cmds.len() - 1);
                self.feeder = self
                    .out
                    .cmds
                    .last()
                    .filter(|_| ended)
                    .cloned()
                    .map(Box::new);
            }
        }
    }

    fn single(&mut self) {
        self.word_mut().quoted = true;
        loop {
            match self.take() {
                None => return self.fail(),
                Some('\'') => return,
                Some(c) => self.word_mut().text.push(c),
            }
        }
    }

    fn double(&mut self) {
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

    fn dollar(&mut self, in_double: bool) {
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
            _ => {
                // `$VAR`, `${VAR}`: the value is unknown, keep the text.
                let word = self.word_mut();
                word.dynamic = true;
                word.text.push('$');
            }
        }
    }

    fn backtick(&mut self) {
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

    fn substitute(&mut self, inner: &str) {
        self.compound = true;
        let mut deferred = Vec::new();
        self.out.enter_scope();
        lex(inner, self.depth + 1, self.out, Some(&mut deferred));
        self.out.exit_scope();
        self.cmd_deferred.append(&mut deferred);
        let word = self.word_mut();
        word.dynamic = true;
        word.text.push('$');
    }

    /// `$'…'`: decoded so `$'\x67it'` is seen as `git`.
    fn ansi_c(&mut self) {
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

    fn redirect(&mut self) {
        // A bare number glued to the operator is a file descriptor.
        match &self.word {
            Some(tok)
                if !tok.quoted
                    && !tok.text.is_empty()
                    && tok.text.chars().all(|c| c.is_ascii_digit()) =>
            {
                self.word = None;
            }
            _ => self.end_word(),
        }
        let mut op = String::new();
        if self.peek() == Some('&') {
            op.push('&');
            self.pos += 1;
        }
        let Some(first) = self.peek() else { return };
        while self.peek() == Some(first) && op.matches(first).count() < 3 {
            op.push(first);
            self.pos += 1;
        }
        match self.peek() {
            // Process substitution: the parenthesis splits the commands.
            Some('(') => {
                self.compound = true;
                return;
            }
            Some(c @ ('&' | '|')) => {
                op.push(c);
                self.pos += 1;
            }
            Some('-') if op == "<<" => {
                op.push('-');
                self.pos += 1;
            }
            Some('>') if op == "<" => {
                op.push('>');
                self.pos += 1;
            }
            _ => {}
        }
        self.pending = Some(op);
    }

    fn end_word(&mut self) {
        let Some(tok) = self.word.take() else { return };
        match self.pending.take() {
            Some(op) if op == "<<" || op == "<<-" => self.heredocs.push(Heredoc {
                delim: tok.text,
                strip_tabs: op == "<<-",
                quoted: tok.quoted,
                cleared: None,
                cmd_idx: None,
                pipeline: Vec::new(),
            }),
            Some(op) => self.redirects.push(Redirect {
                op,
                target: tok.text,
            }),
            None => self.toks.push(tok),
        }
    }
}
