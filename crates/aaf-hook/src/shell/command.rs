//! Turns the words of one simple command into a [`Cmd`]: strips wrappers and
//! recurses into nested interpreters.

use std::mem;

use super::decode::split_env_s;
use super::stdin::{ShellInput, clears_env, find_execs, shell_input, split_assign, stdin_scripts};
use super::{
    Cmd, DATA_CONSUMERS, Lexer, MAX_DEPTH, RESERVED, Redirect, SHELLS, Script, Tok, basename, lex,
};

pub const DECLARERS: &[&str] = &["export", "declare", "typeset", "readonly", "local"];
/// Commands that give a value to the variable named as a bare argument.
pub const NAME_SETTERS: &[&str] = &["read", "printf", "for", "select", "getopts"];

impl Cmd {
    /// The command gives `var` a value, whatever the value: as a prefix or
    /// through `env` (both in `assigns`), as an argument in assignment form
    /// (`export V=1`, `make V=1`), or by naming it to a builtin that sets it.
    pub fn sets_var(&self, var: &str) -> bool {
        let assigned = |word: &String| {
            word.strip_prefix(var)
                .is_some_and(|rest| rest.starts_with('=') || rest.starts_with("+="))
        };
        let names = DECLARERS.contains(&self.name()) || NAME_SETTERS.contains(&self.name());
        self.assigns.iter().any(|(key, _)| key == var)
            || self.words.iter().any(assigned)
            || (names && self.args().iter().any(|arg| arg == var))
    }

    /// The command changes what a later bare `aaf` runs: an alias, a `hash`
    /// entry, a function loaded by name (each may be named at run time), or
    /// the search path.
    fn rebinds_aaf(&self) -> bool {
        match self.name() {
            "alias" | "hash" | "autoload" => self
                .args()
                .iter()
                .any(|arg| arg.contains("aaf") || arg.contains('$')),
            _ => self.sets_var("PATH") || self.sets_var("path"),
        }
    }
}

impl Script {
    /// Run once the whole line is split: only then is it known whether the
    /// word `aaf` still means the installed CLI on it.
    pub(super) fn settle(&mut self) {
        // What a payload of `aaf slot --` sets (`aaf slot -- env PATH=… cmd`)
        // comes after aaf was found and changes nothing about it.
        let rebound = |cmd: &Cmd| !cmd.slotted && cmd.rebinds_aaf();
        if self.rebound
            || self.functions.iter().any(|name| name == "aaf")
            || self.cmds.iter().any(rebound)
        {
            self.distrust_aaf();
        }
    }

    /// No command of the line is run by the trusted `aaf`: a payload of
    /// `aaf slot --` is judged as if typed bare, and `aaf` itself like any
    /// other program.
    pub fn distrust_aaf(&mut self) {
        for cmd in &mut self.cmds {
            cmd.slotted = false;
            cmd.aaf_untrusted = true;
        }
    }

    pub fn aaf_untrusted(&self) -> bool {
        self.cmds.iter().any(|cmd| cmd.aaf_untrusted)
    }
}

impl Lexer<'_, '_> {
    /// Called at a `(`: `name()` and `name ()` define a shell function.
    pub(super) fn note_function(&mut self) {
        let rest = self.chars.get(self.pos + 1..).unwrap_or(&[]);
        let closes = rest.iter().find(|c| !matches!(c, ' ' | '\t')) == Some(&')');
        if self.peek() == Some('(') && closes {
            // zsh defines every word before the parentheses (`a b() { … }`).
            let names = self.toks.iter().chain(&self.word);
            self.out.functions.extend(names.map(|tok| tok.text.clone()));
        }
    }

