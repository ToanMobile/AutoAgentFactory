//! Protected-file write checks for `cp`, `install`, `rsync`, and `sed`
//! (in-place `-i`/`-I`/`--in*` and script `w`/`W` write commands).

use std::fs;
use std::path::Path;

use crate::{Ctx, Denial, paths, sed_script};

use super::{check_recursive_rm_target, deny, hit, is_guarded_dir, pattern_could_match, target};

pub(super) fn check_sed(args: &[String], ctx: &Ctx) -> Option<Denial> {
    for shell_cmd in sed_script::exec_commands(args) {
        if ctx.depth >= crate::shell::MAX_DEPTH {
            return Some(deny(
                "sed `e` shell execution is nested too deeply to analyse".to_string(),
            ));
        }
        if let Some(denial) = crate::bash(&shell_cmd, ctx.nested()) {
            return Some(denial);
        }
    }
    if sed_script::has_bare_exec(args) {
        return Some(deny(
            "sed bare `e` or `s/.../e` executes arbitrary pattern-space text as a shell command"
                .to_string(),
        ));
    }
    for write_path in sed_script::script_files(args, &['w', 'W']) {
        if let Some(denial) = target(&write_path, false, "sed", ctx) {
            return Some(denial);
        }
    }
    if sed_script::edits_in_place(args) {
        let (_, files) = sed_script::parse_args(args);
        for file in files {
            // Secret files (`.env`, keys, certs) are denied right after by
            // `secret_guard` with secret-specific remediation; `delete_guard`
            // handles non-secret protected config files like `.claude/settings.json`.
            if paths::is_secret_file(&ctx.expand(file)).is_some() {
                continue;
            }
            if let Some(denial) = target(file, false, "sed", ctx) {
                return Some(denial);
            }
        }
    }
    None
}

fn copier_skips_next(name: &str, arg: &str) -> bool {
    if arg.contains('=') {
        return false;
    }
    if arg.starts_with('-') && !arg.starts_with("--") && arg.len() > 1 {
        let valued = match name {
            "cp" => "S",
            "install" => "mogS",
            "rsync" => "efT",
            _ => "",
        };
        if let Some(pos) = arg[1..].find(|c| valued.contains(c)) {
            return 1 + pos == arg.len() - 1;
        }
        return false;
    }
    match name {
        "cp" => arg == "--suffix",
        "install" => matches!(arg, "--mode" | "--owner" | "--group" | "--suffix"),
        "rsync" => matches!(
            arg,
            "--rsh"
                | "--exclude"
                | "--include"
                | "--filter"
                | "--suffix"
                | "--chmod"
                | "--chown"
                | "--temp-dir"
                | "--log-file"
                | "--out-format"
                | "--bwlimit"
                | "--compare-dest"
                | "--copy-dest"
                | "--link-dest"
                | "--backup-dir"
        ),
        _ => false,
    }
}

fn check_copy_into_dir(
    dir: &str,
    sources: &[&str],
    verb: &str,
    force_dir: bool,
    recursive: bool,
    ctx: &Ctx,
) -> Option<Denial> {
    if let Some(denial) = target(dir, false, verb, ctx) {
        return Some(denial);
    }
    if paths::has_glob(dir) {
        if recursive
            && sources
                .iter()
                .any(|src| src.ends_with('/') || src.ends_with("/.") || *src == ".")
            && let Some(denial) = target(dir, true, verb, ctx)
        {
            return Some(denial);
        }
        let base_dir = dir.trim_end_matches('/');
        for src in sources {
            let Some(fname) = Path::new(src).file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if let Some(denial) = target(&format!("{base_dir}/{fname}"), recursive, verb, ctx) {
                return Some(denial);
            }
            if (paths::has_glob(fname)
                || super::NESTED_PROTECTED
                    .iter()
                    .any(|&(_, f)| paths::fold(fname) == f))
                && let Some(denial) = target(dir, true, verb, ctx)
            {
                return Some(denial);
            }
        }
        return None;
    }
    let dir_path = ctx.expand_real_dotdot(dir);
    let guarded_dst = is_guarded_dir(&dir_path);
    let is_dir = force_dir
        || dir.ends_with('/')
        || matches!(dir, "." | "..")
        || sources.len() > 1
        || guarded_dst
        || dir_path.is_dir();
    if !is_dir {
        return None;
    }
    if recursive
        && guarded_dst
        && ctx.in_repo(&dir_path)
        && sources
            .iter()
            .any(|src| src.ends_with('/') || src.ends_with("/.") || *src == ".")
    {
        let dir_base = dir_path.file_name().and_then(|n| n.to_str()).unwrap_or(dir);
        return Some(deny(format!(
            "{verb} {dir}: recursive copy into {dir_base}/ could overwrite protected settings"
        )));
    }
    for src in sources {
        let Some(fname) = Path::new(src).file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if paths::has_glob(fname) {
            if guarded_dst && ctx.in_repo(&dir_path) {
                return Some(deny(format!(
                    "{verb} {dir}: glob source {src} into guarded directory could overwrite protected settings"
                )));
            }
            let src_path = ctx.expand_real_dotdot(src);
            if let Some(src_parent) = src_path.parent()
                && !paths::has_glob(&src_parent.to_string_lossy())
                && let Ok(entries) = fs::read_dir(src_parent)
            {
                for entry in entries.flatten() {
                    let Some(entry_name) = entry.file_name().into_string().ok() else {
                        continue;
                    };
                    if paths::glob_match(fname, &entry_name)
                        && let Some(why) = hit(&dir_path.join(&entry_name), recursive, ctx)
                    {
                        return Some(deny(format!("{verb} {dir}: {entry_name}: {why}")));
                    }
                }
                continue;
            }
            let dest = dir_path.join(fname);
            if pattern_could_match(&dest, fname, recursive) && ctx.in_repo(&dir_path) {
                return Some(deny(format!(
                    "{verb} {dir}: glob source {src} could overwrite a protected file"
                )));
            }
            continue;
        }
        let dest = dir_path.join(fname);
        if let Some(why) = hit(&dest, recursive, ctx) {
            return Some(deny(format!("{verb} {dir}: {fname}: {why}")));
        }
    }
    None
}

