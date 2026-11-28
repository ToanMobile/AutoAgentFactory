//! Turns the words of one simple command into a [`Cmd`]: strips wrappers and
//! recurses into nested interpreters.

use std::mem;

use super::decode::split_env_s;
use super::launcher::LAUNCHERS;
use super::options::{env_altpath, script_command, slot_payload, split_string, time_output};
use super::stdin::{
    ShellInput, clears_env, find_execs, names_fd, names_stdin, shell_input, split_assign,
    stdin_scripts,
};
use super::{
    Cmd, DATA_CONSUMERS, Lexer, MAX_DEPTH, RESERVED, Redirect, SHELLS, Script, Tok, basename,
    fold_name, lex,
};

pub const DECLARERS: &[&str] = &["export", "declare", "typeset", "readonly", "local"];
/// Commands that give a value to the variable named as a bare argument.
pub const NAME_SETTERS: &[&str] = &["read", "printf", "for", "select", "getopts"];

/// Zsh associative tables that override how bare command names resolve.
const ZSH_CMD_TABLES: &[&str] = &["functions", "aliases", "galiases", "saliases", "commands"];

fn rebinds_zsh_table_word(word: &str) -> bool {
    let Some((lhs, _)) = word.split_once('=') else {
        return false;
    };
    let lhs = lhs.strip_suffix('+').unwrap_or(lhs);
    if ZSH_CMD_TABLES.contains(&lhs) {
        return true;
    }
    let Some((table, rest)) = lhs.split_once('[') else {
        return false;
    };
    let Some(key) = rest.strip_suffix(']') else {
        return false;
    };
    if !ZSH_CMD_TABLES.contains(&table) {
        return false;
    }
    let clean_key = key.trim_matches(['\'', '"']);
    clean_key == "aaf" || key.contains(['$', '`', '*', '?', '['])
}

impl Cmd {
    /// The command gives `var` a value, whatever the value: as a prefix or
    /// through `env` (both in `assigns`), as an argument in assignment form
    /// (`export V=1`, `make V=1`), or by naming it to a builtin that sets it.
    pub fn sets_var(&self, var: &str) -> bool {
        let assigned = |word: &String| {
            word.strip_prefix(var)
                .is_some_and(|rest| rest.starts_with('=') || rest.starts_with("+="))
        };
        let names = self
            .words
            .first()
            .is_some_and(|w| DECLARERS.contains(&w.as_str()) || NAME_SETTERS.contains(&w.as_str()));
        self.assigns.iter().any(|(key, _)| key == var)
            || self.words.iter().any(assigned)
            || (names && self.args().iter().any(|arg| arg == var))
    }