    pub(super) fn end_cmd(&mut self) {
        self.end_word();
        self.pending = None;
        let toks = mem::take(&mut self.toks);
        let redirects = mem::take(&mut self.redirects);
        let deferred = mem::take(&mut self.cmd_deferred);

        let mut iter = toks.into_iter().peekable();
        loop {
            match iter.peek() {
                Some(tok) if !tok.quoted && RESERVED.contains(&tok.text.as_str()) => {
                    iter.next();
                }
                Some(tok) if !tok.quoted && tok.text == "function" => {
                    iter.next();
                    // zsh takes several names (`function a b { … }`).
                    while let Some(name) = iter.next_if(|tok| tok.text != "{") {
                        self.out.functions.push(name.text);
                    }
                }
                _ => break,
            }
        }
        let mut assigns = Vec::new();
        while let Some(pair) = iter.peek().and_then(split_assign) {
            assigns.push(pair);
            iter.next();
        }
        let words: Vec<Tok> = iter.collect();
        let name = if words.is_empty() && assigns.is_empty() && redirects.is_empty() {
            // A blank between `|` and its command keeps the feeder waiting.
            String::new()
        } else {
            let feeder = self.feeder.take();
            let feeder_idx = self.feeder_idx.take();
            let name = self.emit(assigns, words, redirects, feeder);
            // The heredocs this command reads keep its wrapper's cleared
            // environment (`env -i sh <<EOF`): their bodies are split once
            // the line ends, after `emit` restored the outer state.
            let cmd_idx = self.out.cmds.len().checked_sub(1);
            let cleared = self.out.cmds.last().is_some_and(|cmd| cmd.env_cleared);
            for doc in self.heredocs.iter_mut().filter(|doc| doc.cleared.is_none()) {
                doc.cleared = Some(cleared);
                doc.cmd_idx = cmd_idx;
            }
            if let (Some(src_idx), Some(dst_idx)) = (feeder_idx, cmd_idx) {
                for doc in self
                    .heredocs
                    .iter_mut()
                    .filter(|doc| doc.pipeline.last().copied().or(doc.cmd_idx) == Some(src_idx))
                {
                    doc.pipeline.push(dst_idx);
                }
            }
            name
        };
        if !DATA_CONSUMERS.contains(&name.as_str()) {
            for body in deferred {
                lex(&body, self.depth + 1, self.out, None);
            }
        }
    }