fn is_recursive_flag(name: &str, arg: &str) -> bool {
    if !matches!(name, "cp" | "rsync") {
        return false;
    }
    if matches!(arg, "--recursive" | "--archive") {
        return true;
    }
    if !arg.starts_with('-') || arg.starts_with("--") || arg.len() <= 1 {
        return false;
    }
    let flags = match name {
        "cp" => arg[1..].find('S').map_or(&arg[1..], |p| &arg[1..1 + p]),
        "rsync" => arg[1..]
            .find(['e', 'f', 'T'])
            .map_or(&arg[1..], |p| &arg[1..1 + p]),
        _ => "",
    };
    flags.chars().any(|c| matches!(c, 'r' | 'R' | 'a'))
}

pub(super) fn check_copier(name: &str, args: &[String], ctx: &Ctx) -> Option<Denial> {
    let mut target_dir: Option<&str> = None;
    let mut positionals: Vec<&str> = Vec::new();
    let mut only_pos = false;
    let mut recursive = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if only_pos || !arg.starts_with('-') || arg == "-" {
            positionals.push(arg);
            i += 1;
            continue;
        }
        if arg == "--" {
            only_pos = true;
            i += 1;
            continue;
        }
        if let Some(dir) = arg.strip_prefix("--target-directory=") {
            target_dir = Some(dir);
            i += 1;
            continue;
        }
        if arg == "--target-directory" {
            target_dir = args.get(i + 1).map(String::as_str);
            i += 2;
            continue;
        }
        let t_pos = if !arg.starts_with("--") && arg.len() > 1 && matches!(name, "cp" | "install") {
            let stop = match name {
                "cp" => arg[1..].find('S'),
                "install" => arg[1..].find(['m', 'o', 'g', 'S']),
                _ => None,
            };
            let prefix = stop.map_or(&arg[1..], |p| &arg[1..1 + p]);
            prefix.find('t')
        } else {
            None
        };
        if let Some(pos) = t_pos {
            if name == "cp"
                && arg[1..1 + pos]
                    .chars()
                    .any(|c| matches!(c, 'r' | 'R' | 'a'))
            {
                recursive = true;
            }
            let rest = &arg[2 + pos..];
            if rest.is_empty() {
                target_dir = args.get(i + 1).map(String::as_str);
                i += 2;
            } else {
                target_dir = Some(rest);
                i += 1;
            }
            continue;
        }
        if is_recursive_flag(name, arg) {
            recursive = true;
        }
        i += if copier_skips_next(name, arg) { 2 } else { 1 };
    }
    if name == "rsync"
        && let Some(denial) = rsync_deletes(args, &positionals, ctx)
    {
        return Some(denial);
    }
    if let Some(dir) = target_dir {
        return check_copy_into_dir(dir, &positionals, name, true, recursive, ctx);
    }
    let (&dst, sources) = positionals.split_last()?;
    check_copy_into_dir(dst, sources, name, false, recursive, ctx)
}

/// `rsync --delete*` empties the destination of whatever the source lacks
/// and `--remove-source-files` deletes the sources: both get the
/// containment of `rm -r`. A remote side (`host:path`) is not this machine.
/// rsync takes any unambiguous prefix of a long option, and the old name
/// `--remove-sent-files`: no other option starts with `--remov`.
fn rsync_deletes(args: &[String], positionals: &[&str], ctx: &Ctx) -> Option<Denial> {
    let options = || args.iter().take_while(|arg| *arg != "--");
    let deletes = options().any(|arg| arg.starts_with("--del"));
    let removes_sources = options().any(|arg| arg.starts_with("--remov"));
    let remote = |path: &str| {
        path.split('/')
            .next()
            .is_some_and(|head| head.contains(':'))
    };
    let (&dst, sources) = positionals.split_last()?;
    if deletes
        && !remote(dst)
        && let Some(denial) = check_recursive_rm_target(dst, "rsync --delete", ctx)
    {
        return Some(denial);
    }
    if removes_sources {
        return sources
            .iter()
            .filter(|src| !remote(src))
            .find_map(|src| check_recursive_rm_target(src, "rsync --remove-source-files", ctx));
    }
    None
}
