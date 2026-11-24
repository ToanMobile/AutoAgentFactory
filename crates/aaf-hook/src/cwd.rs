//! Working-directory tracking across `env -C` / `sudo -D` wrappers, shell
//! directory builtins (`cd`, `cd -`, `pushd`, `popd`), and subshell boundaries.

use std::mem;
use std::path::PathBuf;

use crate::{Ctx, Denial, Line, fallback, heavy_guard, judge, paths, shell};

mod cdpath;
mod dirstack;

use cdpath::{cdpath, searched_dir};
use dirstack::apply_dir_builtin;

/// Extracts cumulative working-directory changes from `env -C`/`--chdir` and
/// `sudo -D`/`--chdir` options present in `wrapped`.
pub(crate) fn wrapped_cwd(wrapped: &[String], ctx: &Ctx) -> Option<PathBuf> {
    if wrapped.is_empty() {
        return None;
    }
    let mut cwd: Option<PathBuf> = None;
    let mut i = 0;
    let mut tool = "";
    while i < wrapped.len() {
        let word = wrapped[i].as_str();
        if !word.starts_with('-') || word == "-" {
            let base = shell::basename(word);
            let launcher = DIR_LAUNCHERS.contains(&tool);
            if base == "env" || base == "sudo" || DIR_LAUNCHERS.contains(&base) {
                tool = base;
            } else if launcher && matches!(word, "exec" | "x" | "dlx" | "run") {
                // The launcher's subcommand: its options still follow.
            } else if matches!(
                base,
                "command"
                    | "builtin"
                    | "nohup"
                    | "time"
                    | "noglob"
                    | "setsid"
                    | "unbuffer"
                    | "script"
                    | "exec"
                    | "doas"
                    | "nice"
                    | "timeout"
                    | "stdbuf"
                    | "ionice"
                    | "caffeinate"
                    | "xargs"
                    | "aaf"
                    | "xcrun"
                    | "arch"
                    | "bundle"
                    | "rbenv"
                    | "npm"
                    | "npx"
                    | "bunx"
                    | "nocorrect"
                    | "bun"
                    | "uvx"
                    | "pipenv"
                    | "pdm"
                    | "pipx"
                    | "nix"
                    | "nix-shell"
                    | "direnv"
                    | "gtime"
                    | "pixi"
                    | "hatch"
                    | "rye"
                    | "devbox"
                    | "op"
                    | "doppler"
                    | "dotenvx"
                    | "asdf"
                    | "chronic"
                    | "flock"
                    | "taskset"
            ) {
                tool = "";
            }
            i += 1;
            continue;
        }
        match tool {
            "env" => {
                if let Some(rest) = word.strip_prefix("--chdir=") {
                    if !rest.is_empty() {
                        let base = cwd.as_deref().unwrap_or(&ctx.cwd);
                        cwd = Some(paths::expand_real_dotdot(
                            rest,
                            base,
                            ctx.env.home.as_deref(),
                        ));
                    }
                    i += 1;
                } else if word == "--chdir" {
                    if let Some(dir) = wrapped.get(i + 1) {
                        let base = cwd.as_deref().unwrap_or(&ctx.cwd);
                        cwd = Some(paths::expand_real_dotdot(
                            dir,
                            base,
                            ctx.env.home.as_deref(),
                        ));
                    }
                    i += 2;
                } else if matches!(word, "--unset" | "--split-string") {
                    i += 2;
                } else if word.starts_with("--") {
                    i += 1;
                } else if let Some(at) = word[1..].find(['u', 'C', 'P', 'S']) {
                    let pos = 1 + at;
                    let flag = word.as_bytes()[pos];
                    let rest = &word[pos + 1..];
                    if flag == b'C' {
                        let dir = if rest.is_empty() {
                            wrapped.get(i + 1).map(String::as_str)
                        } else {
                            Some(rest)
                        };
                        if let Some(dir) = dir {
                            let base = cwd.as_deref().unwrap_or(&ctx.cwd);
                            cwd = Some(paths::expand_real_dotdot(
                                dir,
                                base,
                                ctx.env.home.as_deref(),
                            ));
                        }
                    }
                    i += if rest.is_empty() { 2 } else { 1 };
                } else {
                    i += 1;
                }
            }
            "sudo" => {
                if let Some(rest) = word.strip_prefix("--chdir=") {
                    if !rest.is_empty() {
                        let base = cwd.as_deref().unwrap_or(&ctx.cwd);
                        cwd = Some(paths::expand_real_dotdot(
                            rest,
                            base,
                            ctx.env.home.as_deref(),
                        ));
                    }
                    i += 1;
                } else if word == "--chdir" {
                    if let Some(dir) = wrapped.get(i + 1) {
                        let base = cwd.as_deref().unwrap_or(&ctx.cwd);
                        cwd = Some(paths::expand_real_dotdot(
                            dir,
                            base,
                            ctx.env.home.as_deref(),
                        ));
                    }
                    i += 2;
                } else if matches!(
                    word,
                    "--user"
                        | "--group"
                        | "--prompt"
                        | "--close-from"
                        | "--chroot"
                        | "--host"
                        | "--role"
                        | "--type"
                        | "--other-user"
                        | "--command-timeout"
                ) {
                    i += 2;
                } else if word.starts_with("--") {
                    i += 1;
                } else if let Some(at) =
                    word[1..].find(['u', 'g', 'p', 'C', 'h', 'D', 'R', 'T', 'U'])
                {
                    let pos = 1 + at;
                    let flag = word.as_bytes()[pos];
                    let rest = &word[pos + 1..];
                    if flag == b'D' {
                        let dir = if rest.is_empty() {
                            wrapped.get(i + 1).map(String::as_str)
                        } else {
                            Some(rest)
                        };
                        if let Some(dir) = dir {
                            let base = cwd.as_deref().unwrap_or(&ctx.cwd);
                            cwd = Some(paths::expand_real_dotdot(
                                dir,
                                base,
                                ctx.env.home.as_deref(),
                            ));
                        }
                    }
                    i += if rest.is_empty() { 2 } else { 1 };
                } else {
                    i += 1;
                }
            }
            // Launchers that run their command elsewhere: `mise -C`/`--cd`,
            // `pnpm -C`/`--dir`, `yarn --cwd`, `uv --directory`, `poetry -C`,
            // `conda run --cwd`.
            launcher if DIR_LAUNCHERS.contains(&launcher) => {
                let options: &[&str] = match tool {
                    "mise" => &["-C", "--cd"],
                    "pnpm" => &["-C", "--dir"],
                    "uv" => &["--directory"],
                    "poetry" => &["-C", "--directory"],
                    // `yarn --cwd`, `conda run --cwd`.
                    _ => &["--cwd"],
                };
                let (flag, glued) = match word.split_once('=') {
                    Some((flag, glued)) => (flag, Some(glued)),
                    // `mise -C..`: the directory glued to the letter.
                    None if tool == "mise" && word.len() > 2 && word.starts_with("-C") => {
                        ("-C", Some(&word[2..]))
                    }
                    None => (word, None),
                };
                if options.contains(&flag) {
                    let dir = glued.or_else(|| wrapped.get(i + 1).map(String::as_str));
                    if let Some(dir) = dir.filter(|dir| !dir.is_empty()) {
                        let base = cwd.as_deref().unwrap_or(&ctx.cwd);
                        // Like `env -C`: the launcher `chdir`s, so `..` after
                        // a symlinked directory is resolved physically.
                        cwd = Some(paths::expand_real_dotdot(
                            dir,
                            base,
                            ctx.env.home.as_deref(),
                        ));
                    }
                    i += if glued.is_some() { 1 } else { 2 };
                } else {
                    i += 1;
                }
            }
            _ => {
                i += 1;
            }
        }
    }
    cwd
}

