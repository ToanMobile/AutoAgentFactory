//! The files git obeys on every later command, the user's own included:
//! the configuration (aliases, `core.hooksPath`, keys that name a program),
//! the hook scripts, and the refs of the protected branches. Reading them is
//! fine; an agent does not write them.
//!
//! There is no list of "writing" programs: any program handed one of these
//! paths is taken to write it unless it is on the short list of programs
//! that only read. The earlier writer list missed `tar -C`, `curl -o`,
//! `unzip -d` and `ditto`, and would have missed the next one too.

use std::path::{Component, Path};

use crate::git_guard::is_protected;
use crate::shell::{Cmd, DECLARERS, SHELLS};
use crate::{Ctx, Denial, paths, sed_script};

const RULE: &str = "git_guard";
const FIX: &str = "leave git's hooks, control files, and protected refs untouched; read them with \
                   cat/ls/grep, use ordinary `git config <key> <value>` for non-executable \
                   settings, and complete the task on your feature branch; this needs a human: \
                   finish the task without it and name the step in your final message";

const HOOK: &str = "a git hook";
const CONFIG: &str = "git's configuration";
const EXCLUDE: &str = "git's local exclude rules";
/// The directory itself, or one that holds the control files of a linked
/// worktree or a submodule: moving, replacing or unpacking into it brings
/// hooks and configuration along.
const GIT_DIR: &str = "the git directory";
const REF: &str = "a protected branch";
const AAF_REF: &str = "refs/aaf/* snapshots";
const AAF_REF_REASON: &str = "refs/aaf/* snapshots are protected";

/// Programs that do not write the files named to them. `find` and the
/// non-in-place `sed`/`perl` are decided in [`reads_only`].
const READERS: &[&str] = &[
    "cat",
    "ls",
    "head",
    "tail",
    "less",
    "more",
    "bat",
    "grep",
    "egrep",
    "fgrep",
    "rg",
    "ag",
    "fd",
    "tree",
    "wc",
    "file",
    "stat",
    "du",
    "diff",
    "cmp",
    "md5",
    "md5sum",
    "shasum",
    "sha1sum",
    "sha256sum",
    "cksum",
    "od",
    "hexdump",
    "strings",
    "cut",
    "nl",
    "jq",
    "test",
    "[",
    "[[",
    "readlink",
    "realpath",
    "basename",
    "dirname",
    "echo",
    "printf",
    "true",
    "false",
    "which",
    "type",
    "pwd",
    "cd",
    "pushd",
    "popd",
    "source",
    ".",
];
/// These only read what they copy from: the last name is the one written.
/// Copying a sample hook out of the repository stays possible.
const COPIERS: &[&str] = &["cp", "install", "rsync", "ditto", "scp"];
/// `find` primaries that delete, run or write.
const FIND_ACTS: &[&str] = &[
    "-delete", "-exec", "-execdir", "-ok", "-okdir", "-fprint", "-fprint0", "-fprintf", "-fls",
];
/// `find` primaries whose value is a pattern, not a place to work in.
const FIND_TESTS: &[&str] = &[
    "-path",
    "-ipath",
    "-wholename",
    "-iwholename",
    "-name",
    "-iname",
    "-regex",
    "-iregex",
];
/// Their arguments carry prose (commit messages, pull request bodies).
const MESSAGE_TOOLS: &[&str] = &["git", "gh"];

/// A path component is `name`, or a glob that can expand to it. Folded as
/// the default macOS file system compares names (`hooKs`, `conﬁg`); a glob
/// reaches a dot name only when it starts with the dot itself.
fn is(part: &str, name: &str) -> bool {
    paths::fold(part).eq_ignore_ascii_case(name)
        || (paths::has_glob(part)
            && (!name.starts_with('.') || part.starts_with('.'))
            && paths::glob_match(part, name))
}

