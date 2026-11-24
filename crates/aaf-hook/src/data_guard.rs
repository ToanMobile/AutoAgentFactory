//! Keeps agents out of the AutoAgentFactory data directory (signing key,
//! receipts, approvals). Only the `aaf` CLI may name it.

use std::path::{Component, Path};

use crate::shell::Cmd;
use crate::{Ctx, Denial, paths};

mod search;
use search::search_needle_exempt_mask;

const RULE: &str = "data_dir_guard";
/// The reader is an agent. Inside a factory run `aaf gate` cannot work (the
/// factory runs the gate outside the sandbox) and `aaf doctor` cannot read
/// the data directory, so neither is offered; what an agent can run is.
const FIX: &str = "leave it alone: the factory or the operator runs aaf gate after the agent \
                   ends; an agent runs its tests directly and heavy builds through aaf slot -- \
                   <command>";
/// Spellings of the data directory that do not depend on its real location.
const NEEDLES: &[&str] = &[".local/share/autofactory", "AAF_DATA_DIR", "receipt.key"];

fn deny(what: &str) -> Denial {
    Denial::new(
        RULE,
        format!("{what} refers to the AutoAgentFactory data directory"),
        FIX,
    )
}

/// `dir` occurs in `text` as a whole path, not as a prefix of a longer name.
fn contains_path(text: &str, dir: &str) -> bool {
    if dir.len() < 2 {
        return false;
    }
    text.match_indices(dir).any(|(at, _)| {
        text[at + dir.len()..]
            .chars()
            .next()
            .is_none_or(|next| !(next.is_alphanumeric() || "_-.".contains(next)))
    })
}

fn mentions_data_dir(text: &str, ctx: &Ctx) -> bool {
    ctx.data_dirs
        .iter()
        .any(|dir| contains_path(text, &dir.to_string_lossy()))
}

/// Text-level check, used for words and for unparsable input.
pub fn mentions(text: &str, ctx: &Ctx) -> bool {
    NEEDLES.iter().any(|needle| text.contains(needle)) || mentions_data_dir(text, ctx)
}

/// Text a program reads as code (an interpreter's heredoc or `-c` script)
/// names the data directory.
pub fn check_text(program: &str, text: &str, ctx: &Ctx) -> Option<Denial> {
    mentions(text, ctx).then(|| deny(&format!("a `{program}` script")))
}

/// `text` read as a path lands in the data directory, lexically or through
/// a symlink. Relative paths count: `data/aaf.db` from the parent directory.
fn resolves_inside(text: &str, ctx: &Ctx) -> bool {
    if text.is_empty() || text.starts_with('-') || ctx.data_dirs.is_empty() {
        return false;
    }
    let path = ctx.expand(text);
    // Above an unknown working directory: it cannot be shown to stay out.
    if paths::escapes(&path) {
        return true;
    }
    let inside = |p: &Path| ctx.data_dirs.iter().any(|dir| p.starts_with(dir));
    if inside(&path) || inside(&ctx.real_of(&path)) {
        return true;
    }
    if text.contains("..") {
        let real_dd = ctx.expand_real_dotdot(text);
        if real_dd != path && (inside(&real_dd) || inside(&ctx.real_of(&real_dd))) {
            return true;
        }
    }
    false
}

/// The tails of a short-option word that could be a value glued to it:
/// `./data` in `-C./data`, `x.tar` in `-xzfx.tar`.
fn glued_values(word: &str) -> impl Iterator<Item = &str> {
    let cluster = word.len() > 2 && word.starts_with('-') && !word.starts_with("--");
    word.char_indices()
        .skip(2)
        .filter(move |_| cluster)
        .map(|(at, _)| &word[at..])
}

/// A glued tail resolves into the data directory. Most tails are option
/// letters (`a` in `-la`), so the file system is asked only about a tail
/// whose first component exists; a missing one can be inside only by name.
fn tail_inside(tail: &str, ctx: &Ctx) -> bool {
    let first = match Path::new(tail).components().next() {
        Some(Component::Normal(first)) => first,
        Some(_) => return resolves_inside(tail, ctx),
        None => return false,
    };
    let path = ctx.expand(tail);
    ctx.data_dirs.iter().any(|dir| path.starts_with(dir))
        || (ctx.cwd.join(first).symlink_metadata().is_ok() && resolves_inside(tail, ctx))
}

/// The word, the value of a `key=value` word (`if=data/aaf.db`,
/// `--directory=data`) or a value glued to a short option (`-Cdata`) is a
/// path into the data directory.
fn points_inside(word: &str, ctx: &Ctx) -> bool {
    let value = word.split_once('=').map(|(_, value)| value);
    resolves_inside(word, ctx)
        || value.is_some_and(|value| resolves_inside(value, ctx))
        || (!ctx.data_dirs.is_empty() && glued_values(word).any(|tail| tail_inside(tail, ctx)))
}

