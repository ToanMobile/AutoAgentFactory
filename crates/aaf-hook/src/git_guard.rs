//! Blocks git calls that rewrite shared history, delete protected branches
//! or switch the repository's hooks off.

use std::fs;
use std::path::{Path, PathBuf};

use crate::shell::Cmd;
use crate::{Ctx, Denial, base_ref, git_alias, git_files, paths};

mod push;
pub(crate) use push::current_branch;

const RULE: &str = "git_guard";

/// A git invocation with its global options peeled off.
pub struct GitCall<'a> {
    /// Last `-C <dir>` (kept for single-`-C` callers; prefer [`Self::resolve_dir`]
    /// when multiple `-C` options may be chained).
    pub dir: Option<&'a str>,
    /// Every `-C <dir>` in order, applied sequentially by git.
    pub dirs: Vec<&'a str>,
    /// A global option sets `core.hooksPath`.
    pub hooks_override: bool,
    /// The global options, in order.
    pub globals: &'a [String],
    /// `-c alias.<name>=<value>` definitions.
    pub aliases: Vec<Alias<'a>>,
    /// Every `-c <key>=<value>` pair passed as a global option.
    pub config_pairs: Vec<(&'a str, &'a str)>,
    /// `--config-env=<key>=<var>` entries whose `<key>` runs a program.
    pub config_env_programs: Vec<(&'a str, Option<&'a str>)>,
    /// An alias whose value is not on the command line (`--config-env`).
    pub hidden_alias: bool,
    /// A global option includes a configuration file (`include.path`).
    pub includes_file: bool,
    /// A global option sets a configuration key that is not literal.
    pub unknown_key: bool,
    pub sub: &'a str,
    pub rest: &'a [String],
}

impl GitCall<'_> {
    /// Resolves all `-C <dir>` flags sequentially starting from `ctx.cwd`,
    /// matching git's own handling of multiple `-C` options.
    pub fn resolve_dir(&self, ctx: &Ctx) -> PathBuf {
        let mut cur = ctx.cwd.clone();
        for dir in &self.dirs {
            cur = paths::expand_real_dotdot(dir, &cur, ctx.env.home.as_deref());
        }
        cur
    }

    /// Resolves all `-C <dir>` flags before the last one when two or more `-C`
    /// flags are present, so callers that apply `self.dir` once still see the
    /// cumulative working directory.
    pub(crate) fn resolve_prefix_dir(&self, ctx: &Ctx) -> Option<PathBuf> {
        let (_, prefix) = self.dirs.split_last()?;
        if prefix.is_empty() {
            return None;
        }
        let mut cur = ctx.cwd.clone();
        for dir in prefix {
            cur = paths::expand_real_dotdot(dir, &cur, ctx.env.home.as_deref());
        }
        Some(cur)
    }

    /// Last `--git-dir` value in `self.globals`, if any.
    pub(crate) fn explicit_git_dir(&self) -> Option<&str> {
        let mut found = None;
        let mut at = 0;
        while let Some(arg) = self.globals.get(at).map(String::as_str) {
            if let Some(val) = arg.strip_prefix("--git-dir=") {
                found = Some(val);
                at += 1;
            } else if arg == "--git-dir" {
                found = self.globals.get(at + 1).map(String::as_str);
                at += 2;
            } else if matches!(
                arg,
                "-C" | "-c"
                    | "--work-tree"
                    | "--namespace"
                    | "--config-env"
                    | "--attr-source"
                    | "--super-prefix"
            ) {
                at += 2;
            } else {
                at += 1;
            }
        }
        found
    }
}

pub struct Alias<'a> {
    /// Index of its `-c` in [`GitCall::globals`].
    pub at: usize,
    /// Words it takes there: 2 for `-c k=v`, 1 for the glued `-ck=v`.
    pub width: usize,
    pub name: &'a str,
    pub value: &'a str,
}

