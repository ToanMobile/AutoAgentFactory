//! Shell stdin/script helpers extracted from `shell::command`: `-c`/stdin
//! classification, `echo`/`printf` pipeline extraction, env-clearing checks,
//! and `find -exec` extraction.

use std::mem;

use super::decode::{
    base64_ignores_garbage, base64_is_decode, decode_base64, decode_escapes, expand_printf,
};
use super::scan::read_heredoc_body;
use super::{Cmd, HeredocBody, Lexer, MAX_DEPTH, Redirect, SHELLS, Script, Tok, fold_name, lex};

/// Where a shell (`sh`, `bash`, `zsh`, …) takes the script it runs from.
pub(super) enum ShellInput<'a> {
    /// `-c`: the first word that is not an option, wherever `-c` stands
    /// (`sh -c -- '…'`, `bash -c -x '…'`, `bash -o pipefail -c '…'`).
    Command(&'a str),
    /// `-s`, or no operand: standard input.
    Stdin,
    /// `/dev/fd/N`: what a redirection on the command opens as fd N.
    Fd,
    /// The first operand names a script file.
    File,
}

pub(super) fn shell_input(words: &[Tok]) -> ShellInput<'_> {
    let (mut command, mut stdin) = (false, false);
    let mut at = 1;
    while let Some(word) = words.get(at).map(|tok| tok.text.as_str()) {
        if word == "--" || word == "-" {
            at += 1;
            break;
        }
        let flags = match word.strip_prefix(['-', '+']) {
            Some(flags) if !flags.is_empty() => flags,
            _ => break,
        };
        at += 1;
        if word.starts_with("--") {
            if matches!(word, "--rcfile" | "--init-file") {
                at += 1;
            }
            continue;
        }
        let set = word.starts_with('-');
        command |= set && flags.contains('c');
        stdin |= set && flags.contains('s');
        // `-o pipefail`, `+O extglob`: the option name is the next word.
        if flags.contains(['o', 'O']) {
            at += 1;
        }
    }
    match words.get(at) {
        Some(tok) if command => ShellInput::Command(&tok.text),
        // `-c` without its string: the shell refuses to start.
        None if command => ShellInput::File,
        _ if stdin => ShellInput::Stdin,
        None => ShellInput::Stdin,
        Some(tok) if names_stdin(&tok.text) => ShellInput::Stdin,
        Some(tok) if names_fd(&tok.text) => ShellInput::Fd,
        Some(_) => ShellInput::File,
    }
}

/// A script operand that is standard input itself (`bash /dev/stdin`,
/// `. /dev/fd/0`, `bash /proc/self/fd/0`, `bash -- -`).
pub(super) fn names_stdin(word: &str) -> bool {
    matches!(word, "/dev/stdin" | "/dev/fd/0" | "/proc/self/fd/0" | "-")
}

/// A script operand that is another open file descriptor (`/dev/fd/3`,
/// `/proc/self/fd/3`): fed by a redirection on the command (`3<<< '…'`).
pub(super) fn names_fd(word: &str) -> bool {
    ["/dev/fd/", "/proc/self/fd/"]
        .iter()
        .filter_map(|prefix| word.strip_prefix(prefix))
        .any(|fd| !fd.is_empty() && fd.bytes().all(|b| b.is_ascii_digit()))
}

/// The literal script a shell or interpreter reading standard input gets: a
/// here-string, or what `echo`/`printf`/`cat <<EOF`/`base64 -d` pipe into it.
/// Heredoc bodies on the command itself are stored in `Cmd::heredocs` and split
/// in `read_heredocs` / `finish_bodies`.
pub(crate) fn stdin_scripts(redirects: &[Redirect], feeder: Option<&Cmd>) -> Vec<String> {
    stdin_scripts_inner(redirects, feeder, 0)
}