/// Launchers with an option that moves the command they run.
const DIR_LAUNCHERS: &[&str] = &[
    "mise",
    "pnpm",
    "yarn",
    "uv",
    "poetry",
    "conda",
    "mamba",
    "micromamba",
];

/// Where the shell may stand: its working directory, `cd -`'s target and
/// the `pushd` stack.
#[derive(Clone, PartialEq)]
struct Place {
    cwd: PathBuf,
    old: Option<PathBuf>,
    stack: Vec<PathBuf>,
}

/// Bound on the places one line is judged in; past it, places that differ
/// only in `cd -`/`popd` history are merged, and past that the line is refused.
const MAX_PLACES: usize = 32;

fn is_dir_builtin(cmd: &shell::Cmd) -> bool {
    matches!(cmd.name(), "cd" | "pushd" | "popd")
}

/// Wrappers that run a builtin in the current shell; any other (`env`,
/// `nice`, `timeout`, `xargs`, `sudo`, a launcher) starts a process whose
/// `cd` moves nothing here.
const IN_SHELL_WRAPPERS: &[&str] = &["command", "builtin", "time", "noglob", "nocorrect", "-"];

fn runs_elsewhere(cmd: &shell::Cmd) -> bool {
    cmd.wrapped
        .iter()
        .any(|word| !word.starts_with('-') && !IN_SHELL_WRAPPERS.contains(&shell::basename(word)))
}