pub fn split(args: &[String]) -> GitCall<'_> {
    let mut at = 0;
    let mut dir = None;
    let mut dirs = Vec::new();
    let mut hooks_override = false;
    let mut aliases = Vec::new();
    let mut config_pairs = Vec::new();
    let mut config_env_programs = Vec::new();
    let (mut hidden_alias, mut includes_file, mut unknown_key) = (false, false, false);
    while let Some(arg) = args.get(at) {
        if !arg.starts_with('-') {
            break;
        }
        // `-c<key=value>` and `-C<dir>`: the value glued to its option is
        // read like the separate word.
        let glued = ["-c", "-C"].iter().find_map(|flag| {
            let rest = arg.strip_prefix(flag).filter(|rest| !rest.is_empty())?;
            Some((*flag, rest))
        });
        let takes_value = matches!(
            arg.as_str(),
            "-C" | "-c"
                | "--git-dir"
                | "--work-tree"
                | "--namespace"
                | "--config-env"
                | "--attr-source"
                | "--super-prefix"
        );
        let (flag, value, width) = match glued {
            Some((flag, rest)) => (flag, Some(rest), 1),
            None if takes_value => (arg.as_str(), args.get(at + 1).map(String::as_str), 2),
            None => (arg.as_str(), None, 1),
        };
        if flag == "-C" {
            dir = value;
            if let Some(v) = value {
                dirs.push(v);
            }
        }
        if mentions_hooks(arg) || value.is_some_and(mentions_hooks) {
            hooks_override = true;
        }
        if flag == "-c"
            && let Some(pair) = value
        {
            let (key, value) = match pair.split_once('=') {
                Some((key, value)) => (key, Some(value)),
                None => (pair, None),
            };
            includes_file |= is_include(key);
            unknown_key |= !is_literal(key);
            if let Some(val) = value {
                config_pairs.push((key, val));
                if let Some(name) = alias_name(key) {
                    aliases.push(Alias {
                        at,
                        width,
                        name,
                        value: val,
                    });
                }
            }
        }
        let env_key = match arg.strip_prefix("--config-env") {
            Some("") => value,
            Some(attached) => attached.strip_prefix('='),
            None => None,
        };
        if let Some(pair) = env_key {
            let (key, env_var) = pair
                .rsplit_once('=')
                .map_or((pair, None), |(key, var)| (key, Some(var)));
            hidden_alias |= alias_name(key).is_some();
            includes_file |= is_include(key);
            unknown_key |= !is_literal(key);
            if crate::git_persist::runs_program(key) {
                config_env_programs.push((key, env_var));
            }
        }
        at += width;
    }
    GitCall {
        dir,
        dirs,
        hooks_override,
        globals: args.get(..at).unwrap_or(args),
        aliases,
        config_pairs,
        config_env_programs,
        hidden_alias,
        includes_file,
        unknown_key,
        sub: args.get(at).map_or("", String::as_str),
        rest: args.get(at + 1..).unwrap_or(&[]),
    }
}

/// `include.path` and `includeIf.<condition>.path` pull in a file whose
/// aliases and hooks settings the hook never sees.
fn is_include(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key == "include.path" || key.starts_with("includeif.")
}

/// A key the shell fills in at run time could be any of the refused ones.
pub(crate) fn is_literal(key: &str) -> bool {
    !key.is_empty() && !key.contains(['$', '`'])
}

/// The variable when `text` is `VAR=<file>` pointing git at another
/// configuration file. `/dev/null` defines nothing and is fine.
fn config_file_var(text: &str) -> Option<&str> {
    let (key, value) = text.split_once('=')?;
    (matches!(
        key,
        "GIT_CONFIG_GLOBAL" | "GIT_CONFIG_SYSTEM" | "GIT_CONFIG"
    ) && value != "/dev/null")
        .then_some(key)
}

/// Text-level check: the word brings in git configuration from a file.
pub fn reads_unseen_config(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("include.path")
        || lower.contains("includeif.")
        || config_file_var(text).is_some()
}

/// `f` for the config key `alias.f` (section names ignore case).
pub fn alias_name(key: &str) -> Option<&str> {
    let (section, name) = key.split_at_checked(6)?;
    section.eq_ignore_ascii_case("alias.").then_some(name)
}

fn mentions_hooks(text: &str) -> bool {
    text.to_ascii_lowercase().contains("core.hookspath")
}

/// Any non-ASCII name counts: APFS folds `maﬆer` and `MAſTER` onto the loose
/// ref of `master`, and git resolves them through it.
pub fn is_protected(branch: &str) -> bool {
    is_protected_with(branch, None)
}

