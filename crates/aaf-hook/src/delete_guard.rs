//! Protects secrets and factory settings inside the repository from being
//! deleted, moved or overwritten.

use std::fs;
use std::path::Path;

use crate::shell::{self, Cmd};
use crate::{Ctx, Denial, git_guard, paths};

const RULE: &str = "delete_guard";
const FIX: &str = "leave it in place; protected files are changed by a human, by hand";
/// Directories whose removal takes protected settings with them.
const GUARDED_DIRS: &[&str] = &[".claude", ".autofactory"];

fn deny(reason: String) -> Denial {
    Denial::new(RULE, reason, FIX)
}

/// The protected pattern of a file in the repository, by the name given or
/// by the file a symlink leads to: writing through `env_link -> .env`
/// changes `.env`.
fn protected(path: &Path, ctx: &Ctx) -> Option<&'static str> {
    let named = |path: &Path| paths::protected_name(path).filter(|_| ctx.in_repo(path));
    named(path).or_else(|| named(&paths::real(path)))
}

/// `Write|Edit|MultiEdit` on a path with `.`/`..` already removed.
pub fn check_file(path: &Path, ctx: &Ctx) -> Option<Denial> {
    let pattern = protected(path, ctx)?;
    Some(deny(format!(
        "{} is a protected file (pattern {pattern})",
        path.display()
    )))
}

pub fn check_cmd(cmd: &Cmd, ctx: &Ctx) -> Option<Denial> {
    for redirect in &cmd.redirects {
        if redirect.op.contains('>')
            && let Some(denial) = target(&redirect.target, false, "redirect to", ctx)
        {
            return Some(denial);
        }
    }
    let name = cmd.name();
    let args = cmd.args();
    if cmd.xargs
        && (cmd.dynamic_name || DELETERS.contains(&name))
        && feeds_protected(cmd.feeder.as_deref(), ctx)
    {
        let tool = if cmd.dynamic_name {
            "a tool chosen at run time"
        } else {
            name
        };
        return Some(Denial::new(
            RULE,
            format!("xargs would hand {tool} file names that may include protected files"),
            "feed xargs from a find whose -name pattern cannot match a protected file, or pass explicit paths",
        ));
    }
    match name {
        "rm" | "unlink" | "truncate" | "mv" | "tee" | "shred" => {
            let takes_dirs = name == "mv"
                || (name == "rm"
                    && args
                        .iter()
                        .any(|a| a == "--recursive" || short_has(a, &['r', 'R'])));
            positional(args)
                .into_iter()
                .find_map(|arg| target(arg, takes_dirs, name, ctx))
        }
        "git" => git(args, ctx),
        "find" => find(args, ctx),
        _ => None,
    }
}

/// Tools that remove or empty the files `find -exec` hands them. A shell is
/// one of them: what its script does with `$1` is not analysed.
const DELETERS: &[&str] = &[
    "rm", "unlink", "shred", "truncate", "mv", "tee", "sh", "bash", "zsh", "dash", "ksh",
];

const EXEC_ACTIONS: &[&str] = &["-exec", "-execdir", "-ok", "-okdir"];

/// The argv after `find -exec` runs a deleting tool, seen through the same
/// wrapper stripping as a typed command (`env rm`, `nice /bin/rm`,
/// `sh -c …`). A tool chosen at run time, or an argv that cannot be
/// analysed, could be one.
fn runs_deleter(argv: &[String]) -> bool {
    let script = shell::parse_argv(argv);
    !script.opaque.is_empty()
        || script
            .cmds
            .iter()
            .any(|cmd| cmd.dynamic_name || DELETERS.contains(&cmd.name()))
}

/// Where the expression of a `find` starts: its search roots end at the
/// first test, action or operator.
fn expression_start(args: &[String]) -> usize {
    args.iter()
        .position(|arg| {
            matches!(arg.as_str(), "(" | "!") || (arg.starts_with('-') && arg.len() > 2)
        })
        .unwrap_or(args.len())
}

/// `find … -delete` (or `-exec rm {}`) removes whatever its tests let
/// through, protected files included. It is allowed only when a `-name`
/// test every deleted file must pass cannot match a protected file.
fn find(args: &[String], ctx: &Ctx) -> Option<Denial> {
    let word = |at: usize| args.get(at).map_or("", String::as_str);
    let expression = expression_start(args);
    let action = (expression..args.len()).find(|at| match word(*at) {
        "-delete" => true,
        action if EXEC_ACTIONS.contains(&action) => {
            let end = (*at + 1..args.len())
                .find(|end| matches!(word(*end), ";" | "+"))
                .unwrap_or(args.len());
            let argv = args.get(*at + 1..end).unwrap_or(&[]);
            argv.iter().any(|arg| arg.contains("{}")) && runs_deleter(argv)
        }
        _ => false,
    })?;
    reaches_protected(args, expression, action, ctx).then(|| {
        Denial::new(
            RULE,
            "find would delete every match and its tests do not rule out protected files",
            "add a -name pattern that cannot match a protected file, or delete explicit paths",
        )
    })
}

/// The names piped into `xargs <deleter>` may include a protected file.
/// Only a `find` is understood; any other feeder (`ls`, `git ls-files`,
/// `cat list`, a pipeline stage in between) prints names the hook cannot
/// know, and that is refused.
fn feeds_protected(feeder: Option<&Cmd>, ctx: &Ctx) -> bool {
    let Some(find) = feeder.filter(|cmd| cmd.name() == "find" && !cmd.dynamic_name && !cmd.xargs)
    else {
        return true;
    };
    let args = find.args();
    let expression = expression_start(args);
    let tail = args.get(expression..).unwrap_or(&[]);
    // What `-exec` or a format string prints need not be the matched name.
    let prints_other = tail.iter().any(|arg| {
        EXEC_ACTIONS.contains(&arg.as_str()) || matches!(arg.as_str(), "-printf" | "-fprintf")
    });
    // Names are printed where the first action stands, else at the end;
    // a test after that point filters nothing.
    let action = tail
        .iter()
        .position(|arg| {
            matches!(
                arg.as_str(),
                "-print" | "-print0" | "-ls" | "-fprint" | "-fprint0" | "-fls" | "-delete"
            )
        })
        .map_or(args.len(), |at| expression + at);
    prints_other || reaches_protected(args, expression, action, ctx)
}

