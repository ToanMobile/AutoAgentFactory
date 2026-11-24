//! Launchers that run the program named in their arguments (`xcrun`,
//! `arch`, `bundle exec`, `mise exec`, `npm exec`, `npx`, `uv run`,
//! `conda run`, `nix develop -c`, `pixi run`/`exec`, `hatch env run`,
//! `op run --`, `op plugin run --`, `flock <file>`,
//! `taskset <mask>`, …): stripped like any wrapper, so every guard judges
//! the program they run. `watch` hands its words to a shell as one line.

use super::{Cmd, Lexer, MAX_DEPTH, Redirect, Tok, lex};

/// Programs [`Lexer::launcher_len`] knows.
pub(super) const LAUNCHERS: &[&str] = &[
    "xcrun",
    "arch",
    "bundle",
    "rbenv",
    "mise",
    "npm",
    "pnpm",
    "yarn",
    "npx",
    "bunx",
    "bun",
    "uv",
    "uvx",
    "poetry",
    "pipenv",
    "pdm",
    "pipx",
    "conda",
    "mamba",
    "micromamba",
    "nix",
    "nix-shell",
    "direnv",
    "pixi",
    "hatch",
    "rye",
    "devbox",
    "op",
    "doppler",
    "dotenvx",
    "asdf",
    "chronic",
    "flock",
    "taskset",
    "watch",
];

/// The subcommands that launch a program; empty when the launcher takes
/// the program directly.
fn subcommands(name: &str) -> &'static [&'static str] {
    match name {
        "bundle" | "rbenv" | "mise" => &["exec", "x"],
        "npm" => &["exec", "x"],
        "yarn" | "pnpm" => &["exec", "dlx", "run"],
        "bun" => &["x"],
        "uv" => &["run", "tool"],
        "poetry" | "pipenv" | "pdm" | "pipx" | "conda" | "mamba" | "micromamba" => &["run"],
        "nix" => &["develop", "shell", "run"],
        "direnv" | "asdf" => &["exec"],
        "pixi" => &["run", "exec"],
        "hatch" => &["run", "env"],
        "op" => &["run", "plugin"],
        "rye" | "devbox" | "doppler" | "dotenvx" => &["run"],
        _ => &[],
    }
}

/// Options that take the next word as their value.
fn valued(name: &str) -> &'static [&'static str] {
    match name {
        "xcrun" => &["-sdk", "--sdk", "-toolchain", "--toolchain"],
        "arch" => &["-arch", "-e", "-d"],
        "mise" => &[
            "-C",
            "--cd",
            "-j",
            "--jobs",
            "-E",
            "--env",
            "-P",
            "--profile",
            "--log-level",
            "--raw",
        ],
        "npm" | "pnpm" | "yarn" | "npx" | "bunx" | "bun" => &[
            "-p",
            "--package",
            "--prefix",
            "-w",
            "--workspace",
            "-C",
            "--dir",
            "--cwd",
            "--filter",
            "-F",
            // `yarn workspaces foreach --include <glob> -j <n> run …`.
            "--include",
            "--exclude",
            "--from",
            "-j",
            "--jobs",
        ],
        "uv" | "uvx" => &[
            "-p",
            "--python",
            "-w",
            "--with",
            "--with-editable",
            "--with-requirements",
            "--from",
            "--project",
            "--directory",
            "--package",
            "--extra",
            "--group",
            "--only-group",
            "--env-file",
            "--index",
            "--index-url",
            "--default-index",
            "--extra-index-url",
            "--cache-dir",
            "--config-file",
            "--color",
        ],
        "poetry" => &["-C", "--directory", "-P", "--project"],
        "pdm" => &["-p", "--project", "--venv"],
        "pipx" => &["--spec", "--python", "--pip-args"],
        "conda" | "mamba" | "micromamba" => &["-n", "--name", "-p", "--prefix", "--cwd"],
        "pixi" => &[
            "-e",
            "--environment",
            "--manifest-path",
            "-s",
            "--spec",
            "-c",
            "--channel",
            "-p",
            "--platform",
        ],
        "hatch" => &["-e", "--env", "-p", "--project"],
        "rye" => &["--pyproject"],
        "devbox" => &["-c", "--config", "-e", "--env", "--env-file"],
        "op" => &[
            "--env-file",
            "--environment",
            "--account",
            "--config",
            "--session",
        ],
        "doppler" => &["-p", "--project", "-c", "--config", "--token", "--mount"],
        "dotenvx" => &["-f", "--env-file", "-fk", "--env-keys-file", "-e", "--env"],
        "flock" => &["-w", "--timeout", "--wait", "-E", "--conflict-exit-code"],
        "watch" => &["-n", "--interval", "-q", "--equexit"],
        _ => &[],
    }
}

/// Launchers whose unknown `--option` may take the next word as its value.
fn guesses_values(name: &str) -> bool {
    !matches!(name, "xcrun" | "arch" | "nix-shell" | "direnv")
}

