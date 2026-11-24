//! Interpreter scripts that write files: what they write must not be a
//! protected file or one of git's own (settings, hooks, the git directory's
//! state).
//!
//! Only a path in a write position counts: the first argument of a write
//! API, the destination of a copy or link, both sides of a move, the path
//! of an `open` whose own mode writes, or the path object a `write_text`
//! is called on. A file the same script only reads is not held against it.
//! When a written path is filled in at run time (a loop variable, a
//! function parameter, `ps[0]`), every literal of the script is judged as
//! written, but a bare protected directory (`.git`, `.claude`): opening or
//! writing a directory fails, so it is judged only when a copy, link or
//! move may fill it. Below each such directory the script names, the parts
//! the written path joins below its run-time part are judged
//! (`open(os.path.join(d, 'config'), 'a')` may write `.git/config`).
//! When two run-time parts are joined (`join(d, f)`), each literal beside
//! such a directory's name in a tuple, list, dict or call's arguments
//! (`[('.git', 'hooks/pre-commit')]`, `w('.git', 'config')`) is judged
//! below it. Names handed to `ignore_patterns`, or given as what a glob
//! leaves out (`ignore=…`), are not written. A script that writes also has
//! the directories it changes to (`os.chdir`) judged as written, and, while
//! a written path is not known, the parts it joins into paths. Git's own
//! state is judged where a script writes or may write, not in every
//! literal it names (`open('.git/HEAD').read()`).

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path};
use std::rc::Rc;

use crate::{Ctx, Denial, delete_guard, git_files, paths};

use super::{deny, extract_literals};

mod chdir;
mod named;
mod parse;

use parse::{Below, Call, Resolved, Script, calls};

/// Which arguments of a call are written to.
#[derive(Clone, Copy, PartialEq)]
enum Written {
    /// The first: `writeFileSync(path, …)`, `os.remove(path)`.
    First,
    /// The destination: `shutil.copy(src, dst)`, `fs.cp(src, dst, cb)`.
    Second,
    /// Every one: a move empties its source (`os.rename(a, b)`).
    All,
    /// The path, when the call's own mode writes: `open(path, 'w')`.
    Open,
    /// The object it is called on: `Path(…).write_text(…)`.
    Receiver,
    /// The object it is called on, and every argument: `Path(a).rename(b)`.
    ReceiverAll,
}

fn classify(call: &Call) -> Option<Written> {
    let qual = call.qual.rsplit(['.', ':']).next().unwrap_or("");
    let on_path = call.receiver.as_deref().is_some_and(|r| r.contains("Path"));
    let kind = match call.name.as_str() {
        "writeFile" | "writeFileSync" | "appendFile" | "appendFileSync" | "createWriteStream"
        | "file_put_contents" | "truncateSync" | "binwrite" | "unlinkSync" => Written::First,
        // `shutil` helpers that write nothing.
        "ignore_patterns" | "which" | "disk_usage" | "get_terminal_size" => return None,
        "write" if matches!(qual, "File" | "IO") => Written::First,
        "write_text" | "write_bytes" | "symlink_to" | "hardlink_to" => Written::Receiver,
        "write" | "touch" | "unlink" if on_path => Written::Receiver,
        "unlink" if matches!(qual, "" | "os" | "fs" | "File") => Written::First,
        "remove" if qual == "os" => Written::First,
        "delete" if qual == "File" => Written::First,
        "truncate" if matches!(qual, "os" | "fs" | "File") => Written::First,
        "copyFile" | "copyFileSync" | "cp" | "cpSync" | "copy" | "copy2" | "copyfile"
        | "copytree" | "copy_file" | "symlink" | "symlinkSync" | "link" | "linkSync" | "ln"
        | "ln_s" | "ln_sf" | "cp_r" => Written::Second,
        "install" if qual == "FileUtils" => Written::Second,
        "rename" | "replace" if on_path => Written::ReceiverAll,
        "rename" | "renameSync" | "move" | "mv" => Written::All,
        "replace" if qual == "os" => Written::All,
        "open" | "fopen" | "openSync" | "sysopen" | "fdopen" => Written::Open,
        "new" if matches!(qual, "File" | "IO") => Written::Open,
        _ if matches!(qual, "FileUtils" | "shutil") => Written::All,
        _ => return None,
    };
    Some(kind)
}