    /// Stores the command and returns its name (empty when unknown).
    pub(super) fn emit(
        &mut self,
        mut assigns: Vec<(String, String)>,
        mut words: Vec<Tok>,
        redirects: Vec<Redirect>,
        feeder: Option<Box<Cmd>>,
    ) -> String {
        let mut wrapped = Vec::new();
        let mut xargs = false;
        let outer_slot = self.out.in_slot;
        let outer_cleared = self.out.env_cleared;
        let mut env_s_depth = self.depth;
        loop {
            // A wrapper already stripped (`env -i env -S '…'`, `sudo env -S
            // '…'`) clears the environment of what a later one splits.
            self.out.env_cleared |= clears_env(&wrapped);
            let saved_assigns = assigns.len();
            match self.wrapper_len(&words, &wrapped, &mut assigns) {
                Some(Wrapper::Strip(skip)) => {
                    if skip >= words.len() {
                        break;
                    }
                    match words.first().map(|w| w.text.as_str()) {
                        // Also for what the payload nests (`aaf slot -- sh -c '…'`).
                        // Only the bare name is the installed aaf: `./aaf` or
                        // `/tmp/x/aaf` is any program, and its payload is judged as
                        // if typed without it.
                        Some("aaf") => {
                            // The search path given to aaf itself (`PATH=… aaf`,
                            // `env PATH=… aaf`): the payload's command carries the
                            // assignment and would hide it as its own.
                            self.out.rebound |= assigns
                                .iter()
                                .any(|(key, _)| key.eq_ignore_ascii_case("PATH"));
                            self.out.in_slot = true;
                        }
                        Some(first) if basename(first) == "xargs" => xargs = true,
                        _ => {}
                    }
                    wrapped.extend(words.drain(..skip).map(|tok| tok.text));
                }
                Some(Wrapper::ExpandEnv { at, step, script }) => {
                    assigns.truncate(saved_assigns);
                    let prefix: Vec<String> = words[..=at].iter().map(|w| w.text.clone()).collect();
                    self.out.env_cleared |= clears_env(&prefix);
                    let raw_opaque = || {
                        let mut parts: Vec<&str> = wrapped.iter().map(String::as_str).collect();
                        parts.extend(words.iter().map(|w| w.text.as_str()));
                        parts.join(" ")
                    };
                    if env_s_depth >= MAX_DEPTH {
                        self.out.opaque.push(raw_opaque());
                        break;
                    }
                    let Some(parsed) = split_env_s(&script) else {
                        self.out.opaque.push(raw_opaque());
                        break;
                    };
                    env_s_depth += 1;
                    let mut combined = Vec::with_capacity(words.len() + parsed.len());
                    combined.push(words[0].clone());
                    combined.extend(words[1..at].iter().cloned());
                    combined.extend(parsed);
                    if let Some(trailing) = words.get(at + step..) {
                        combined.extend(trailing.iter().cloned());
                    }
                    words = combined;
                }
                None => break,
            }
        }
        let name = words
            .first()
            .filter(|tok| !tok.dynamic)
            .map(|tok| basename(&tok.text).to_string())
            .unwrap_or_default();
        if SHELLS.contains(&name.as_str()) {
            match shell_input(&words) {
                ShellInput::Command(script) => {
                    self.out.enter_scope_wrapped(wrapped.clone());
                    lex(script, self.depth + 1, self.out, None);
                    self.out.exit_scope();
                }
                ShellInput::Stdin => {
                    for script in stdin_scripts(&redirects, feeder.as_deref()) {
                        self.out.enter_scope_wrapped(wrapped.clone());
                        lex(&script, self.depth + 1, self.out, None);
                        self.out.exit_scope();
                    }
                }
                ShellInput::File => {}
            }
        } else if name == "eval" {
            let joined = words
                .iter()
                .skip(1)
                .map(|tok| tok.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            lex(&joined, self.depth + 1, self.out, None);
        } else if name == "find" {
            for sub in find_execs(&words) {
                self.emit(Vec::new(), sub, Vec::new(), None);
            }
        }
        self.out.cmds.push(Cmd {
            assigns,
            dynamic_name: words.first().is_some_and(|tok| tok.dynamic),
            arg_dynamic: words.iter().map(|tok| tok.dynamic).collect(),
            words: words.into_iter().map(|tok| tok.text).collect(),
            redirects,
            heredocs: Vec::new(),
            wrapped,
            xargs,
            feeder,
            slotted: self.out.in_slot,
            // Decided for the whole line once it is split: `Script::settle`.
            aaf_untrusted: false,
            env_cleared: self.out.env_cleared,
        });
        self.out.in_slot = outer_slot;
        self.out.env_cleared = outer_cleared;
        name
    }

    /// Number of leading words that only wrap the real command, or an `env -S`
    /// expansion to splice into `words` while preserving prior wrappers/xargs.
    fn wrapper_len(
        &mut self,
        words: &[Tok],
        wrapped: &[String],
        assigns: &mut Vec<(String, String)>,
    ) -> Option<Wrapper> {
        let first = words.first().filter(|tok| !tok.dynamic)?;
        let name = basename(&first.text);
        if name == "aaf" {
            return slot_payload(words).map(Wrapper::Strip);
        }
        // Options of each wrapper that take a separate value: the
        // single-letter ones, then the long ones. Missing one would make its
        // value the command and hide the real one behind it.
        let (valued, long_valued): (&str, &[&str]) = match name {
            "command" | "builtin" | "nohup" | "time" | "noglob" | "setsid" => ("", &[]),
            // `unbuffer` and `script` run their command on a terminal of its
            // own. `script [options] <file> [command]` (BSD), or the command
            // as the value of `-c` (util-linux).
            "unbuffer" => ("", &[]),
            "script" => (
                "tTIOBmEo",
                &[
                    "--log-in",
                    "--log-out",
                    "--log-io",
                    "--log-timing",
                    "--logging-format",
                    "--echo",
                    "--output-limit",
                ],
            ),
            "exec" => ("a", &[]),
            "sudo" | "doas" => (
                "ugpChDRTU",
                &[
                    "--user",
                    "--group",
                    "--prompt",
                    "--close-from",
                    "--chdir",
                    "--chroot",
                    "--host",
                    "--role",
                    "--type",
                    "--other-user",
                    "--command-timeout",
                ],
            ),
            "env" => ("uCP", &["--unset", "--chdir"]),
            "nice" => ("n", &["--adjustment"]),
            "timeout" => ("sk", &["--signal", "--kill-after"]),
            "stdbuf" => ("ioe", &["--input", "--output", "--error"]),
            "ionice" => ("cn", &["--class", "--classdata"]),
            "caffeinate" => ("tw", &[]),
            "xargs" => (
                "nIPLdEsSJRa",
                &[
                    "--max-args",
                    "--max-procs",
                    "--max-chars",
                    "--delimiter",
                    "--arg-file",
                    "--process-slot-var",
                ],
            ),
            _ => return None,
        };
        let mut at = 1;
        while let Some(tok) = words.get(at) {
            let text = tok.text.as_str();
            let carried = match name {
                "env" => split_string(text),
                "script" => script_command(text),
                _ => None,
            };
            if name == "env"
                && let Some(glued) = env_altpath(text)
            {
                let dirs = match glued {
                    "" => words.get(at + 1).map(|next| next.text.as_str()),
                    glued => Some(glued),
                };
                if let Some(dirs) = dirs {
                    // BSD `env -P <dirs>` (separate, glued, or clustered) is the search path.
                    assigns.push(("PATH".to_string(), dirs.to_string()));
                }
            }
            if let Some(glued) = carried {
                // `-S 'cmd'`, `-iS 'cmd'`, `-Scmd`, `--split-string=cmd`.
                let step = if glued.is_empty() { 2 } else { 1 };
                let script = match glued {
                    "" => words.get(at + 1).map(|next| next.text.as_str()),
                    glued => Some(glued),
                };
                if let Some(script) = script {
                    if name == "env" {
                        return Some(Wrapper::ExpandEnv {
                            at,
                            step,
                            script: script.to_string(),
                        });
                    }
                    let prefix: Vec<String> = words[..=at].iter().map(|w| w.text.clone()).collect();
                    let outer_cleared = self.out.env_cleared;
                    self.out.env_cleared |= clears_env(&prefix);
                    self.out.enter_scope_wrapped(wrapped.to_vec());
                    lex(script, self.depth + 1, self.out, None);
                    self.out.exit_scope();
                    self.out.env_cleared = outer_cleared;
                }
                at += step;
            } else if name == "env" && text == "-" {
                // `env -` is `env -i`; left as the command it hid the real one.
                at += 1;
            } else if text.starts_with('-') && text.len() > 1 {
                // In a cluster (`-iu NAME`, `-Eu owner`) the first letter that
                // takes a value takes the rest of the word, or the next word.
                let takes_value = if text.starts_with("--") {
                    long_valued.contains(&text)
                } else {
                    text.find(|c| valued.contains(c)) == Some(text.len() - 1)
                };
                at += if takes_value { 2 } else { 1 };
            } else if matches!(name, "env" | "sudo")
                && let Some(pair) = split_assign(tok)
            {
                assigns.push(pair);
                at += 1;
            } else {
                break;
            }
        }
        // The duration, and the file `script` records into.
        if matches!(name, "timeout" | "script") {
            at += 1;
        }
        Some(Wrapper::Strip(at))
    }
}

enum Wrapper {
    Strip(usize),
    ExpandEnv {
        at: usize,
        step: usize,
        script: String,
    },
}

/// `aaf [--json] slot [options] -- <command>`: where the command starts, so
/// the guards judge it like one typed bare. Without the `--` no word is
/// known to be a command and `aaf` stays the command.
fn slot_payload(words: &[Tok]) -> Option<usize> {
    let sub = 1 + words
        .iter()
        .skip(1)
        .position(|tok| !tok.text.starts_with('-'))?;
    let slot = words.get(sub).filter(|tok| !tok.dynamic)?;
    if slot.text != "slot" {
        return None;
    }
    let dashes = words
        .iter()
        .skip(sub + 1)
        .position(|tok| tok.text == "--")?;
    Some(sub + dashes + 2)
}

/// For an `env` option that carries a command line (`-S`, a cluster ending
/// in or holding `S`, `--split-string[=…]`): the part glued to it, empty
/// when the command line is the next word.
fn split_string(text: &str) -> Option<&str> {
    if let Some(long) = text.strip_prefix("--split-string") {
        return match long.strip_prefix('=') {
            Some(glued) => Some(glued),
            None => long.is_empty().then_some(""),
        };
    }
    if !text.starts_with('-') || text.starts_with("--") {
        return None;
    }
    // `u`, `C` and `P` come first when they do: `-uS` unsets a variable named S,
    // `-P/dirS` sets the search path to `/dirS`.
    let at = text.find(['S', 'u', 'C', 'P'])?;
    text[at..].strip_prefix('S')
}

/// For BSD `env -P <dirs>`, `-P<dirs>`, or a cluster ending in `P` (`-iP <dirs>`):
/// returns the glued directory, or `""` when the directory is the next word.
fn env_altpath(text: &str) -> Option<&str> {
    if !text.starts_with('-') || text.starts_with("--") {
        return None;
    }
    let at = text.find(['P', 'u', 'C', 'S'])?;
    text[at..].strip_prefix('P')
}

/// For util-linux `script -c <cmd>`, `-c<cmd>`, `-qc<cmd>`, or `--command[=<cmd>]`:
/// returns the glued command, or `""` when the command is the next word.
fn script_command(text: &str) -> Option<&str> {
    if let Some(long) = text.strip_prefix("--command") {
        return match long.strip_prefix('=') {
            Some(glued) => Some(glued),
            None => long.is_empty().then_some(""),
        };
    }
    if !text.starts_with('-') || text.starts_with("--") {
        return None;
    }
    let at = text.find(['c', 't', 'T', 'I', 'O', 'B', 'm', 'E', 'o'])?;
    text[at..].strip_prefix('c')
}