/// Like [`is_protected`], also treating a configured `[gate].base_ref` branch
/// (case-insensitive) as protected.
pub(crate) fn is_protected_with(branch: &str, extra_base: Option<&str>) -> bool {
    let lower = branch.to_ascii_lowercase();
    let branch = lower.strip_prefix("refs/heads/").unwrap_or(&lower);
    !branch.is_ascii()
        || branch == "main"
        || branch == "master"
        || branch.starts_with("release/")
        || extra_base.is_some_and(|extra| branch.eq_ignore_ascii_case(extra))
}

/// git accepts any unambiguous prefix of a long option.
pub(crate) fn abbrev(arg: &str, full: &str, min: usize) -> bool {
    let flag = arg.split('=').next().unwrap_or(arg);
    flag.len() >= min && full.starts_with(flag)
}

fn is_cluster(arg: &str) -> bool {
    arg.len() > 1 && arg.starts_with('-') && !arg.starts_with("--")
}

/// Arguments up to a literal `--`.
pub(crate) fn options(rest: &[String]) -> impl Iterator<Item = &str> {
    rest.iter().map(String::as_str).take_while(|a| *a != "--")
}

pub(crate) type Verdict = Option<(String, &'static str)>;

const FIX_HOOKS: &str = "run the command with the repository hooks enabled";
const FIX_CONFIG: &str = "run git on its normal configuration: no include.path, no -c key built at run time, no GIT_CONFIG_GLOBAL/GIT_CONFIG_SYSTEM/HOME override";
const FIX_FORCE: &str =
    "push without force; to update your own feature branch use --force-with-lease and name it";
const FIX_PROTECTED: &str = "leave main, master and release/* alone; open a pull request instead";

fn strictly_under(child: &Path, parent: &Path) -> bool {
    child != parent && child.starts_with(parent)
}

fn overlaps_dir(a: &Path, b: &Path) -> bool {
    let real_b = paths::real(b);
    a == real_b || a.starts_with(&real_b) || real_b.starts_with(a)
}

/// True iff `path` exists on disk, canonicalizes cleanly strictly inside
/// `/tmp` or `$TMPDIR` (never `/tmp` or `$TMPDIR` itself), and does not
/// resolve into or overlap the workspace repository.
fn is_safe_tmp_dir(path: &Path, ctx: &Ctx) -> bool {
    let Ok(canon) = fs::canonicalize(path) else {
        return false;
    };
    let in_tmp = fs::canonicalize("/tmp")
        .ok()
        .is_some_and(|tmp| strictly_under(&canon, &tmp))
        || strictly_under(&canon, Path::new("/tmp"))
        || strictly_under(&canon, Path::new("/private/tmp"));
    let in_tmpdir = std::env::var_os("TMPDIR")
        .filter(|v| !v.is_empty())
        .and_then(|v| fs::canonicalize(v).ok())
        .is_some_and(|td| strictly_under(&canon, &td));
    (in_tmp || in_tmpdir)
        && !ctx.root().is_some_and(|r| overlaps_dir(&canon, &r))
        && !ctx
            .workspace_root()
            .is_some_and(|r| overlaps_dir(&canon, &r))
        && !(ctx.root().is_none()
            && !ctx.workspace_cwd.as_os_str().is_empty()
            && overlaps_dir(&canon, &ctx.workspace_cwd))
}

fn is_safe_tmp_raw(raw: &str, ctx: &Ctx) -> bool {
    !raw.is_empty()
        && !paths::has_unresolved_var(raw)
        && is_safe_tmp_dir(&ctx.expand_real_dotdot(raw), ctx)
}

pub fn check(cmd: &Cmd, ctx: &Ctx) -> Option<Denial> {
    let deny = |(reason, fix): (String, &'static str)| Denial::new(RULE, reason, fix);
    if let Some(verdict) = env_override(cmd, ctx) {
        return Some(deny(verdict));
    }
    // A command word built from an expansion could be git: judge its
    // arguments as if it were.
    if cmd.name() != "git" && !cmd.dynamic_name {
        return None;
    }
    let call = split(cmd.args());
    if call.hooks_override {
        return Some(deny((
            "a global option overrides core.hooksPath".to_string(),
            FIX_HOOKS,
        )));
    }
    if call.includes_file {
        return Some(deny((
            "a global option includes a configuration file the hook cannot read".to_string(),
            FIX_CONFIG,
        )));
    }
    // What follows is refused for git itself only: `$SHELL -c "$CMD"` and
    // `HOME=… npm test` are other programs' business.
    if !cmd.dynamic_name {
        if call.unknown_key {
            return Some(deny((
                "a global option sets a configuration key chosen at run time".to_string(),
                FIX_CONFIG,
            )));
        }
        let moved = cmd.assigns.iter().find(|(key, val)| {
            (key == "HOME" && !is_safe_tmp_raw(val, ctx)) || key == "XDG_CONFIG_HOME"
        });
        if let Some((key, _)) = moved {
            return Some(deny((
                format!("{key} moves git's global configuration to a place the hook cannot read"),
                FIX_CONFIG,
            )));
        }
    }
    if let Some(denial) = git_alias::check(&call, cmd, ctx)
        .or_else(|| git_files::check_git_aaf_refs(call.sub, call.rest))
    {
        return Some(denial);
    }
    let base = || base_ref::for_dir(&call.resolve_dir(ctx), ctx);
    let verdict = match call.sub {
        "push" => push::push(&call, ctx, base().as_deref()),
        "commit" => commit(call.rest),
        "reset" => options(call.rest)
            .any(|a| abbrev(a, "--hard", 3))
            .then(|| {
                let target = call.resolve_dir(ctx);
                let safe_scratch = !call.dirs.is_empty()
                    && call.aliases.is_empty()
                    && call.explicit_git_dir().is_none()
                    && !ctx.line.git_dir_moved
                    && is_safe_tmp_dir(&target, ctx)
                    && target.join(".git").exists();
                (!safe_scratch).then(|| {
                    (
                        "reset --hard discards uncommitted work".to_string(),
                        "use git stash, or git reset --soft/--mixed",
                    )
                })
            })
            .flatten()
            .or_else(|| no_verify(call.rest)),
        "branch" => branch_delete(call.rest, base().as_deref()).or_else(|| no_verify(call.rest)),
        "update-ref" => update_ref(call.rest, base().as_deref()),
        "config" => config(call.rest),
        _ => no_verify(call.rest),
    };
    verdict.map(deny)
}

/// `GIT_CONFIG_KEY_0=core.hooksPath …` does what `-c core.hooksPath=` does.
fn env_override(cmd: &Cmd, ctx: &Ctx) -> Verdict {
    let exported = matches!(cmd.name(), "export" | "declare" | "typeset" | "readonly");
    let direct_git = cmd.name() == "git" && !cmd.dynamic_name;
    let assigned = cmd
        .assigns
        .iter()
        .map(|(key, value)| format!("{key}={value}"));
    let words = cmd.words.iter().filter(|_| exported).cloned();
    // An alias defined this way has its value in another variable or in a
    // file: it cannot be analysed, so it is refused like the hooks override.
    assigned
        .chain(words)
        .filter(|text| text.starts_with("GIT_CONFIG"))
        .find_map(|text| {
            let (key, value) = text.split_once('=').unwrap_or((text.as_str(), ""));
            // `GIT_CONFIG_KEY_<n>` and `GIT_CONFIG_PARAMETERS` carry keys.
            let carries_key = key.starts_with("GIT_CONFIG_KEY_") || key == "GIT_CONFIG_PARAMETERS";
            if mentions_hooks(&text) {
                Some(("GIT_CONFIG_* sets core.hooksPath".to_string(), FIX_HOOKS))
            } else if text.to_ascii_lowercase().contains("alias.") {
                Some((
                    "GIT_CONFIG_* defines a git alias".to_string(),
                    git_alias::FIX,
                ))
            } else if let Some(var) = config_file_var(&text) {
                if direct_git && var == "GIT_CONFIG_GLOBAL" && is_safe_tmp_raw(value, ctx) {
                    None
                } else {
                    Some((
                        format!("{var} points git at a configuration file the hook cannot read"),
                        FIX_CONFIG,
                    ))
                }
            } else if reads_unseen_config(&text) {
                Some((
                    "GIT_CONFIG_* includes a configuration file the hook cannot read".to_string(),
                    FIX_CONFIG,
                ))
            } else if carries_key && text.contains('=') && !is_literal(value) {
                Some((
                    format!("{key} sets a configuration key chosen at run time"),
                    FIX_CONFIG,
                ))
            } else {
                None
            }
        })
}

fn no_verify(rest: &[String]) -> Verdict {
    options(rest).any(|a| abbrev(a, "--no-verify", 6)).then(|| {
        (
            "--no-verify skips the repository hooks".to_string(),
            FIX_HOOKS,
        )
    })
}

fn commit(rest: &[String]) -> Verdict {
    const VALUED_LONG: &[&str] = &[
        "--message",
        "--file",
        "--author",
        "--date",
        "--reuse-message",
        "--reedit-message",
        "--fixup",
        "--squash",
        "--template",
        "--cleanup",
        "--trailer",
        "--pathspec-from-file",
    ];
    let mut at = 0;
    while let Some(arg) = rest.get(at).map(String::as_str) {
        at += 1;
        if arg == "--" {
            break;
        }
        if arg.starts_with("--") {
            if abbrev(arg, "--no-verify", 6) {
                return no_verify(rest);
            }
            if VALUED_LONG.contains(&arg) {
                at += 1;
            }
        } else if is_cluster(arg) {
            for (index, flag) in arg.char_indices().skip(1) {
                match flag {
                    'n' => {
                        return Some((
                            "commit -n is --no-verify and skips the hooks".to_string(),
                            FIX_HOOKS,
                        ));
                    }
                    // The value is the rest of the cluster or the next word.
                    'm' | 'F' | 'C' | 'c' | 't' => {
                        if index + flag.len_utf8() == arg.len() {
                            at += 1;
                        }
                        break;
                    }
                    // Optional value, always attached.
                    'u' | 'S' => break,
                    _ => {}
                }
            }
        }
    }
    None
}

fn branch_delete(rest: &[String], extra_base: Option<&str>) -> Verdict {
    let mut delete = false;
    let mut names = Vec::new();
    let mut only_names = false;
    for arg in rest.iter().map(String::as_str) {
        if only_names {
            names.push(arg);
        } else if arg == "--" {
            only_names = true;
        } else if arg.starts_with("--") {
            delete |= abbrev(arg, "--delete", 3);
        } else if is_cluster(arg) {
            delete |= arg.contains(['d', 'D']);
        } else {
            names.push(arg);
        }
    }
    let branch = names
        .into_iter()
        .find(|name| is_protected_with(name, extra_base))?;
    delete.then(|| {
        (
            format!("this deletes the protected branch {branch}"),
            FIX_PROTECTED,
        )
    })
}

fn update_ref(rest: &[String], extra_base: Option<&str>) -> Verdict {
    let delete = options(rest).any(|a| a == "-d" || a == "--delete");
    // The ref may follow a literal `--`.
    let branch = rest
        .iter()
        .map(String::as_str)
        .find(|a| !a.starts_with('-') && is_protected_with(a, extra_base))?;
    delete.then(|| {
        (
            format!("update-ref -d removes the protected ref {branch}"),
            FIX_PROTECTED,
        )
    })
}

/// Reading `core.hooksPath` is fine; setting or unsetting it is not.
fn config(rest: &[String]) -> Verdict {
    // An option's value (`--comment get`) is not the verb.
    let (flags, positional) = crate::git_persist::config_args(rest);
    let key = positional
        .iter()
        .position(|a| mentions_hooks(a) || is_include(a))?;
    let includes = positional.get(key).is_some_and(|a| is_include(a));
    let verb = positional.first().copied().unwrap_or("");
    let writes = matches!(verb, "set" | "unset" | "unset-all")
        || flags.iter().any(|a| {
            abbrev(a, "--unset", 4)
                || abbrev(a, "--unset-all", 8)
                || abbrev(a, "--add", 4)
                || abbrev(a, "--replace-all", 5)
        })
        || (verb != "get" && key + 1 < positional.len());
    writes.then(|| {
        if includes {
            (
                "config stores an include of a file the hook cannot read".to_string(),
                FIX_CONFIG,
            )
        } else {
            ("config changes core.hooksPath".to_string(), FIX_HOOKS)
        }
    })
}