    /// The command changes what a later bare `aaf` runs: an alias, a `hash`
    /// entry, a function loaded by name (each may be named at run time), zsh
    /// command-lookup tables (`functions[aaf]=…`, `aliases[aaf]=…`,
    /// `commands[aaf]=…`), or the search path.
    fn rebinds_aaf(&self) -> bool {
        let zsh_table = self
            .assigns
            .iter()
            .any(|(k, _)| ZSH_CMD_TABLES.contains(&k.as_str()))
            || self.words.iter().any(|w| rebinds_zsh_table_word(w));
        if zsh_table {
            return true;
        }
        match self.words.first().map(String::as_str) {
            Some("alias" | "hash" | "autoload") => self
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

    /// No command of the line is run by the trusted bare `aaf`: a payload of
    /// bare `aaf slot --` is judged as if typed bare, and `aaf` itself like any
    /// other program. Explicit absolute paths in `slot_bin` are unaffected by
    /// `PATH`, shell functions, or aliases.
    pub fn distrust_aaf(&mut self) {
        for cmd in &mut self.cmds {
            cmd.slotted = false;
            if cmd.slot_bin.as_deref() == Some("aaf") {
                cmd.slot_bin = None;
            }
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

    /// Ends the simple command being split; returns its index in
    /// `out.cmds` when one was stored (after any it nests, before the
    /// heredoc bodies its substitutions defer).
    pub(super) fn end_cmd(&mut self) -> Option<usize> {
        self.end_word();
        self.pending = None;
        let toks = mem::take(&mut self.toks);
        let redirects = mem::take(&mut self.redirects);
        let deferred = mem::take(&mut self.cmd_deferred);

        let plain = self.plain_step(&toks);
        let lead = self.keywords(&toks);
        let mut iter = toks.into_iter().skip(lead).peekable();
        let mut assigns = Vec::new();
        while let Some(pair) = iter.peek().and_then(split_assign) {
            assigns.push(pair);
            iter.next();
        }
        let words: Vec<Tok> = iter.collect();
        let mut stored = None;
        let name = if words.is_empty() && assigns.is_empty() && redirects.is_empty() {
            // A blank between `|` and its command keeps the feeder waiting.
            String::new()
        } else {
            let feeder = self.feeder.take();
            let feeder_idx = self.feeder_idx.take();
            // A heredoc on the `cat <<'EOF' |` line is read at its newline,
            // after `pipe` copied the feeder: the bodies it got since.
            let late = match (
                feeder.as_deref(),
                feeder_idx.and_then(|idx| self.out.cmds.get(idx)),
            ) {
                (Some(copy), Some(stored)) if copy.heredocs.len() < stored.heredocs.len() => {
                    Some((copy.clone(), stored.heredocs.clone()))
                }
                _ => None,
            };
            let from = self.out.cmds.len();
            let name = self.emit(assigns, words, redirects, feeder);
            if let Some((copy, bodies)) = late {
                self.give_late_bodies(from, &copy, bodies);
            }
            // The heredocs this command reads keep its wrapper's cleared
            // environment (`env -i sh <<EOF`): their bodies are split once
            // the line ends, after `emit` restored the outer state.
            let cmd_idx = self.out.cmds.len().checked_sub(1);
            stored = cmd_idx;
            self.out.plain_steps.extend(cmd_idx.filter(|_| plain));
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
            // Only `eval` runs the text in this shell; any other consumer
            // that runs it (`sh -c "$(cat <<'EOF' …)"`) starts a shell of
            // its own, so a `cd` there moves nothing here.
            let scoped = name != "eval";
            for body in deferred {
                if scoped {
                    self.out.enter_scope();
                }
                lex(&body, self.depth + 1, self.out, None);
                if scoped {
                    self.out.exit_scope();
                }
            }
        }
        stored
    }

    /// Number of leading reserved words, with what they take: `!`, `{`,
    /// `function NAME…`, `time [-p]`, `coproc [NAME]` before a group, and
    /// zsh's `repeat N`. A `{` after any of them still opens a group.
    fn keywords(&mut self, toks: &[Tok]) -> usize {
        let mut at = 0;
        while let Some(tok) = toks.get(at).filter(|tok| !tok.quoted) {
            at += 1;
            match tok.text.as_str() {
                word if RESERVED.contains(&word) => {}
                "function" => {
                    // zsh takes several names (`function a b { … }`).
                    while let Some(name) = toks.get(at).filter(|tok| tok.text != "{") {
                        self.out.functions.push(name.text.clone());
                        at += 1;
                    }
                }
                "time" => {
                    let option = |tok: &Tok| tok.text.len() > 1 && tok.text.starts_with('-');
                    while toks.get(at).is_some_and(option) {
                        at += 1;
                    }
                }
                "coproc" => {
                    let group = |tok: &Tok| !tok.quoted && matches!(tok.text.as_str(), "{" | "(");
                    if toks.get(at + 1).is_some_and(group) {
                        at += 1;
                    }
                }
                "repeat" => at += 1,
                _ => return at - 1,
            }
        }
        at.min(toks.len())
    }

    /// Stores the command and returns its name (empty when unknown).
    pub(super) fn emit(
        &mut self,
        mut assigns: Vec<(String, String)>,
        mut words: Vec<Tok>,
        mut redirects: Vec<Redirect>,
        feeder: Option<Box<Cmd>>,
    ) -> String {
        let mut wrapped = Vec::new();
        let mut xargs = false;
        let outer_slot = self.out.in_slot;
        let outer_slot_bin = self.out.in_slot_bin.clone();
        let outer_cleared = self.out.env_cleared;
        let mut env_s_depth = self.depth;
        loop {
            // A wrapper already stripped (`env -i env -S '…'`, `sudo env -S
            // '…'`) clears the environment of what a later one splits.
            self.out.env_cleared |= clears_env(&wrapped);
            let saved_assigns = assigns.len();
            match self.wrapper_len(
                &words,
                &wrapped,
                &mut assigns,
                &redirects,
                feeder.as_deref(),
            ) {
                Some(Wrapper::Strip(skip)) => {
                    if skip >= words.len() {
                        break;
                    }
                    match words.first().map(|w| w.text.as_str()) {
                        // Also for what the payload nests (`aaf slot -- sh -c '…'`).
                        // Bare `aaf` sets `in_slot` (protected by `aaf_untrusted`);
                        // an explicit path records `in_slot_bin` so `heavy_guard`
                        // can match it lexically against `trusted_aaf_bins` without
                        // ever canonicalizing untrusted `/tmp/aaf` symlinks.
                        Some("aaf") => {
                            // The search path given to aaf itself (`PATH=… aaf`,
                            // `env PATH=… aaf`): the payload's command carries the
                            // assignment and would hide it as its own.
                            self.out.rebound |= assigns
                                .iter()
                                .any(|(key, _)| key.eq_ignore_ascii_case("PATH"));
                            self.out.in_slot = true;
                            if self.out.in_slot_bin.is_none() {
                                self.out.in_slot_bin = Some("aaf".to_string());
                            }
                        }
                        Some(first) if basename(first) == "aaf" => {
                            if self.out.in_slot_bin.is_none() {
                                self.out.in_slot_bin = Some(first.to_string());
                            }
                        }
                        Some(first) if fold_name(first) == "xargs" => xargs = true,
                        // The external `time -o <file>` writes its report there.
                        Some(first) if matches!(fold_name(first).as_str(), "time" | "gtime") => {
                            redirects.extend(time_output(&words[1..skip]));
                        }
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
        let exact_name = words
            .first()
            .filter(|tok| !tok.dynamic)
            .map(|tok| basename(&tok.text).to_string())
            .unwrap_or_default();
        let name = exact_name.to_ascii_lowercase();
        if SHELLS.contains(&name.as_str()) {
            match shell_input(&words) {
                ShellInput::Command(script) => {
                    self.out.enter_scope_wrapped(wrapped.clone());
                    lex(script, self.depth + 1, self.out, None);
                    self.out.exit_scope();
                }
                input @ (ShellInput::Stdin | ShellInput::Fd) => {
                    // A here-string on `/dev/fd/N`; only fd 0 is the pipe.
                    let fed = feeder
                        .as_deref()
                        .filter(|_| matches!(input, ShellInput::Stdin));
                    for script in stdin_scripts(&redirects, fed) {
                        self.out.enter_scope_wrapped(wrapped.clone());
                        lex(&script, self.depth + 1, self.out, None);
                        self.out.exit_scope();
                    }
                }
                ShellInput::File => {}
            }
        } else if matches!(name.as_str(), "." | "source")
            && words
                .get(1)
                .is_some_and(|tok| names_stdin(&tok.text) || names_fd(&tok.text))
        {
            // Read in this shell, like `eval`: no scope of its own.
            let fed = feeder
                .as_deref()
                .filter(|_| words.get(1).is_some_and(|tok| names_stdin(&tok.text)));
            for script in stdin_scripts(&redirects, fed) {
                lex(&script, self.depth + 1, self.out, None);
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
        let dynamic_name = words.first().is_some_and(|tok| tok.dynamic);
        let arg_dynamic = words.iter().map(|tok| tok.dynamic).collect();
        let mut arg_cat_heredoc = Vec::with_capacity(words.len());
        let mut text_words = Vec::with_capacity(words.len());
        for tok in words {
            arg_cat_heredoc.push(tok.cat_heredoc);
            text_words.push(tok.text);
        }
        let folded_name = text_words
            .first()
            .map_or_else(String::new, |w| fold_name(w));
        self.out.cmds.push(Cmd {
            assigns,
            dynamic_name,
            arg_dynamic,
            arg_cat_heredoc,
            folded_name,
            words: text_words,
            redirects,
            heredocs: Vec::new(),
            wrapped,
            xargs,
            feeder,
            slotted: self.out.in_slot,
            slot_bin: self.out.in_slot_bin.clone(),
            // Decided for the whole line once it is split: `Script::settle`.
            aaf_untrusted: false,
            env_cleared: self.out.env_cleared,
        });
        self.out.in_slot = outer_slot;
        self.out.in_slot_bin = outer_slot_bin;
        self.out.env_cleared = outer_cleared;
        exact_name
    }

    /// Number of leading words that only wrap the real command, or an `env -S`
    /// expansion to splice into `words` while preserving prior wrappers/xargs.
    fn wrapper_len(
        &mut self,
        words: &[Tok],
        wrapped: &[String],
        assigns: &mut Vec<(String, String)>,
        redirects: &[Redirect],
        feeder: Option<&Cmd>,
    ) -> Option<Wrapper> {
        let first = words.first().filter(|tok| !tok.dynamic)?;
        let folded = fold_name(&first.text);
        let name = folded.as_str();
        if name == "aaf" {
            return slot_payload(words).map(Wrapper::Strip);
        }
        if LAUNCHERS.contains(&name) {
            return self
                .launcher_len(name, words, redirects, feeder)
                .map(Wrapper::Strip);
        }
        // Options of each wrapper that take a separate value: the
        // single-letter ones, then the long ones. Missing one would make its
        // value the command and hide the real one behind it.
        let (valued, long_valued): (&str, &[&str]) = match name {
            "command" | "builtin" | "nohup" | "noglob" | "setsid" | "nocorrect" | "-" => ("", &[]),
            // The external `time` (GNU, BSD) and `gtime`: `-o <file>`,
            // `-f <format>`; the keyword `time` is read in `keywords`.
            "time" | "gtime" => ("of", &["--output", "--format"]),
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