/// A mode literal of `open`/`fopen`/`File.open` that writes (`'w'`, `'a+'`,
/// `'r+b'`), or a Perl `open` target that does (`'>'`, `">file"`, `'+<'`).
fn write_mode(lit: &str) -> bool {
    let lit = lit.trim();
    lit.starts_with(['>', '|'])
        || lit.starts_with("+<")
        || (!lit.is_empty()
            && lit.len() <= 4
            && lit.chars().all(|c| "rwaxbt+".contains(c))
            && lit.contains(['w', 'a', 'x', '+']))
}

/// An `os.open`/`sysopen` flag that writes.
fn write_flag(arg: &str) -> bool {
    ["O_WRONLY", "O_RDWR", "O_CREAT", "O_TRUNC", "O_APPEND"]
        .iter()
        .any(|flag| arg.contains(flag))
}

/// A Perl `open` mode or target: `'<'`, `'>'`, `'+<'`, `'-|'`.
fn perl_mode(lit: &str) -> bool {
    let lit = lit.trim();
    lit.starts_with(['<', '>', '|']) || lit.starts_with("+<") || lit.ends_with('|')
}

/// What a script writes, as far as it can be told.
#[derive(Default)]
struct Writes {
    /// The expressions whose paths are in `paths` already.
    added: HashSet<String>,
    /// The paths in write positions, as written in the script.
    paths: Vec<String>,
    /// The expressions in write positions.
    exprs: Vec<String>,
    /// Some call writes a file.
    wrote: bool,
    /// Some written path is filled in at run time.
    unsure: bool,
    /// Some path a copy, link or move fills is filled in at run time: it may
    /// be a directory, the git directory itself.
    unsure_dir: bool,
}

impl Writes {
    fn add(&mut self, (text, found): (&str, Rc<Resolved>)) {
        self.unsure |= !found.sure;
        self.extend(text, &found);
    }

    fn add_dir(&mut self, (text, found): (&str, Rc<Resolved>)) {
        self.unsure_dir |= !found.sure;
        self.add((text, found));
    }

    /// The paths of `text`, once however often it is written. A path
    /// joined from literals only is judged whole (`join('.git', 'info',
    /// 'exclude')`), not the directories on its way.
    fn extend(&mut self, text: &str, found: &Resolved) {
        if !self.added.insert(text.trim().to_string()) {
            return;
        }
        let whole = found
            .lits
            .last()
            .filter(|_| found.sure && found.lits.len() > 1);
        let on_way = |lit: &String| {
            whole.is_some_and(|whole| {
                whole
                    .strip_prefix(lit.trim_end_matches('/'))
                    .is_some_and(|rest| rest.starts_with('/'))
            })
        };
        self.paths
            .extend(found.lits.iter().filter(|lit| !on_way(lit)).cloned());
    }
}

/// Each distinct expression of a write position resolved once: a loop body
/// writes the same path many times over.
struct Cache<'s, 'a> {
    script: &'s Script<'a>,
    found: HashMap<String, Rc<Resolved>>,
}

impl Cache<'_, '_> {
    fn get<'t>(&mut self, text: Option<&'t String>, missing: bool) -> (&'t str, Rc<Resolved>) {
        let Some(text) = text else {
            let none = Resolved {
                lits: Vec::new(),
                sure: missing,
            };
            return ("", Rc::new(none));
        };
        let key = text.trim();
        if let Some(found) = self.found.get(key) {
            return (text, Rc::clone(found));
        }
        let found = Rc::new(self.script.resolve(key, 0));
        self.found.insert(key.to_string(), Rc::clone(&found));
        (text, found)
    }
}