/// What `path` is when it is one of the guarded files: the user's global
/// configuration; `config` and `config.worktree` of a git directory (of the
/// repository, of a linked worktree under `worktrees/<name>/`, of a
/// submodule under `modules/`); everything in a `hooks` directory; the git
/// directory itself; the refs of the protected branches.
fn guarded(path: &Path) -> Option<&'static str> {
    let parts: Vec<&str> = path
        .components()
        .filter_map(|part| match part {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect();
    match parts.as_slice() {
        [.., name] if is(name, ".gitconfig") => return Some(CONFIG),
        [.., dir, git, name] if is(dir, ".config") && is(git, "git") && is(name, "config") => {
            return Some(CONFIG);
        }
        ["etc", "gitconfig"] => return Some(CONFIG),
        _ => {}
    }
    if parts.windows(2).any(|pair| {
        paths::fold(pair[0]).eq_ignore_ascii_case("refs")
            && paths::fold(pair[1]).eq_ignore_ascii_case("aaf")
    }) {
        return Some(AAF_REF);
    }
    let at = parts.iter().position(|part| is(part, ".git"))?;
    let below = parts.get(at + 1..).unwrap_or(&[]);
    if below.iter().any(|part| is(part, "hooks")) {
        return Some(HOOK);
    }
    match below {
        [.., last] if is(last, "config") || is(last, "config.worktree") => Some(CONFIG),
        [.., info, name]
            if is(info, "info") && (is(name, "exclude") || is(name, "exclude.lock")) =>
        {
            Some(EXCLUDE)
        }
        [info] if is(info, "info") => Some(EXCLUDE),
        [.., last] if is(last, "commondir") || is(last, "gitdir") => Some(GIT_DIR),
        [] => Some(GIT_DIR),
        [first] | [first, _] if is(first, "worktrees") || is(first, "modules") => Some(GIT_DIR),
        [name] if is(name, "packed-refs") => Some(REF),
        [refs] if is(refs, "refs") => Some(REF),
        [refs, aaf, ..] if is(refs, "refs") && is(aaf, "aaf") => Some(AAF_REF),
        [refs, heads, branch @ ..] if is(refs, "refs") && is(heads, "heads") => {
            let branch = branch.join("/").to_ascii_lowercase();
            // The directory of all branches or of `release/*`, a protected
            // branch, or a glob that could name one.
            (branch.is_empty()
                || branch == "release"
                || is_protected(&branch)
                || paths::has_glob(&branch))
            .then_some(REF)
        }
        _ => None,
    }
}

/// By the path as written, or by the file a symlink leads to.
fn hit(path: &Path) -> Option<&'static str> {
    guarded(path).or_else(|| guarded(&paths::real(path)))
}

/// Like [`hit`], using [`Ctx::real_of`] so non-existent tokens under `ctx.cwd`
/// resolve with a single `lstat` instead of walking ancestors.
fn hit_ctx(path: &Path, ctx: &Ctx) -> Option<&'static str> {
    guarded(path).or_else(|| guarded(&ctx.real_of(path)))
}

/// Like [`hit_ctx`], also resolving symlinks before `..` when `raw` contains
/// `..` (e.g. `link/../hooks/pre-commit` where `link -> .git/objects`).
fn hit_raw(raw: &str, path: &Path, ctx: &Ctx) -> Option<&'static str> {
    hit_ctx(path, ctx).or_else(|| {
        if !raw.contains("..") {
            return None;
        }
        let real_dd = paths::expand_real_dotdot(raw, &ctx.cwd, ctx.env.home.as_deref());
        (real_dd != path).then(|| hit_ctx(&real_dd, ctx)).flatten()
    })
}

fn deny(what: &str, path: &Path, kind: &str) -> Denial {
    Denial::new(
        RULE,
        format!("{what} {} would change {kind}", path.display()),
        FIX,
    )
}

/// `Write|Edit|MultiEdit` on a path with `.`/`..` already removed.
pub fn check_file(path: &Path) -> Option<Denial> {
    hit(path).map(|kind| deny("writing", path, kind))
}

/// `sed -i`/`-I`, `perl -i`: the only forms of these two that write their input.
fn edits_in_place(cmd: &Cmd) -> bool {
    let short = |arg: &String| arg.starts_with('-') && !arg.starts_with("--");
    match cmd.name() {
        "sed" => sed_script::edits_in_place(cmd.args()),
        "perl" => cmd.args().iter().any(|arg| short(arg) && arg.contains('i')),
        _ => false,
    }
}