/// Evaluates each command in `script` in order, tracking subshell/wrapper
/// scope boundaries and directory-changing builtins (`cd`, `cd -`, `pushd`,
/// `popd`), and invokes `judge_cmd` on every command in every place the
/// shell may stand. A move is followed for sure only when the next command
/// runs because it succeeded in this shell (`cd dir && …`), and only up to
/// the end of that `&&` chain: past a `;`, a newline, `||`, a pipe or `&`
/// the `cd` may have failed, so the places it left count again. Otherwise
/// what follows is judged both where the shell was and where it may be. A
/// line that defines a function named like a directory builtin has no sure
/// move at all.
pub(crate) fn eval_script<'a, F>(
    script: &shell::Script,
    mut ctx: Ctx<'a>,
    mut judge_cmd: F,
) -> Option<Denial>
where
    F: FnMut(&shell::Cmd, &Ctx<'a>) -> Option<Denial>,
{
    let mut places = vec![Place {
        cwd: ctx.cwd.clone(),
        old: None,
        stack: Vec::new(),
    }];
    // The places a sure `cd` of the current `&&` chain left behind.
    let mut left: Vec<Place> = Vec::new();
    let mut scope_stack: Vec<(Vec<Place>, Vec<Place>)> = Vec::new();
    let mut scope_at = 0;
    let redefined = script
        .functions
        .iter()
        .any(|name| matches!(name.as_str(), "cd" | "pushd" | "popd"));
    let search = cdpath(script);
    for (idx, cmd) in script.cmds.iter().enumerate() {
        while let Some((at, event)) = script.scopes.get(scope_at) {
            if *at > idx {
                break;
            }
            scope_at += 1;
            match event {
                shell::ScopeEvent::Enter(wrapped) => {
                    scope_stack.push((places.clone(), mem::take(&mut left)));
                    // A wrapper (`env -C dir sh -c …`) moves the scope it runs.
                    for place in &mut places {
                        ctx.set_cwd(place.cwd.clone());
                        if let Some(next) = wrapped_cwd(wrapped, &ctx) {
                            place.cwd = next;
                        }
                    }
                }
                shell::ScopeEvent::Exit => {
                    if let Some(saved) = scope_stack.pop() {
                        (places, left) = saved;
                    }
                }
            }
        }
        for place in &places {
            ctx.set_cwd(place.cwd.clone());
            if let Some(denial) = judge_cmd(cmd, &ctx) {
                return Some(denial);
            }
        }
        let sure = script.sure_steps.contains(&idx);
        if is_dir_builtin(cmd) && !runs_elsewhere(cmd) {
            let search = search.at(idx);
            let searched = searched_dir(cmd).filter(|_| search.is_some());
            let entries = match (&search, searched) {
                (Some(None), Some(_)) => {
                    return Some(Denial::new(
                        fallback::RULE,
                        "cannot analyse a cd that CDPATH may send to an unknown directory",
                        "cd to ./<dir> or an absolute path, or give CDPATH a literal value",
                    ));
                }
                (Some(Some(entries)), Some(_)) => entries.as_slice(),
                _ => &[],
            };
            let mut moved = Vec::with_capacity(places.len());
            for place in &places {
                let mut next = place.clone();
                ctx.set_cwd(next.cwd.clone());
                apply_dir_builtin(cmd, &mut ctx, &mut next.old, &mut next.stack);
                next.cwd = ctx.cwd.clone();
                // For each value the search path may have, each directory
                // the name may be found in, up to the first where it is:
                // `cd` stops there.
                for value in entries {
                    for entry in value {
                        let base = match entry.as_str() {
                            "" => place.cwd.clone(),
                            entry => paths::expand(entry, &place.cwd, ctx.env.home.as_deref()),
                        };
                        let mut found = next.clone();
                        found.cwd = paths::normalize(&base.join(searched.unwrap_or_default()));
                        let there = found.cwd.is_dir();
                        moved.push(found);
                        if there {
                            break;
                        }
                    }
                }
                moved.push(next);
            }
            if sure && !redefined {
                left.append(&mut places);
            } else {
                moved.splice(0..0, places);
            }
            places = moved;
        }
        if !sure {
            places.append(&mut left);
        }
        places = merge(places);
        left = merge(left);
        if places.len() > MAX_PLACES || left.len() > MAX_PLACES {
            return Some(Denial::new(
                fallback::RULE,
                "cannot analyse a command that may change directory in too many ways",
                "split the command into separate calls, joining each cd to what follows with &&",
            ));
        }
    }
    None
}

/// Evaluates a full shell command line: its opaque parts, heavy commands,
/// then every command through [`eval_script`].
pub(crate) fn bash(command: &str, mut ctx: Ctx) -> Option<Denial> {
    let script = shell::parse(command);
    ctx.line = Line::of(&script);
    for text in &script.opaque {
        if let Some(denial) = fallback::check(text, &ctx) {
            return Some(denial);
        }
    }
    if let Some(denial) = heavy_guard::check_script(&script.cmds) {
        return Some(denial);
    }
    eval_script(&script, ctx, judge)
}

/// Drops duplicate places; when there are too many, keeps one per directory.
fn merge(places: Vec<Place>) -> Vec<Place> {
    let mut out: Vec<Place> = Vec::with_capacity(places.len());
    for place in places {
        if !out.contains(&place) {
            out.push(place);
        }
    }
    if out.len() > MAX_PLACES {
        let mut by_dir: Vec<Place> = Vec::new();
        for place in out {
            if !by_dir.iter().any(|kept| kept.cwd == place.cwd) {
                by_dir.push(place);
            }
        }
        return by_dir;
    }
    out
}
