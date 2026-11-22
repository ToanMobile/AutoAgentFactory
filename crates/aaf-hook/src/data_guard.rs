//! Keeps agents out of the AutoAgentFactory data directory (signing key,
//! receipts, approvals). Only the `aaf` CLI may name it.

use std::path::{Component, Path};

use crate::shell::Cmd;
use crate::{Ctx, Denial, paths};

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

/// Text-level check, used for words and for unparsable input.
pub fn mentions(text: &str, ctx: &Ctx) -> bool {
    NEEDLES.iter().any(|needle| text.contains(needle))
        || ctx
            .data_dirs
            .iter()
            .any(|dir| contains_path(text, &dir.to_string_lossy()))
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
    let inside = |path: &Path| ctx.data_dirs.iter().any(|dir| path.starts_with(dir));
    inside(&path) || inside(&paths::real(&path))
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
            moved = Some(base.with_cwd(base.expand(dir)));
        }
    }
    false
}

pub fn check_cmd(cmd: &Cmd, ctx: &Ctx) -> Option<Denial> {
    // Only the command typed as plain `aaf` is the CLI: `/tmp/evil/aaf` and a
    // name built at run time are some other program, and so is `aaf` on a
    // line that rebinds the word.
    let literal = cmd.words.first().is_some_and(|word| word == "aaf") && !cmd.dynamic_name;
    let is_aaf = literal && !cmd.aaf_untrusted;
    let assigns = cmd
        .assigns
        .iter()
        .flat_map(|(key, value)| [key.as_str(), value.as_str()]);
    // A stripped wrapper's words are still paths the line names.
    let argv = || cmd.wrapped.iter().chain(&cmd.words).map(String::as_str);
    // Redirects are opened by the shell, whoever the command is.
    let redirects = cmd.redirects.iter().map(|r| r.target.as_str());
    let named = argv()
        .chain(assigns)
        .filter(|_| !is_aaf)
        .chain(redirects)
        .any(|text| mentions(text, ctx) || points_inside(text, ctx));
    (named || (!is_aaf && !ctx.data_dirs.is_empty() && inside_after_chdir(argv(), ctx)))
        .then(|| deny(&format!("`{}`", cmd.words.join(" "))))
}

/// `Read|Write|Edit|MultiEdit`: `raw` as given, `path` resolved.
pub fn check_file(raw: &str, path: &Path, ctx: &Ctx) -> Option<Denial> {
    let inside = ctx.data_dirs.iter().any(|dir| paths::is_inside(path, dir));
    // Above an unknown working directory: it cannot be shown to stay out.
    let escapes = !ctx.data_dirs.is_empty() && paths::escapes(path);
    (inside || escapes || mentions(raw, ctx)).then(|| deny(&path.display().to_string()))
}