fn reads_only(cmd: &Cmd) -> bool {
    let name = cmd.name();
    match name {
        "sed" => !edits_in_place(cmd),
        // `perl -e 'open(my $f, ">", $ARGV[0])' <file>` writes its file.
        "perl" => !edits_in_place(cmd) && !crate::interp_guard::perl_inline_writes(cmd.args()),
        "find" => !cmd
            .args()
            .iter()
            .any(|arg| FIND_ACTS.contains(&arg.as_str())),
        // `export GIT_DIR=…/.git` names a path and writes nothing.
        _ => READERS.contains(&name) || SHELLS.contains(&name) || DECLARERS.contains(&name),
    }
}

/// The texts of one word that may be a path, and whether each is looked at
/// by its spelling only: the word, the value of `key=value` (`of=<file>`,
/// `--output=<file>`), and for a short option every tail that could be a
/// value glued to it (`-C.git/hooks`). The tails are mostly option letters,
/// so the file system is not asked about them.
fn candidates(word: &str) -> impl Iterator<Item = (&str, bool)> {
    let option = word.starts_with('-');
    let long = word.starts_with("--");
    let value = word
        .split_once('=')
        .filter(|(key, _)| !option || (long && !skips_value(key)))
        .map(|(_, value)| value);
    let tails = word
        .char_indices()
        .skip(2)
        .filter(move |_| option && !long)
        .map(move |(at, _)| (&word[at..], true));
    (!option)
        .then_some(word)
        .into_iter()
        .chain(value)
        .map(|text| (text, false))
        .chain(tails)
}

/// An option whose value says what to leave alone (`--exclude .git`,
/// `--ignore-pattern=.git`): the program does not write there.
fn skips_value(option: &str) -> bool {
    let option = option.to_ascii_lowercase();
    option.starts_with("--") && (option.contains("exclude") || option.contains("ignore"))
}

pub fn check_cmd(cmd: &Cmd, ctx: &Ctx) -> Option<Denial> {
    for redirect in &cmd.redirects {
        let path = ctx.expand(&redirect.target);
        if redirect.op.contains('>')
            && let Some(kind) = hit_raw(&redirect.target, &path, ctx)
        {
            return Some(deny("the redirect to", &path, kind));
        }
    }
    let name = cmd.name();
    if !cmd.dynamic_name && name == "sed" {
        for target in sed_script::script_files(cmd.args(), &['w', 'W']) {
            let path = ctx.expand(&target);
            if let Some(kind) = hit_raw(&target, &path, ctx) {
                return Some(deny(name, &path, kind));
            }
        }
    }
    if cmd.words.is_empty() || (!cmd.dynamic_name && reads_only(cmd)) {
        return None;
    }
    // git's own arguments are judged by `git_guard`; here it is only kept
    // from writing a file into a hooks directory (`format-patch -o`).
    let is_git = name == "git" && !cmd.dynamic_name;
    if !is_git && let Some(kind) = hit_ctx(&ctx.cwd, ctx) {
        // No path needs naming from inside (`cd .git/hooks && curl -O …`).
        return Some(deny(&format!("`{name}` run inside"), &ctx.cwd, kind));
    }
    let prose = !cmd.dynamic_name && MESSAGE_TOOLS.contains(&name);
    let copier = !cmd.dynamic_name && COPIERS.contains(&name);
    let wrapped = cmd.wrapped.iter().filter(|_| !is_git);
    let words: Vec<&str> = wrapped.chain(cmd.args()).map(String::as_str).collect();
    let last_plain = words.iter().rposition(|word| !word.starts_with('-'));
    let mut before = "";
    for (at, word) in words.iter().copied().enumerate() {
        let previous = std::mem::replace(&mut before, word);
        if (skips_value(previous) && !previous.contains('='))
            || (name == "find" && FIND_TESTS.contains(&previous))
            || (prose && word.contains(char::is_whitespace))
        {
            continue;
        }
        if name == "zip" && word == "-x" {
            // What follows are the patterns zip leaves out.
            break;
        }
        // `-t <dir>` and `--target-directory <dir>` name where copies go.
        let target = previous == "--target-directory"
            || (previous.starts_with('-')
                && !previous.starts_with("--")
                && previous.ends_with('t'));
        // A source is only read. It still matters when it is a whole git
        // directory, which lands in the destination with its hooks.
        let source = copier && Some(at) != last_plain && !word.starts_with('-') && !target;
        for (text, by_spelling) in candidates(word) {
            let path = ctx.expand(text);
            let kind = if by_spelling {
                guarded(&path)
            } else {
                hit_raw(text, &path, ctx)
            };
            if let Some(kind) = kind
                && (!source || kind == GIT_DIR)
                && (!is_git || kind == HOOK)
            {
                return Some(deny(name, &path, kind));
            }
        }
    }
    None
}