/// A file of the repository can reach the action at `action` without having
/// passed a `-name` test that rules protected files out.
fn reaches_protected(args: &[String], expression: usize, action: usize, ctx: &Ctx) -> bool {
    let word = |at: usize| args.get(at).map_or("", String::as_str);
    let mut roots: Vec<&str> = (0..expression)
        .map(word)
        .filter(|arg| !arg.starts_with('-'))
        .collect();
    if roots.is_empty() {
        roots.push(".");
    }
    let repo = ctx.root().unwrap_or_else(|| ctx.cwd.clone());
    let reaches_repo = roots.iter().any(|raw| {
        let root = ctx.expand(raw);
        paths::is_inside(&root, &repo) || paths::is_inside(&repo, &root)
    });
    if !reaches_repo {
        return false;
    }
    // With an operator a file can reach the action without passing the test;
    // a test after the action filters nothing.
    let branches = (expression..args.len())
        .any(|at| matches!(word(at), "-o" | "-or" | "!" | "-not" | "(" | ")" | ","));
    let filtered = !branches
        && (expression..action).any(|at| {
            matches!(word(at), "-name" | "-iname")
                && at + 1 < action
                && !paths::may_name_protected(word(at + 1))
        });
    !filtered
}

fn short_has(arg: &str, flags: &[char]) -> bool {
    arg.len() > 1 && arg.starts_with('-') && !arg.starts_with("--") && arg.contains(flags)
}

fn positional(args: &[String]) -> Vec<&str> {
    let mut only_positional = false;
    let mut out = Vec::new();
    for arg in args.iter().map(String::as_str) {
        if only_positional || !arg.starts_with('-') {
            out.push(arg);
        } else if arg == "--" {
            only_positional = true;
        }
    }
    out
}

fn git(args: &[String], ctx: &Ctx) -> Option<Denial> {
    let call = git_guard::split(args);
    match call.sub {
        "rm" => {
            // Pathspecs are relative to `-C <dir>` when given.
            let moved;
            let ctx = match call.dir {
                Some(dir) => {
                    moved = ctx.with_cwd(ctx.expand(dir));
                    &moved
                }
                None => ctx,
            };
            positional(call.rest)
                .into_iter()
                .find_map(|arg| target(arg, true, "git rm", ctx))
        }
        "clean" => {
            let flags: Vec<&str> = call
                .rest
                .iter()
                .map(String::as_str)
                .take_while(|a| *a != "--")
                .collect();
            let ignored = flags.iter().any(|a| short_has(a, &['x', 'X']));
            // The last of `-n`/`--dry-run` and `--no-dry-run` wins, as in git.
            let dry_run = flags.iter().fold(false, |dry_run, a| {
                if *a == "--dry-run" || short_has(a, &['n']) {
                    true
                } else {
                    dry_run && !a.starts_with("--no-d")
                }
            });
            (ignored && !dry_run).then(|| {
                Denial::new(
                    RULE,
                    "git clean -x/-X deletes ignored files such as .env and keystores",
                    "use git clean -fd (without -x), or preview with -n",
                )
            })
        }
        _ => None,
    }
}

fn target(raw: &str, takes_dirs: bool, verb: &str, ctx: &Ctx) -> Option<Denial> {
    let path = ctx.expand(raw);
    let why = if paths::has_glob(raw) {
        glob_hit(&path, takes_dirs, ctx)
    } else {
        hit(&path, takes_dirs, ctx)
    };
    why.map(|why| deny(format!("{verb} {raw}: {why}")))
}

fn hit(path: &Path, takes_dirs: bool, ctx: &Ctx) -> Option<String> {
    if let Some(pattern) = protected(path, ctx) {
        return Some(format!("protected file (pattern {pattern})"));
    }
    if !takes_dirs {
        return None;
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if GUARDED_DIRS.contains(&name) && ctx.in_repo(path) {
        return Some(format!("{name}/ holds protected settings"));
    }
    // Only a path at or above the working directory can contain the repo, so
    // the root lookup stays off the common `rm -rf build` path.
    if ctx.cwd.starts_with(path) && ctx.root().is_some_and(|root| paths::is_inside(&root, path)) {
        return Some("it contains the whole repository".to_string());
    }
    None
}

/// Expands a glob in the last component against the real directory; a glob
/// in a directory component is judged by what it could match.
fn glob_hit(path: &Path, takes_dirs: bool, ctx: &Ctx) -> Option<String> {
    let parent = path.parent()?;
    let last = path.file_name()?.to_str()?;
    if paths::has_glob(&parent.to_string_lossy()) {
        let could_match = paths::protected_name(path).is_some()
            || paths::SAMPLE_NAMES
                .iter()
                .any(|sample| paths::glob_match(last, sample));
        return (could_match && ctx.in_repo(path))
            .then(|| "the pattern can match a protected file".to_string());
    }
    // A directory that cannot be listed has nothing the glob could delete.
    let entries = fs::read_dir(parent).ok()?;
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| paths::glob_match(last, name))
        .find_map(|name| {
            hit(&parent.join(&name), takes_dirs, ctx).map(|why| format!("{name}: {why}"))
        })
}
