//! Tracks shell variables bound directly to safe `mktemp -d` calls within the
//! current script scope so `rm -rf "$TMP"` can be allowed without opening up
//! arbitrary variable-expanded recursive deletes.

use crate::{Ctx, paths, shell};

fn is_safe_mktemp_dir_arg(val: &str) -> bool {
    !val.contains("..")
        && !paths::has_unresolved_var(val)
        && (val.starts_with("/tmp/")
            || val.starts_with("/private/tmp/")
            || val.starts_with("$TMPDIR/")
            || val.starts_with("${TMPDIR}/"))
}

fn is_safe_mktemp_dir_cmd(cmd: &shell::Cmd) -> bool {
    if cmd.dynamic_name || cmd.name() != "mktemp" || !cmd.assigns.is_empty() {
        return false;
    }
    let mut has_dir_flag = false;
    let mut after_dd = false;
    let mut i = 0;
    let args = cmd.args();
    while i < args.len() {
        let arg = args[i].as_str();
        if !after_dd && arg == "--" {
            after_dd = true;
            i += 1;
            continue;
        }
        if !after_dd && arg.starts_with("--") {
            if arg == "--directory" {
                has_dir_flag = true;
            } else if arg == "--quiet" || arg == "--dry-run" {
                // harmless
            } else if let Some(val) = arg.strip_prefix("--tmpdir=") {
                if !val.is_empty() && !is_safe_mktemp_dir_arg(val) {
                    return false;
                }
            } else if arg == "--tmpdir" {
                // optional value only when glued with `=`; bare `--tmpdir` uses TMPDIR
            } else if arg.starts_with("--suffix=") {
                if arg.contains('/') || arg.contains("..") {
                    return false;
                }
            } else {
                return false;
            }
            i += 1;
            continue;
        }
        if !after_dd && arg.starts_with('-') && arg.len() > 1 {
            let mut chars = arg[1..].chars();
            while let Some(c) = chars.next() {
                match c {
                    'd' => has_dir_flag = true,
                    'q' | 'u' => {}
                    't' => {
                        let rest = chars.as_str();
                        let val = if rest.is_empty() {
                            i += 1;
                            args.get(i).map_or("", String::as_str)
                        } else {
                            rest
                        };
                        if val.contains('/') || val.contains("..") {
                            return false;
                        }
                        break;
                    }
                    'p' => {
                        let rest = chars.as_str();
                        let val = if rest.is_empty() {
                            i += 1;
                            args.get(i).map_or("", String::as_str)
                        } else {
                            rest
                        };
                        if !is_safe_mktemp_dir_arg(val) {
                            return false;
                        }
                        break;
                    }
                    _ => return false,
                }
            }
            i += 1;
            continue;
        }
        if !is_safe_mktemp_dir_arg(arg) {
            return false;
        }
        i += 1;
    }
    has_dir_flag
}

/// Returns the variable name if `script.cmds[idx]` is a direct single-variable
/// assignment `VAR=$(mktemp -d ...)` (or `local`/`export`/`readonly`/`declare`
/// form) fed by a single-command substitution scope immediately preceding `idx`.
fn assigned_mktemp_var(script: &shell::Script, idx: usize) -> Option<&str> {
    if !script.opaque.is_empty() || idx == 0 {
        return None;
    }
    let cmd = &script.cmds[idx];
    let var = if cmd.words.is_empty() && cmd.assigns.len() == 1 && cmd.assigns[0].1 == "$" {
        cmd.assigns[0].0.as_str()
    } else if matches!(
        cmd.name(),
        "local" | "export" | "readonly" | "declare" | "typeset"
    ) && cmd.assigns.is_empty()
    {
        let non_flags: Vec<&str> = cmd
            .args()
            .iter()
            .map(String::as_str)
            .filter(|a| !a.starts_with('-'))
            .collect();
        if non_flags.len() != 1 {
            return None;
        }
        let (k, v) = non_flags[0].split_once('=')?;
        if v != "$" || k.is_empty() {
            return None;
        }
        k
    } else {
        return None;
    };
    let prev_idx = idx - 1;
    let has_single_subst_scope = script.scopes.iter().any(|(at, ev)| {
        *at == prev_idx && matches!(ev, shell::ScopeEvent::Enter(w) if w.is_empty())
    }) && script
        .scopes
        .iter()
        .any(|(at, ev)| *at == idx && matches!(ev, shell::ScopeEvent::Exit));
    (has_single_subst_scope && is_safe_mktemp_dir_cmd(&script.cmds[prev_idx])).then_some(var)
}

pub(super) fn update_mktemp_vars(script: &shell::Script, idx: usize, ctx: &mut Ctx) {
    let cmd = &script.cmds[idx];
    if let Some(var) = assigned_mktemp_var(script, idx) {
        if !ctx.mktemp_vars.iter().any(|v| v == var) {
            ctx.mktemp_vars.push(var.to_string());
        }
        return;
    }
    if ctx.mktemp_vars.is_empty() {
        return;
    }
    if cmd.name() == "eval" && cmd.arg_dynamic.iter().skip(1).any(|&d| d) {
        ctx.mktemp_vars.clear();
        return;
    }
    let is_unset = cmd.name() == "unset";
    ctx.mktemp_vars.retain(|var| {
        !cmd.sets_var(var) && !(is_unset && cmd.args().iter().any(|a| a == var || a.contains('$')))
    });
}