/// True when `text` references `refs/aaf` (case- and APFS-folded).
pub(crate) fn is_aaf_ref_text(text: &str) -> bool {
    paths::fold(text)
        .replace('\\', "/")
        .to_ascii_lowercase()
        .contains("refs/aaf")
}

/// True when any argument in `words` references `refs/aaf` (case-insensitively).
pub(crate) fn touches_aaf_refs(words: &[String]) -> bool {
    words.iter().any(|word| is_aaf_ref_text(word))
}

/// Denies ref-writing git subcommands that touch `refs/aaf/*`.
pub(crate) fn check_git_aaf_refs(sub: &str, rest: &[String]) -> Option<Denial> {
    const REF_WRITERS: &[&str] = &[
        "update-ref",
        "branch",
        "tag",
        "push",
        "fetch",
        "pull",
        "symbolic-ref",
        "pack-refs",
        "checkout",
        "switch",
        "reset",
        "replace",
        "notes",
        "remote",
        "gc",
        "prune",
        "worktree",
        "receive-pack",
        "fast-import",
    ];
    (REF_WRITERS.contains(&sub) && touches_aaf_refs(rest))
        .then(|| Denial::new(RULE, AAF_REF_REASON, FIX))
}

/// Denies shell commands and write redirects that mutate `refs/aaf/*` files directly.
pub(crate) fn check_shell_aaf_refs(cmd: &Cmd, ctx: &Ctx) -> Option<Denial> {
    for redirect in &cmd.redirects {
        if redirect.op.contains('>')
            && (is_aaf_ref_text(&redirect.target)
                || is_aaf_ref_text(&ctx.expand(&redirect.target).to_string_lossy()))
        {
            return Some(Denial::new(RULE, AAF_REF_REASON, FIX));
        }
    }
    if !cmd.dynamic_name && cmd.name() == "sed" {
        for target in sed_script::script_files(cmd.args(), &['w', 'W']) {
            if is_aaf_ref_text(&target) || is_aaf_ref_text(&ctx.expand(&target).to_string_lossy()) {
                return Some(Denial::new(RULE, AAF_REF_REASON, FIX));
            }
        }
    }
    if cmd.words.is_empty() || (!cmd.dynamic_name && reads_only(cmd)) {
        return None;
    }
    if cmd.name() == "git" && !cmd.dynamic_name {
        return None;
    }
    if is_aaf_ref_text(&ctx.cwd.to_string_lossy()) {
        return Some(Denial::new(RULE, AAF_REF_REASON, FIX));
    }
    let copier = !cmd.dynamic_name && COPIERS.contains(&cmd.name());
    let words: Vec<&str> = cmd
        .wrapped
        .iter()
        .chain(cmd.args())
        .map(String::as_str)
        .collect();
    let last_plain = words.iter().rposition(|w| !w.starts_with('-'));
    let mut before = "";
    for (at, word) in words.iter().copied().enumerate() {
        let prev = std::mem::replace(&mut before, word);
        if skips_value(prev) && !prev.contains('=') {
            continue;
        }
        let target = prev == "--target-directory"
            || (prev.starts_with('-') && !prev.starts_with("--") && prev.ends_with('t'));
        let source = copier && Some(at) != last_plain && !word.starts_with('-') && !target;
        if source {
            continue;
        }
        if is_aaf_ref_text(word)
            || candidates(word)
                .any(|(text, _)| is_aaf_ref_text(&ctx.expand(text).to_string_lossy()))
        {
            return Some(Denial::new(RULE, AAF_REF_REASON, FIX));
        }
    }
    None
}