/// An option that carries a shell command line: `npx -c '<line>'`,
/// `npm exec --call '<line>'`, `mise exec --command '<line>'`,
/// `nix-shell --run '<line>'`. Returns the part glued to it (`--call=…`),
/// empty when the line is the next word.
fn carries_line<'a>(name: &str, text: &'a str) -> Option<&'a str> {
    let long = match name {
        // Its `-c` is `--config`.
        "doppler" | "flock" => {
            let rest = text.strip_prefix("--command")?;
            return match rest.strip_prefix('=') {
                Some(glued) => Some(glued),
                None => rest.is_empty().then_some(""),
            };
        }
        "mise" => "--command",
        "npm" | "pnpm" | "yarn" | "npx" | "bunx" => "--call",
        "nix-shell" => return matches!(text, "--run" | "--command").then_some(""),
        _ => return None,
    };
    if text == "-c" {
        return Some("");
    }
    let rest = text.strip_prefix(long)?;
    match rest.strip_prefix('=') {
        Some(glued) => Some(glued),
        None => rest.is_empty().then_some(""),
    }
}

impl Lexer<'_, '_> {
    /// For a launcher `name`: the number of words before the program it
    /// runs, `None` when it runs none (`xcrun -f clang`, `npm install`,
    /// bare `arch`). A command line it hands a shell is split here, in a
    /// scope of its own, and the launcher stays the command.
    pub(super) fn launcher_len(
        &mut self,
        name: &str,
        words: &[Tok],
        redirects: &[Redirect],
        feeder: Option<&Cmd>,
    ) -> Option<usize> {
        match name {
            "flock" => return self.flock_len(words),
            "taskset" => return taskset_len(words),
            "watch" => return self.watch_len(words),
            _ => {}
        }
        let subs = subcommands(name);
        let valued = valued(name);
        // The subcommand seen so far; `""` for a launcher that has none.
        let mut sub = (subs.is_empty() && name != "nix-shell").then_some("");
        let mut dashes = false;
        let mut at = 1;
        while let Some(tok) = words.get(at) {
            let text = tok.text.as_str();
            if !dashes && let Some(glued) = carries_line(name, text) {
                let line = match glued {
                    "" => words.get(at + 1).map(|next| next.text.as_str()),
                    glued => Some(glued),
                };
                if let Some(line) = line {
                    self.shell_line(line);
                }
                return Some(words.len());
            }
            if name == "xcrun"
                && (matches!(text, "-f" | "--find" | "-h" | "--help" | "--version")
                    || text.starts_with("--show-"))
            {
                return None;
            }
            // `nix develop -c <program>`, `nix shell <pkgs> --command <program>`.
            if matches!(sub, Some("develop" | "shell")) {
                if matches!(text, "-c" | "--command") {
                    return (at + 1 < words.len()).then_some(at + 1);
                }
                at += 1;
                continue;
            }
            if !dashes && text == "--" {
                // Before the subcommand `--` ends the launcher's own options.
                sub?;
                dashes = true;
                at += 1;
                continue;
            }
            if !dashes && text.starts_with('-') && text.len() > 1 {
                let known = valued.contains(&text);
                let next = words.get(at + 1).filter(|next| !next.text.starts_with('-'));
                if !known
                    && text.starts_with("--")
                    && !text.contains('=')
                    && next.is_some()
                    && guesses_values(name)
                {
                    // `pnpm --loglevel error exec …`: the word after an
                    // unknown option may be its value; judge that reading too.
                    let mut other = words[..at].to_vec();
                    other.extend_from_slice(&words[at + 2..]);
                    self.other_reading(other, redirects, feeder);
                }
                at += if known { 2 } else { 1 };
                continue;
            }
            let Some(seen) = sub else {
                if name == "nix-shell" {
                    // Its operands are files; only `--run` runs anything.
                    at += 1;
                    continue;
                }
                // `yarn workspace <name> exec …`, `yarn workspaces foreach
                // [options] run …`.
                if name == "yarn"
                    && (text == "workspace"
                        || (text == "workspaces"
                            && words.get(at + 1).is_some_and(|next| next.text == "foreach")))
                {
                    at += 2;
                    continue;
                }
                if !subs.contains(&text) {
                    return None;
                }
                // `uv tool run <program>`, `hatch env run`, `op plugin run`.
                if matches!(text, "tool" | "env" | "plugin") {
                    if words.get(at + 1).map(|next| next.text.as_str()) != Some("run") {
                        return None;
                    }
                    at += 1;
                }
                // `direnv exec <dir> <program>`.
                if name == "direnv" {
                    at += 1;
                }
                sub = Some(text);
                at += 1;
                continue;
            };
            if seen == "run" && name == "nix" {
                self.nix_run(&words[at..], redirects, feeder);
                return None;
            }
            // `hatch run <env>:<program>`: the program after the env name.
            if name == "hatch"
                && seen == "run"
                && let Some((env, program)) = text.split_once(':')
                && !env.is_empty()
                && !program.is_empty()
                && !env.contains('/')
            {
                let mut words = words[at..].to_vec();
                words[0].text = program.to_string();
                self.other_reading(words, redirects, feeder);
                return None;
            }
            // `mise exec node@20 -- gradle`: tool specs come before the `--`.
            if name == "mise" && !dashes && text.contains('@') {
                at += 1;
                continue;
            }
            return Some(at);
        }
        None
    }

