//! `find … -delete`, `find -exec rm`, and `find | xargs rm`: the search
//! roots get the containment of `rm -r`, and the tests must rule out
//! protected files.

use std::path::{Path, PathBuf};

use crate::shell::{self, Cmd};
use crate::{Ctx, Denial, paths};

use super::{RULE, check_delete_root};

/// Tools that remove or empty the files `find -exec` hands them. A shell is
/// one of them: what its script does with `$1` is not analysed.
pub(super) const DELETERS: &[&str] = &[
    "rm",
    "unlink",
    "shred",
    "truncate",
    "mv",
    "tee",
    "sh",
    "bash",
    "zsh",
    "dash",
    "ksh",
    "rimraf",
    "trash",
    "trash-put",
    "del",
    "del-cli",
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

/// A global option `find` reads before its search roots: `-H`, `-L`, `-P`
/// and BSD `-E`, `-X`, `-d`, `-s`, `-x`, alone or clustered (`-sx`), or GNU
/// `-O<level>`.
fn global_option(arg: &str) -> bool {
    let Some(flags) = arg.strip_prefix('-') else {
        return false;
    };
    let clustered = !flags.is_empty() && flags.chars().all(|c| "HLPEXdsx".contains(c));
    let level = flags
        .strip_prefix('O')
        .is_some_and(|level| !level.is_empty() && level.chars().all(|c| c.is_ascii_digit()));
    clustered || level
}

/// BSD `find` (getopt `EHLPXdf:sx`): a cluster of flag letters ending in
/// `f` (`-f`, `-xf`, `-sf/`) whose value, glued or the next word, is a
/// search root. `Some("")` when the value is the next word.
fn dash_f(arg: &str) -> Option<&str> {
    let letters = arg.strip_prefix('-')?;
    let at = letters.find('f')?;
    letters[..at]
        .chars()
        .all(|c| "HLPEXdsx".contains(c))
        .then(|| &letters[at + 1..])
}

/// GNU `find` primaries that read like a glued BSD `-f<root>`.
const F_PRIMARIES: &[&str] = &[
    "-false", "-fls", "-fprint", "-fprint0", "-fprintf", "-fstype", "-follow",
];

/// The search roots of a `find` and where its expression starts. Leading
/// global options are skipped (GNU `-D <opts>` with its value), the value
/// of BSD `-f` (glued, `-f/x`, or the next word, also after a cluster of
/// flags, `-xf x`) is a root whatever it looks like, and the roots end at
/// the first test, action or operator. `.` stands in when none is named.
fn layout(args: &[String]) -> (Vec<&str>, usize) {
    let mut roots = Vec::new();
    let mut at = 0;
    while let Some(arg) = args.get(at).map(String::as_str) {
        if arg == "-D" {
            at += 2;
        } else if let Some(glued) = dash_f(arg) {
            if glued.is_empty() {
                roots.extend(args.get(at + 1).map(String::as_str));
                at += 2;
            } else {
                roots.push(glued);
                if F_PRIMARIES.contains(&arg) {
                    // GNU reads it as the expression, searching `.`.
                    roots.push(".");
                    return (roots, at);
                }
                at += 1;
            }
        } else if global_option(arg) {
            at += 1;
        } else {
            break;
        }
    }
    while let Some(arg) = args.get(at).map(String::as_str) {
        if matches!(arg, "(" | "!") || (arg.starts_with('-') && arg.len() > 2) {
            break;
        }
        if !arg.starts_with('-') {
            roots.push(arg);
        }
        at += 1;
    }
    if roots.is_empty() {
        roots.push(".");
    }
    (roots, at.min(args.len()))
}

/// Where the expression of a `find` starts.
fn expression_start(args: &[String]) -> usize {
    layout(args).1
}

/// The search roots of a `find`.
fn roots(args: &[String]) -> Vec<&str> {
    layout(args).0
}

/// A search root the deletes may not start from. A word filled in at run
/// time could be any directory. `/tmp`, `/private/tmp` or `$TMPDIR` itself
/// only has its contents deleted, like `rm -r /tmp/<name>`, unless it holds
/// the repository or the home directory. Any other root gets the
/// containment of `rm -r`.
fn root_denied(raw: &str, ctx: &Ctx) -> Option<Denial> {
    if raw.is_empty() {
        return Some(Denial::new(
            RULE,
            "find with a search root filled in at run time could delete anywhere",
            "name the directory to search literally",
        ));
    }
    if temp_root(raw, ctx) {
        return None;
    }
    check_delete_root(raw, "find", ctx)
}

fn temp_root(raw: &str, ctx: &Ctx) -> bool {
    if paths::has_unresolved_var(raw) {
        return false;
    }
    let path = ctx.expand(raw);
    let real = paths::real(&path);
    let mut temps = vec![PathBuf::from("/tmp"), PathBuf::from("/private/tmp")];
    if let Some(dir) = std::env::var_os("TMPDIR").filter(|dir| !dir.is_empty()) {
        let dir = paths::normalize(Path::new(&dir));
        temps.push(paths::real(&dir));
        temps.push(dir);
    }
    let holds = |dir: &Path| {
        ctx.holds_workspace_repo(dir)
            || ctx.root().is_some_and(|root| paths::is_inside(&root, dir))
            || ctx
                .env
                .home
                .as_deref()
                .is_some_and(|home| paths::is_inside(home, dir))
    };
    temps.contains(&path) && temps.contains(&real) && !holds(&path) && !holds(&real)
}

/// `find … -delete` (or `-exec rm {}`) removes whatever its tests let
/// through, protected files included. It is allowed only when a `-name`
/// test every deleted file must pass cannot match a protected file.
pub(super) fn find(args: &[String], ctx: &Ctx) -> Option<Denial> {
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
    // Whatever its tests, the search roots are deleted from: they get the
    // containment of `rm -r` (the repository, `/tmp`, `$TMPDIR`).
    if let Some(denial) = roots(args)
        .into_iter()
        .find_map(|root| root_denied(root, ctx))
    {
        return Some(denial);
    }
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
pub(super) fn feeds_protected(feeder: Option<&Cmd>, ctx: &Ctx) -> bool {
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
    prints_other
        || roots(args)
            .into_iter()
            .any(|root| root_denied(root, ctx).is_some())
        || reaches_protected(args, expression, action, ctx)
}

/// A file of the repository can reach the action at `action` without having
/// passed a `-name` test that rules protected files out.
fn reaches_protected(args: &[String], expression: usize, action: usize, ctx: &Ctx) -> bool {
    let word = |at: usize| args.get(at).map_or("", String::as_str);
    let roots = roots(args);
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
