//! Working-directory tracking across `env -C` / `sudo -D` wrappers, shell
//! directory builtins (`cd`, `cd -`, `pushd`, `popd`), and subshell boundaries.

use std::path::PathBuf;

use crate::{Ctx, Denial, Line, fallback, heavy_guard, judge, paths, shell};

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
            if matches!(base, "env" | "sudo") {
                tool = base;
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
            _ => {
                i += 1;
            }
        }
    }
    cwd
}

/// Parses a `pushd`/`popd` stack offset: `+N` (from top/left, `true`) or `-N`
/// (from bottom/right, `false`).
fn parse_stack_offset(arg: &str) -> Option<(bool, usize)> {
    let (from_left, digits) = if let Some(rest) = arg.strip_prefix('+') {
        (true, rest)
    } else {
        let rest = arg.strip_prefix('-')?;
        (false, rest)
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<usize>().ok().map(|n| (from_left, n))
}

fn resolve_stack_idx(from_left: bool, n: usize, total: usize) -> Option<usize> {
    (n < total).then(|| if from_left { n } else { total - 1 - n })
}

fn apply_dir_builtin(
    cmd: &shell::Cmd,
    ctx: &mut Ctx,
    old_cwd: &mut Option<PathBuf>,
    dir_stack: &mut Vec<PathBuf>,
) {
    match cmd.name() {
        "cd" => {
            let mut after_dd = false;
            let mut dash_only = false;
            let mut physical = false;
            let mut target = None;
            for arg in cmd.args() {
                if after_dd {
                    target = Some(arg.as_str());
                    break;
                }
                if arg == "--" {
                    after_dd = true;
                } else if arg == "-" {
                    dash_only = true;
                    break;
                } else if let Some(flags) = arg.strip_prefix('-') {
                    for ch in flags.chars() {
                        match ch {
                            'P' => physical = true,
                            'L' => physical = false,
                            _ => {}
                        }
                    }
                } else {
                    target = Some(arg.as_str());
                    break;
                }
            }
            if let Some(dir) = target {
                let next = if physical {
                    paths::real(&ctx.expand_real_dotdot(dir))
                } else {
                    ctx.expand(dir)
                };
                *old_cwd = Some(ctx.cwd.clone());
                ctx.set_cwd(next);
            } else if dash_only {
                if let Some(prev) = old_cwd.take() {
                    let next = if physical { paths::real(&prev) } else { prev };
                    *old_cwd = Some(ctx.cwd.clone());
                    ctx.set_cwd(next);
                }
            } else if let Some(home) = ctx.env.home.as_deref() {
                let next = if physical {
                    paths::real(home)
                } else {
                    paths::normalize(home)
                };
                *old_cwd = Some(ctx.cwd.clone());
                ctx.set_cwd(next);
            }
        }
        "pushd" => {
            let mut after_dd = false;
            let mut no_cd = false;
            let mut target = None;
            let mut offset = None;
            for arg in cmd.args() {
                if after_dd {
                    target = Some(arg.as_str());
                    break;
                }
                if arg == "--" {
                    after_dd = true;
                } else if arg == "-n" {
                    no_cd = true;
                } else if let Some(off) = parse_stack_offset(arg) {
                    offset = Some(off);
                    break;
                } else if !arg.starts_with('-') {
                    target = Some(arg.as_str());
                    break;
                }
            }
            if let Some((from_left, n)) = offset {
                let mut view = Vec::with_capacity(dir_stack.len() + 1);
                view.push(ctx.cwd.clone());
                view.extend(dir_stack.iter().rev().cloned());
                if let Some(k) = resolve_stack_idx(from_left, n, view.len()) {
                    view.rotate_left(k);
                    if !no_cd && let Some(new_cwd) = view.first().cloned() {
                        *old_cwd = Some(ctx.cwd.clone());
                        ctx.set_cwd(new_cwd);
                    }
                    dir_stack.clear();
                    dir_stack.extend(view.into_iter().skip(1).rev());
                }
            } else if let Some(dir) = target {
                let next = ctx.expand(dir);
                if no_cd {
                    dir_stack.push(next);
                } else {
                    dir_stack.push(ctx.cwd.clone());
                    *old_cwd = Some(ctx.cwd.clone());
                    ctx.set_cwd(next);
                }
            } else if !no_cd && let Some(top) = dir_stack.pop() {
                dir_stack.push(ctx.cwd.clone());
                *old_cwd = Some(ctx.cwd.clone());
                ctx.set_cwd(top);
            }
        }
        "popd" => {
            let mut after_dd = false;
            let mut no_cd = false;
            let mut offset = None;
            for arg in cmd.args() {
                if after_dd {
                    if let Some(off) = parse_stack_offset(arg) {
                        offset = Some(off);
                    }
                    break;
                }
                if arg == "--" {
                    after_dd = true;
                } else if arg == "-n" {
                    no_cd = true;
                } else if let Some(off) = parse_stack_offset(arg) {
                    offset = Some(off);
                    break;
                }
            }
            if dir_stack.is_empty() {
                return;
            }
            let (from_left, n) = offset.unwrap_or((true, 0));
            let mut view = Vec::with_capacity(dir_stack.len() + 1);
            view.push(ctx.cwd.clone());
            view.extend(dir_stack.iter().rev().cloned());
            if let Some(k) = resolve_stack_idx(from_left, n, view.len()) {
                if k == 0 {
                    if no_cd {
                        view.remove(1);
                    } else {
                        view.remove(0);
                        if let Some(new_cwd) = view.first().cloned() {
                            *old_cwd = Some(ctx.cwd.clone());
                            ctx.set_cwd(new_cwd);
                        }
                    }
                } else {
                    view.remove(k);
                }
                dir_stack.clear();
                dir_stack.extend(view.into_iter().skip(1).rev());
            }
        }
        _ => {}
    }
}

/// Evaluates each command in `script` in chronological order while tracking
/// subshell/wrapper scope boundaries and directory builtins (`cd`, `pushd`,
/// `popd`), invoking `judge_cmd` on every command with the active `Ctx`.
pub(crate) fn eval_script<'a, F>(
    script: &shell::Script,
    mut ctx: Ctx<'a>,
    mut judge_cmd: F,
) -> Option<Denial>
where
    F: FnMut(&shell::Cmd, &Ctx<'a>) -> Option<Denial>,
{
    let mut old_cwd: Option<PathBuf> = None;
    let mut dir_stack: Vec<PathBuf> = Vec::new();
    let mut scope_stack: Vec<(PathBuf, Option<PathBuf>, Vec<PathBuf>)> = Vec::new();
    let mut scope_at = 0;
    for (idx, cmd) in script.cmds.iter().enumerate() {
        while let Some((at, event)) = script.scopes.get(scope_at) {
            if *at > idx {
                break;
            }
            scope_at += 1;
            match event {
                shell::ScopeEvent::Enter(wrapped) => {
                    scope_stack.push((ctx.cwd.clone(), old_cwd.clone(), dir_stack.clone()));
                    if let Some(next) = wrapped_cwd(wrapped, &ctx) {
                        ctx.set_cwd(next);
                    }
                }
                shell::ScopeEvent::Exit => {
                    if let Some((saved_cwd, saved_old, saved_dirs)) = scope_stack.pop() {
                        ctx.set_cwd(saved_cwd);
                        old_cwd = saved_old;
                        dir_stack = saved_dirs;
                    }
                }
            }
        }
        if let Some(denial) = judge_cmd(cmd, &ctx) {
            return Some(denial);
        }
        apply_dir_builtin(cmd, &mut ctx, &mut old_cwd, &mut dir_stack);
    }
    None
}

/// Evaluates a full shell command line, tracking subshell scope boundaries and
/// directory-changing builtins (`cd`, `cd -`, `pushd`, `popd`).
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