fn stdin_scripts_inner(redirects: &[Redirect], feeder: Option<&Cmd>, depth: usize) -> Vec<String> {
    if depth > MAX_DEPTH {
        return Vec::new();
    }
    let mut scripts: Vec<String> = redirects
        .iter()
        .filter(|redirect| redirect.op == "<<<")
        .map(|redirect| redirect.target.clone())
        .collect();
    let Some(cmd) = feeder.filter(|cmd| !cmd.dynamic_name) else {
        return scripts;
    };
    let args = cmd.args();
    let echo_flags = |arg: &&String| {
        arg.len() > 1
            && arg.starts_with('-')
            && arg[1..].chars().all(|c| matches!(c, 'n' | 'e' | 'E'))
    };
    match cmd.name() {
        "echo" => {
            let text = args[args.iter().take_while(echo_flags).count()..].join(" ");
            scripts.push(decode_escapes(&text, true));
        }
        "printf" => {
            let rest = if args.first().map(String::as_str) == Some("--") {
                &args[1..]
            } else {
                args
            };
            if rest.len() >= 2 {
                scripts.push(decode_escapes(&rest[0], false));
                let joined = decode_escapes(&rest[1..].join(" "), false);
                scripts.push(joined.clone());
                if let Some(formatted) = expand_printf(&rest[0], &rest[1..])
                    && formatted != joined
                {
                    scripts.push(formatted);
                }
            } else if let Some(first) = rest.first() {
                scripts.push(decode_escapes(first, false));
            }
        }
        "cat" => {
            if !cmd.heredocs.is_empty() {
                scripts.extend(cmd.heredocs.iter().cloned());
            } else if args.is_empty() || args == ["-"] {
                scripts.extend(stdin_scripts_inner(
                    &cmd.redirects,
                    cmd.feeder.as_deref(),
                    depth + 1,
                ));
            }
        }
        "base64" if base64_is_decode(args) => {
            let ignore_garbage = base64_ignores_garbage(args);
            let inputs = stdin_scripts_inner(&cmd.redirects, cmd.feeder.as_deref(), depth + 1);
            for raw in inputs.iter().chain(&cmd.heredocs) {
                if let Some(decoded) = decode_base64(raw, ignore_garbage) {
                    scripts.push(decoded);
                }
            }
        }
        _ => {}
    }
    scripts
}

/// Single-quotes `word` so re-lexing preserves its exact text.
pub(crate) fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