/// The paths a script writes to.
fn written(script: &Script, found: &[Call]) -> Writes {
    let mut out = Writes::default();
    let mut cache = Cache {
        script,
        found: HashMap::new(),
    };
    for call in found {
        let Some(kind) = classify(call) else {
            continue;
        };
        // No receiver to write through is a path filled in at run time.
        let receiver = |cache: &mut Cache| cache.get(call.receiver.as_ref(), false);
        let arg = |cache: &mut Cache, at: usize| cache.get(call.args.get(at), true);
        let every_arg = |out: &mut Writes, cache: &mut Cache| {
            (0..call.args.len()).for_each(|at| out.add_dir(arg(cache, at)));
        };
        let expr = |out: &mut Writes, at: Option<usize>| {
            let text = match at {
                Some(at) => call.args.get(at),
                None => call.receiver.as_ref(),
            };
            out.exprs.extend(text.cloned());
        };
        match kind {
            Written::First => expr(&mut out, Some(0)),
            Written::Second => expr(&mut out, Some(1)),
            Written::All => (0..call.args.len()).for_each(|at| expr(&mut out, Some(at))),
            Written::Receiver => expr(&mut out, None),
            Written::ReceiverAll => {
                expr(&mut out, None);
                (0..call.args.len()).for_each(|at| expr(&mut out, Some(at)));
            }
            Written::Open => {}
        }
        match kind {
            Written::First => out.add(arg(&mut cache, 0)),
            Written::Second => out.add_dir(arg(&mut cache, 1)),
            Written::All => every_arg(&mut out, &mut cache),
            Written::Receiver => out.add(receiver(&mut cache)),
            Written::ReceiverAll => {
                out.add_dir(receiver(&mut cache));
                every_arg(&mut out, &mut cache);
            }
            Written::Open => {
                let args: Vec<(&str, Rc<Resolved>)> =
                    (0..call.args.len()).map(|at| arg(&mut cache, at)).collect();
                if !opens_for_write(call, &args) {
                    continue;
                }
                let perl = args
                    .iter()
                    .skip(1)
                    .flat_map(|(_, a)| &a.lits)
                    .any(|l| perl_mode(l));
                // The path: what it is called on, the second argument of
                // `sysopen`, the last of a Perl `open`, else the first.
                let path = if call.receiver.is_some() {
                    None
                } else if call.name == "sysopen" {
                    Some(1)
                } else if perl {
                    Some(call.args.len().saturating_sub(1))
                } else {
                    Some(0)
                };
                let (text, on) = receiver(&mut cache);
                out.unsure |= match path {
                    None => !on.sure,
                    Some(at) => args.get(at).is_some_and(|(_, a)| !a.sure),
                };
                expr(&mut out, path);
                out.extend(text, &on);
                for (text, found) in &args {
                    out.extend(text, found);
                }
            }
        }
        out.wrote = true;
    }
    out
}

/// An `open`-like call whose mode writes. Without a receiver the first
/// argument is the path (or a Perl handle); the mode follows it, or is
/// glued to the path. A mode held by a variable of no known value could
/// write; so could any `sysopen` but a read-only one.
fn opens_for_write(call: &Call, args: &[(&str, Rc<Resolved>)]) -> bool {
    if call.name == "sysopen" {
        let flags = call.args.get(2).map_or("", |flags| flags.trim());
        return !matches!(flags, "O_RDONLY" | "0" | "");
    }
    let skip = usize::from(call.receiver.is_none());
    args.iter()
        .zip(&call.args)
        .enumerate()
        .any(|(at, ((_, found), text))| {
            found
                .lits
                .iter()
                .any(|lit| lit.trim_start().starts_with(['>', '+']))
                || (at >= skip
                    && (write_flag(text)
                        || found.lits.iter().any(|l| write_mode(l))
                        || (parse::is_ident(text.trim()) && !found.sure)))
        })
}

/// The script opens or writes a file for writing: a Perl `-e` that does
/// is not one that only reads the files it is given.
pub(super) fn writes_files(code: &str) -> bool {
    let script = Script::new(code);
    written(&script, &calls(&script)).wrote
}