    /// Splits `line` as a command line run by a shell of its own.
    fn shell_line(&mut self, line: &str) {
        self.out.enter_scope();
        lex(line, self.depth + 1, self.out, None);
        self.out.exit_scope();
    }

    /// `flock [options] <file> <program>`, or `-c '<line>'` after the file;
    /// `flock <fd>` alone runs nothing.
    fn flock_len(&mut self, words: &[Tok]) -> Option<usize> {
        let valued = valued("flock");
        let mut file = false;
        let mut at = 1;
        while let Some(text) = words.get(at).map(|tok| tok.text.as_str()) {
            if let Some(glued) = carries_line("flock", text).or((text == "-c").then_some("")) {
                let line = match glued {
                    "" => words.get(at + 1).map(|next| next.text.as_str()),
                    glued => Some(glued),
                };
                if let Some(line) = line {
                    self.shell_line(line);
                }
                return Some(words.len());
            }
            if text.starts_with('-') && text.len() > 1 {
                at += if valued.contains(&text) { 2 } else { 1 };
            } else if !file {
                file = true;
                at += 1;
            } else {
                return Some(at);
            }
        }
        None
    }

    /// `watch [options] <words…>`: the words are joined into one line for
    /// `sh -c`, or run as they are under `-x`.
    fn watch_len(&mut self, words: &[Tok]) -> Option<usize> {
        let valued = valued("watch");
        let mut exec = false;
        let mut at = 1;
        while let Some(text) = words.get(at).map(|tok| tok.text.as_str()) {
            if text == "--" {
                at += 1;
                break;
            }
            if !text.starts_with('-') || text.len() == 1 {
                break;
            }
            let takes_value = if text.starts_with("--") {
                valued.contains(&text)
            } else {
                text.find(['n', 'q']) == Some(text.len() - 1)
            };
            exec |= text == "--exec"
                || (!text.starts_with("--")
                    && text[1..]
                        .chars()
                        .take_while(|c| !matches!(c, 'n' | 'q'))
                        .any(|c| c == 'x'));
            at += if takes_value { 2 } else { 1 };
        }
        if at >= words.len() {
            return None;
        }
        if exec {
            return Some(at);
        }
        let line: Vec<&str> = words[at..].iter().map(|tok| tok.text.as_str()).collect();
        self.shell_line(&line.join(" "));
        Some(words.len())
    }

    /// Judges `words` as one more command, in a scope of its own, while the
    /// launcher line itself is judged as read the other way.
    fn other_reading(&mut self, words: Vec<Tok>, redirects: &[Redirect], feeder: Option<&Cmd>) {
        if self.depth >= MAX_DEPTH {
            let text: Vec<&str> = words.iter().map(|tok| tok.text.as_str()).collect();
            self.out.opaque.push(text.join(" "));
            return;
        }
        self.depth += 1;
        self.out.enter_scope();
        self.emit(
            Vec::new(),
            words,
            redirects.to_vec(),
            feeder.cloned().map(Box::new),
        );
        self.out.exit_scope();
        self.depth -= 1;
    }

    /// `nix run <flake>#<attr> [--] <args>`: runs the program the attribute
    /// is named after (`nixpkgs#bash -- -c '…'` is `bash -c '…'`).
    fn nix_run(&mut self, rest: &[Tok], redirects: &[Redirect], feeder: Option<&Cmd>) {
        let Some((installable, args)) = rest.split_first() else {
            return;
        };
        let Some((_, attr)) = installable.text.split_once('#') else {
            return;
        };
        let program = attr.rsplit('.').next().unwrap_or(attr);
        if program.is_empty() {
            return;
        }
        let args = match args.first() {
            Some(first) if first.text == "--" => &args[1..],
            _ => args,
        };
        let mut words = vec![Tok {
            text: program.to_string(),
            ..installable.clone()
        }];
        words.extend_from_slice(args);
        self.other_reading(words, redirects, feeder);
    }
}

/// `taskset [options] <mask> <program>`; with `-p` it acts on a running
/// process and runs nothing.
fn taskset_len(words: &[Tok]) -> Option<usize> {
    let mut at = 1;
    while let Some(text) = words.get(at).map(|tok| tok.text.as_str()) {
        if text == "--pid"
            || (text.starts_with('-') && !text.starts_with("--") && text.contains('p'))
        {
            return None;
        }
        if !text.starts_with('-') || text.len() == 1 {
            break;
        }
        at += 1;
    }
    // The mask or the CPU list.
    at += 1;
    (at < words.len()).then_some(at)
}