pub(super) fn split_assign(tok: &Tok) -> Option<(String, String)> {
    let (key, value) = tok.text.split_once('=')?;
    let key = key.strip_suffix('+').unwrap_or(key);
    let mut chars = key.chars();
    let first = chars.next()?;
    let valid = (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    valid.then(|| (key.to_string(), value.to_string()))
}

/// True when wrapper words clear or reset the environment (`env -i`, `env -`,
/// `exec -c`, `sudo`, `doas`).
pub(super) fn clears_env(words: &[String]) -> bool {
    let mut tool = String::new();
    words.iter().any(|word| {
        if !word.starts_with('-') || word == "-" {
            let base = fold_name(word);
            if matches!(base.as_str(), "env" | "exec" | "sudo" | "doas") {
                tool = base;
            }
            return matches!(tool.as_str(), "sudo" | "doas") || (tool == "env" && word == "-");
        }
        let cluster = !word.starts_with("--");
        match tool.as_str() {
            "env" => {
                word == "--ignore-environment"
                    || (cluster && {
                        // Stop before any valued option (`u`, `C`, `P`, `S`) so an `i`
                        // inside a glued value (`-P/tmp/evil`) is not read as `-i`.
                        let flags = word
                            .find(['u', 'C', 'P', 'S'])
                            .map_or(word.as_str(), |at| &word[..at]);
                        flags.contains('i')
                    })
            }
            "exec" => cluster && word.contains('c'),
            _ => false,
        }
    })
}

/// The commands `find` would run through `-exec` and friends.
pub(super) fn find_execs(words: &[Tok]) -> Vec<Vec<Tok>> {
    let mut subs = Vec::new();
    let mut at = 1;
    while at < words.len() {
        if matches!(
            words[at].text.as_str(),
            "-exec" | "-execdir" | "-ok" | "-okdir"
        ) {
            let end = (at + 1..words.len())
                .find(|&end| {
                    words[end].text == ";"
                        || (words[end].text == "+" && end - 1 > at && words[end - 1].text == "{}")
                })
                .unwrap_or(words.len());
            if end > at + 1 {
                subs.push(words[at + 1..end].to_vec());
            }
            at = end;
        }
        at += 1;
    }
    subs
}

fn feeder_at_mut(mut cmd: &mut Cmd, hops: usize) -> Option<&mut Cmd> {
    for _ in 0..hops {
        cmd = cmd.feeder.as_deref_mut()?;
    }
    Some(cmd)
}

impl Lexer<'_, '_> {
    pub(super) fn read_heredocs(&mut self) {
        let mut pending = mem::take(&mut self.heredocs);
        for h in 0..pending.len() {
            let strip_tabs = pending[h].strip_tabs;
            let delim = pending[h].delim.clone();
            let quoted = pending[h].quoted;
            let mut cleared = pending[h].cleared.unwrap_or(false);
            let cmd_idx = pending[h].cmd_idx;
            let pipeline = mem::take(&mut pending[h].pipeline);

            let (body, ended) = read_heredoc_body(&self.chars, &mut self.pos, &delim, strip_tabs);
            // No delimiter line: the shell may end the body where the hook
            // did not, so the "body" may be the rest of the script. Never
            // data then.
            let quoted = quoted && ended;
            let mut is_interp = false;
            let mut is_shell = false;
            let mut owner_wrapped = Vec::new();
            let mut owner_slotted = false;
            let mut owner_slot_bin: Option<String> = None;
            let mut base64_stages: Vec<bool> = Vec::new();
            let mut may_be_lone_cat = false;
            if let Some(idx) = cmd_idx {
                let cmds_len = self.out.cmds.len();
                if let Some(cmd) = self.out.cmds.get_mut(idx) {
                    is_interp |= crate::interp_guard::reads_program_from_stdin(cmd);
                    is_shell |= SHELLS.contains(&cmd.name());
                    owner_wrapped = cmd.wrapped.clone();
                    owner_slotted |= cmd.slotted;
                    if owner_slot_bin.is_none() {
                        owner_slot_bin = cmd.slot_bin.clone();
                    }
                    if cmd.name() == "base64" && base64_is_decode(cmd.args()) {
                        base64_stages.push(base64_ignores_garbage(cmd.args()));
                    }
                    may_be_lone_cat = quoted
                        && pipeline.is_empty()
                        && cmds_len == self.first_cmd + 1
                        && cmd.words == ["cat"]
                        && cmd.assigns.is_empty()
                        && cmd.redirects.is_empty();
                    cmd.heredocs.push(body.clone());
                }
                for (step, &consumer_idx) in pipeline.iter().enumerate() {
                    if let Some(consumer) = self.out.cmds.get_mut(consumer_idx) {
                        cleared |= consumer.env_cleared;
                        owner_slotted |= consumer.slotted;
                        if owner_slot_bin.is_none() {
                            owner_slot_bin = consumer.slot_bin.clone();
                        }
                        if consumer.name() == "base64" && base64_is_decode(consumer.args()) {
                            base64_stages.push(base64_ignores_garbage(consumer.args()));
                        }
                        is_shell |= SHELLS.contains(&consumer.name());
                        let consumer_is_interp =
                            crate::interp_guard::reads_program_from_stdin(consumer);
                        if let Some(feeder) = feeder_at_mut(consumer, step + 1) {
                            is_interp |= consumer_is_interp;
                            feeder.heredocs.push(body.clone());
                        }
                        if SHELLS.contains(&consumer.name()) && !consumer.wrapped.is_empty() {
                            owner_wrapped = consumer.wrapped.clone();
                        }
                    }
                }
            }
            // The pipe after the owner is still waiting for its consumer.
            let awaits_consumer = self.feeder_idx.is_some()
                && pipeline.last().copied().or(cmd_idx) == self.feeder_idx;
            if quoted && is_interp && !is_shell {
                // Quoted heredoc feeding an interpreter: shell does not run it,
                // and `interp_guard` inspects `Cmd::heredocs` / `feeder.heredocs`.
            } else if may_be_lone_cat || awaits_consumer {
                // Defer until `finish_bodies` to know whether `cat <<'EOF'`
                // stayed the sole command in this script/substitution, or
                // (`cat <<EOF |` with the consumer on the line after the
                // body) which environment the consumer runs it under.
                self.bodies.push(HeredocBody {
                    body,
                    quoted,
                    cleared,
                    is_interp,
                    wrapped: owner_wrapped,
                });
            } else {
                let mut sub = Script {
                    in_slot: self.out.in_slot || owner_slotted,
                    in_slot_bin: self.out.in_slot_bin.clone().or(owner_slot_bin),
                    env_cleared: self.out.env_cleared | cleared,
                    ..Script::default()
                };
                sub.enter_scope_wrapped(owner_wrapped);
                lex(&body, self.depth + 1, &mut sub, None);
                let mut cur = body;
                for ignore_garbage in base64_stages.into_iter().take(MAX_DEPTH) {
                    let Some(decoded) = decode_base64(&cur, ignore_garbage) else {
                        break;
                    };
                    lex(&decoded, self.depth + 1, &mut sub, None);
                    cur = decoded;
                }
                sub.exit_scope();

                self.out.opaque.extend(sub.opaque);
                self.out.functions.extend(sub.functions);
                self.out.rebound |= sub.rebound;

                let inserted_len = sub.cmds.len();
                if inserted_len > 0 {
                    let insert_at = pipeline
                        .last()
                        .copied()
                        .or(cmd_idx)
                        .map_or(self.out.cmds.len(), |i| (i + 1).min(self.out.cmds.len()));
                    let scope_pos = self
                        .out
                        .scopes
                        .iter()
                        .position(|(at, _)| *at >= insert_at)
                        .unwrap_or(self.out.scopes.len());
                    for (at, _) in &mut self.out.scopes[scope_pos..] {
                        *at += inserted_len;
                    }
                    let shifted_scopes =
                        sub.scopes.into_iter().map(|(at, ev)| (at + insert_at, ev));
                    self.out.scopes.splice(scope_pos..scope_pos, shifted_scopes);
                    self.out.cmds.splice(insert_at..insert_at, sub.cmds);
                    for rem in &mut pending[h + 1..] {
                        if let Some(ref mut idx) = rem.cmd_idx
                            && *idx >= insert_at
                        {
                            *idx += inserted_len;
                        }
                        for idx in &mut rem.pipeline {
                            if *idx >= insert_at {
                                *idx += inserted_len;
                            }
                        }
                    }
                    if let Some(ref mut fidx) = self.feeder_idx
                        && *fidx >= insert_at
                    {
                        *fidx += inserted_len;
                    }
                    self.shift_steps(insert_at, inserted_len);
                    let at = |idx: usize| idx + insert_at;
                    self.out
                        .sure_steps
                        .extend(sub.sure_steps.into_iter().map(at));
                    self.out
                        .plain_steps
                        .extend(sub.plain_steps.into_iter().map(at));
                    self.out.sure_steps.sort_unstable();
                    self.out.plain_steps.sort_unstable();
                }
            }
        }
    }

    /// Indices of commands at or after `at` move by `by` once a heredoc
    /// body's commands are spliced in before them.
    fn shift_steps(&mut self, at: usize, by: usize) {
        let shift = |idx: &mut usize| {
            if *idx >= at {
                *idx += by;
            }
        };
        self.out.sure_steps.iter_mut().for_each(shift);
        self.out.plain_steps.iter_mut().for_each(shift);
        self.groups.iter_mut().for_each(|(_, start)| shift(start));
        if let Some(start) = self.closed_group.as_mut() {
            shift(start);
        }
    }

    /// The bodies a feeder read after `pipe` copied it (`cat <<'EOF' |`,
    /// the body, then the consumer on a line of its own): every command
    /// handed that copy takes them, as `read_heredocs` gives them to a
    /// consumer on the `|` line. Not before `emit`: the bodies are split
    /// once the line ends, and a shell consumer would split them again,
    /// twice more at each level they nest.
    pub(super) fn give_late_bodies(&mut self, from: usize, copy: &Cmd, bodies: Vec<String>) {
        let mut cleared = false;
        let mut wrapped = None;
        for cmd in self.out.cmds.iter_mut().skip(from) {
            let env_cleared = cmd.env_cleared;
            let shell_wrapped = (SHELLS.contains(&cmd.name()) && !cmd.wrapped.is_empty())
                .then(|| cmd.wrapped.clone());
            if let Some(feeder) = cmd.feeder.as_deref_mut()
                && *feeder == *copy
            {
                feeder.heredocs.clone_from(&bodies);
                cleared |= env_cleared;
                wrapped = shell_wrapped.or(wrapped);
            }
        }
        // They run under the environment the consumer clears, in the
        // directory its wrapper (`env -C ..`, `sudo -D ..`) moves to.
        let late = bodies.get(copy.heredocs.len()..).unwrap_or(&[]);
        for item in &mut self.bodies {
            if late.contains(&item.body) {
                item.cleared |= cleared;
                if let Some(wrapped) = &wrapped {
                    item.wrapped.clone_from(wrapped);
                }
            }
        }
    }

    /// Heredoc bodies are commands unless proven to be data: a body can feed
    /// `bash`, and an unquoted one runs its own `$(…)`.
    pub(super) fn finish_bodies(&mut self) {
        let lone_cat = !self.failed
            && self.out.cmds.len() == self.first_cmd + 1
            && self.out.cmds.last().is_some_and(|c| {
                c.words == ["cat"] && c.assigns.is_empty() && c.redirects.is_empty()
            });
        for item in mem::take(&mut self.bodies) {
            if item.quoted && lone_cat {
                // `cat <<'EOF'` alone only prints. Inside a substitution the
                // consumer of that output decides; at statement level it is
                // just output.
                if let Some(defer) = self.defer.as_deref_mut() {
                    defer.push(item.body);
                }
            } else if item.quoted && item.is_interp {
                // Quoted heredoc that is an interpreter's program (it reads
                // its program from stdin): no shell runs it, and `interp_guard`
                // inspects `Cmd::heredocs` directly. As stdin of a program
                // given another way (`perl -e 'exec "sh"' <<'EOF'`) it may
                // reach a shell, and is split like any other body.
            } else {
                let outer_cleared = self.out.env_cleared;
                self.out.env_cleared |= item.cleared;
                self.out.enter_scope_wrapped(item.wrapped);
                lex(&item.body, self.depth + 1, self.out, None);
                self.out.exit_scope();
                self.out.env_cleared = outer_cleared;
            }
        }
    }
}