/// The directories a script changes to, and the parts it joins into paths
/// (`os.path.join`, `joinpath`, `path.resolve`, an interpolated string's
/// fixed head: `f'.git/hooks/{name}'`).
fn context(script: &Script, found: &[Call]) -> (Vec<String>, Vec<String>) {
    let mut dirs = Vec::new();
    let mut parts = Vec::new();
    // Each expression once: the same join in a loop body adds nothing.
    let (mut chdir, mut joined) = (HashSet::new(), HashSet::new());
    let resolve = |seen: &mut HashSet<String>, text: &String| {
        let fresh = seen.insert(text.trim().to_string());
        fresh.then(|| script.resolve(text, 0).lits)
    };
    for call in found {
        match call.name.as_str() {
            "chdir" => {
                for arg in &call.args {
                    dirs.extend(resolve(&mut chdir, arg).into_iter().flatten());
                }
            }
            "join" | "joinpath" | "resolve" => {
                for text in call.args.iter().chain(&call.receiver) {
                    parts.extend(resolve(&mut joined, text).into_iter().flatten());
                }
            }
            _ => {}
        }
    }
    for lit in extract_literals(script.code) {
        if let Some(at) = lit.find(['{', '$'])
            && at > 0
        {
            parts.push(lit[..at].to_string());
        }
    }
    (dirs, parts)
}

/// Where a judged path comes from: git's own state is judged where the
/// script writes, or may write.
#[derive(Clone, Copy, Hash, PartialEq, Eq)]
enum Source {
    /// A path in a write position, or below a directory changed to.
    Written,
    /// A part joined below a protected directory the script names.
    Composed,
    /// Any literal of a script whose written path is not known.
    Named,
}

/// A path a script writes to is a protected file or git's own.
pub(super) fn check_written(interp: &str, code: &str, ctx: &Ctx) -> Option<Denial> {
    let script = Script::new(code);
    let found = calls(&script);
    let writes = written(&script, &found);
    if !writes.wrote {
        return None;
    }
    let (dirs, parts) = context(&script, &found);
    let with = |how: Source| move |lit: String| (lit, how);
    let mut judged: Vec<(String, Source)> = writes
        .paths
        .iter()
        .cloned()
        .map(with(Source::Written))
        .collect();
    // A bare protected directory is judged only when a copy or move may
    // fill a directory.
    let kept = |lit: &String| writes.unsure_dir || protected_dir(lit, ctx).is_none();
    if writes.unsure {
        let named = named::named(code);
        let mut protected: Vec<&str> = named
            .lits
            .iter()
            .filter_map(|lit| protected_dir(lit, ctx))
            .collect();
        protected.sort_unstable();
        protected.dedup();
        if !protected.is_empty() {
            let mut below = Below::default();
            let mut seen = HashSet::new();
            for expr in &writes.exprs {
                script.below(expr, 0, &mut seen, &mut below);
            }
            below.tails.sort_unstable();
            below.tails.dedup();
            for dir in &protected {
                let composed = below.tails.iter().map(|tail| format!("{dir}{tail}"));
                judged.extend(composed.filter(kept).map(with(Source::Composed)));
            }
            if below.pair {
                let beside = named.beside(|lit| protected_dir(lit, ctx));
                judged.extend(beside.into_iter().map(with(Source::Composed)));
            }
        }
        let named = named.lits.into_iter().filter(kept);
        judged.extend(named.map(with(Source::Named)));
    }
    let relative: Vec<String> = judged
        .iter()
        .map(|(lit, _)| bare(lit).to_string())
        .filter(|path| !path.is_empty() && !path.starts_with(['/', '~']))
        .collect();
    let Some(below) = chdir::below_dirs(&dirs, &relative) else {
        return Some(deny(
            format!("{interp} changes directory too often to analyse its writes"),
            "split the script, or write to absolute paths instead of changing directory",
        ));
    };
    judged.extend(below.into_iter().map(with(Source::Written)));
    judged.extend(dirs.into_iter().map(with(Source::Written)));
    if writes.unsure {
        judged.extend(parts.into_iter().filter(kept).map(with(Source::Named)));
    }
    // Each path once, however it is spelled: a loop body repeats the same
    // write many times.
    let mut seen = HashSet::new();
    judged
        .iter()
        .filter(|(lit, how)| judged_at_all(lit) && seen.insert((*how, ctx.expand(bare(lit)))))
        .find_map(|(lit, how)| judge(interp, lit, *how, ctx))
}