/// Options after which a tool reads its operands from another directory.
const DIR_OPTIONS: &[&str] = &["-C", "--directory", "--chdir", "--cwd"];

/// An operand lands in the data directory once it is read from the
/// directory a `-C <dir>` / `--directory=<dir>` before it changes to
/// (`tar -C .. -cf - data/aaf.db`). Each such option is relative to the one
/// before it, as in tar and git.
fn inside_after_chdir<'a>(mut words: impl Iterator<Item = &'a str>, ctx: &Ctx) -> bool {
    let mut moved: Option<Ctx> = None;
    while let Some(word) = words.next() {
        if moved.as_ref().is_some_and(|base| points_inside(word, base)) {
            return true;
        }
        let cluster = word.len() > 1 && word.starts_with('-') && !word.starts_with("--");
        let dir = if DIR_OPTIONS.contains(&word) || (cluster && word.ends_with('C')) {
            // `-C <dir>`, and `-xC <dir>` with the option last in a cluster.
            words.next()
        } else if let Some(glued) = word.strip_prefix("-C") {
            Some(glued)
        } else {
            word.split_once('=')
                .filter(|(option, _)| DIR_OPTIONS.contains(option))
                .map(|(_, dir)| dir)
        };
        if let Some(dir) = dir {
            let base = moved.as_ref().unwrap_or(ctx);
            if moved.is_some() && points_inside(dir, base) {
                return true;
            }
            moved = Some(base.with_cwd(base.expand_real_dotdot(dir)));
        }
    }
    false
}

const GIT_OTHER_VALUED: &[&str] = &[
    "-F",
    "--file",
    "-C",
    "-c",
    "--reuse-message",
    "--reedit-message",
    "--fixup",
    "--squash",
    "--author",
    "--date",
    "-t",
    "--template",
    "--cleanup",
    "--trailer",
    "--pathspec-from-file",
    "--local-user",
    "--strategy",
    "-X",
    "--strategy-option",
];
const GH_OTHER_VALUED: &[&str] = &[
    "-F",
    "--body-file",
    "-R",
    "--repo",
    "-B",
    "--base",
    "-H",
    "--head",
    "-a",
    "--assignee",
    "-l",
    "--label",
    "-m",
    "--milestone",
    "-p",
    "--project",
    "-r",
    "--reviewer",
    "-T",
    "--template",
];

fn is_git_m_flag(word: &str, allow_cluster: bool) -> bool {
    word == "-m"
        || word == "--message"
        || (allow_cluster
            && !word.starts_with("--")
            && word
                .strip_prefix('-')
                .and_then(|f| f.strip_suffix('m'))
                .is_some_and(|pre| !pre.is_empty() && pre.chars().all(|c| matches!(c, 'a' | 's'))))
}

fn mark_flags(
    cmd: &Cmd,
    start: usize,
    exempt: &mut [bool],
    separate: impl Fn(&str) -> bool,
    glued: impl Fn(&str) -> bool,
    other_valued: &[&str],
) {
    let is_dyn = |i: usize| cmd.arg_dynamic.get(i).copied().unwrap_or(true);
    let mut i = start;
    while let Some(word) = cmd.words.get(i).map(String::as_str) {
        if word == "--" {
            break;
        }
        if separate(word) {
            if !is_dyn(i) && i + 1 < cmd.words.len() && !is_dyn(i + 1) {
                exempt[i + 1] = true;
            }
            i += 2;
        } else if glued(word) {
            if !is_dyn(i) {
                exempt[i] = true;
            }
            i += 1;
        } else if other_valued.contains(&word) {
            i += 2;
        } else {
            i += 1;
        }
    }
}

/// Mask parallel to `cmd.words` marking static (non-dynamic) message values on
/// allowlisted `git`/`gh` subcommands that are exempt from `data_guard` only.
/// Returns `None` on the fast path without allocating.
fn msg_exempt_mask(cmd: &Cmd) -> Option<Vec<bool>> {
    if cmd.dynamic_name {
        return None;
    }
    match cmd.words.first().map(String::as_str) {
        Some("git") => {
            let call = crate::git_guard::split(cmd.args());
            let (start, cluster) = match call.sub {
                "commit" | "tag" => (2 + call.globals.len(), true),
                "merge" => (2 + call.globals.len(), false),
                "notes"
                    if matches!(
                        call.rest.first().map(String::as_str),
                        Some("add" | "append")
                    ) =>
                {
                    (3 + call.globals.len(), false)
                }
                _ => return None,
            };
            let mut exempt = vec![false; cmd.words.len()];
            mark_flags(
                cmd,
                start,
                &mut exempt,
                |w| is_git_m_flag(w, cluster),
                |w| (w.starts_with("-m") && w.len() > 2) || w.starts_with("--message="),
                GIT_OTHER_VALUED,
            );
            Some(exempt)
        }
        Some("gh")
            if matches!(cmd.words.get(1).map(String::as_str), Some("pr" | "issue"))
                && matches!(
                    cmd.words.get(2).map(String::as_str),
                    Some("create" | "edit" | "comment")
                ) =>
        {
            let mut exempt = vec![false; cmd.words.len()];
            mark_flags(
                cmd,
                3,
                &mut exempt,
                |w| matches!(w, "-t" | "--title" | "-b" | "--body"),
                |w| {
                    (w.starts_with("-t") && w.len() > 2)
                        || (w.starts_with("-b") && w.len() > 2)
                        || w.starts_with("--title=")
                        || w.starts_with("--body=")
                },
                GH_OTHER_VALUED,
            );
            Some(exempt)
        }
        _ => None,
    }
}

