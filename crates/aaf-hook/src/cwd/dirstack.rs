//! Directory builtins: `cd`, `pushd`, `popd` and their stack offsets.

use std::path::PathBuf;

use crate::{Ctx, paths, shell};

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

pub(super) fn apply_dir_builtin(
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