/// The protected directory a literal names bare (`.git`, `./.git`,
/// `.claude/`, the data dir's own name), as the name to join below.
fn protected_dir<'a>(lit: &'a str, ctx: &Ctx) -> Option<&'a str> {
    let path = bare(lit).trim_end_matches('/');
    let name = path.strip_prefix("./").unwrap_or(path);
    // `'/.git' in root`, `p.includes('/.git/')`: the name, tested.
    let slashed = name.strip_prefix('/').unwrap_or(name);
    let data = ctx
        .data_dirs
        .iter()
        .filter_map(|dir| dir.file_name()?.to_str())
        .any(|base| base == name);
    if [".git", ".claude"]
        .iter()
        .any(|dir| slashed.eq_ignore_ascii_case(dir))
    {
        return Some(slashed);
    }
    (!name.is_empty() && data).then_some(name)
}

/// What git itself reads in its directory: a script that may write there
/// changes the branch, the index or the object store under git's feet.
/// `info/exclude` is the user's own list of ignored files; nothing is
/// below git's files (`config/app.json`). A path composed below a
/// directory picked at run time is not git's when it ends in a dot-file
/// git does not read (`logs/.keep`).
fn git_state(path: &Path, composed: bool) -> bool {
    const FILES: &[&str] = &[
        "HEAD",
        "ORIG_HEAD",
        "FETCH_HEAD",
        "MERGE_HEAD",
        "index",
        "packed-refs",
        "shallow",
        "config",
        "commondir",
        "gitdir",
    ];
    const DIRS: &[&str] = &[
        "info",
        "objects",
        "refs",
        "logs",
        "hooks",
        "modules",
        "worktrees",
        "branches",
        "remotes",
    ];
    let parts: Vec<String> = path
        .components()
        .filter_map(|part| match part {
            Component::Normal(name) => name.to_str().map(paths::fold),
            _ => None,
        })
        .collect();
    let is = |at: usize, name: &str| parts.get(at).is_some_and(|p| p.eq_ignore_ascii_case(name));
    let dot_leaf = composed && parts.last().is_some_and(|last| last.starts_with('.'));
    (0..parts.len()).any(|at| {
        let file = FILES.iter().any(|name| is(at + 1, name)) && parts.len() == at + 2;
        let dir = DIRS.iter().any(|name| is(at + 1, name))
            && !(is(at + 1, "info") && is(at + 2, "exclude"))
            && !(dot_leaf && parts.len() > at + 2);
        is(at, ".git") && (file || dir)
    })
}

#[cfg(test)]
thread_local! {
    /// How often [`judge`] looked at a path.
    static JUDGE_RUNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A literal that can name a path at all: not a bare `open` mode, not text
/// spanning lines.
fn judged_at_all(lit: &str) -> bool {
    let trimmed = lit.trim();
    let mode_only = trimmed.len() <= 4 && trimmed.chars().all(|c| "rwaxbt+<>|-".contains(c));
    !bare(lit).is_empty() && !lit.contains('\n') && !mode_only
}

/// A path with a Perl mode glued to it (`">file"`) taken off.
fn bare(lit: &str) -> &str {
    lit.trim().trim_start_matches(['+', '<', '>', '|']).trim()
}

fn judge(interp: &str, lit: &str, how: Source, ctx: &Ctx) -> Option<Denial> {
    #[cfg(test)]
    JUDGE_RUNS.with(|runs| runs.set(runs.get() + 1));
    let path = bare(lit);
    if let Some(d) = delete_guard::target(path, false, interp, ctx) {
        return Some(deny(
            format!("{interp} write to protected path blocked: {}", d.reason),
            &d.fix,
        ));
    }
    let expanded = ctx.expand(path);
    if let Some(d) = git_files::check_file(&expanded) {
        return Some(deny(
            format!("{interp} write to git file blocked: {}", d.reason),
            &d.fix,
        ));
    }
    if how != Source::Named && git_state(&expanded, how == Source::Composed) {
        return Some(deny(
            format!(
                "{interp} write to the git directory blocked: {} is git's own state",
                expanded.display()
            ),
            "leave the git directory to git: change branches, refs and the index with git commands",
        ));
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