pub fn check_cmd(cmd: &Cmd, ctx: &Ctx) -> Option<Denial> {
    // Only the command typed as plain `aaf` is the CLI: `/tmp/evil/aaf` and a
    // name built at run time are some other program, and so is `aaf` on a
    // line that rebinds the word.
    let literal = cmd.words.first().is_some_and(|word| word == "aaf") && !cmd.dynamic_name;
    let is_aaf = literal && !cmd.aaf_untrusted;
    let exempt = msg_exempt_mask(cmd);
    let needle_exempt = search_needle_exempt_mask(cmd);
    let assigns = cmd
        .assigns
        .iter()
        .flat_map(|(key, value)| [key.as_str(), value.as_str()]);
    // A stripped wrapper's words are still paths the line names; exempt static
    // message values are skipped here only (all other guards still inspect them).
    let argv = || {
        cmd.wrapped.iter().map(String::as_str).chain(
            cmd.words
                .iter()
                .enumerate()
                .filter(|(i, _)| !exempt.as_ref().is_some_and(|m| m[*i]))
                .map(|(_, word)| word.as_str()),
        )
    };
    let word_hits = !is_aaf
        && (cmd
            .wrapped
            .iter()
            .any(|text| mentions(text, ctx) || points_inside(text, ctx))
            || cmd.words.iter().enumerate().any(|(i, word)| {
                if exempt.as_ref().is_some_and(|m| m[i]) {
                    return false;
                }
                if needle_exempt.as_ref().is_some_and(|m| m[i]) {
                    mentions_data_dir(word, ctx) || points_inside(word, ctx)
                } else {
                    mentions(word, ctx) || points_inside(word, ctx)
                }
            }));
    // Redirects are opened by the shell, whoever the command is.
    let redirects = cmd.redirects.iter().map(|r| r.target.as_str());
    let named = word_hits
        || assigns
            .filter(|_| !is_aaf)
            .chain(redirects)
            .any(|text| mentions(text, ctx) || points_inside(text, ctx));
    // Interpreter commands execute their quoted heredoc / piped-heredoc bodies
    // without shell lexing (so `interp_guard` inspects them); check those bodies
    // here too so `python3 - <<'EOF'` cannot reference `AAF_DATA_DIR`.
    let interp_heredoc = !is_aaf
        && crate::interp_guard::is_interpreter(cmd.name())
        && (cmd.heredocs.iter().any(|body| mentions(body, ctx))
            || std::iter::successors(cmd.feeder.as_deref(), |f| f.feeder.as_deref())
                .any(|f| f.heredocs.iter().any(|body| mentions(body, ctx))));
    (named
        || interp_heredoc
        || (!is_aaf && !ctx.data_dirs.is_empty() && inside_after_chdir(argv(), ctx)))
    .then(|| deny(&format!("`{}`", cmd.words.join(" "))))
}

/// `Read|Write|Edit|MultiEdit`: `raw` as given, `path` resolved.
pub fn check_file(raw: &str, path: &Path, ctx: &Ctx) -> Option<Denial> {
    let in_data = |p: &Path| {
        ctx.data_dirs.iter().any(|dir| p.starts_with(dir)) || {
            let real = ctx.real_of(p);
            ctx.data_dirs.iter().any(|dir| real.starts_with(dir))
        }
    };
    let inside = !ctx.data_dirs.is_empty()
        && (in_data(path)
            || (raw.contains("..") && {
                let real_dd = ctx.expand_real_dotdot(raw);
                real_dd != path && in_data(&real_dd)
            }));
    // Above an unknown working directory: it cannot be shown to stay out.
    let escapes = !ctx.data_dirs.is_empty() && paths::escapes(path);
    (inside || escapes || mentions(raw, ctx)).then(|| deny(&path.display().to_string()))
}
