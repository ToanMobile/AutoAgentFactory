//! Blocks git calls that rewrite shared history, delete protected branches
//! or switch the repository's hooks off.

use std::fs;

use crate::shell::Cmd;
use crate::{Ctx, Denial, git_alias, git_files, paths};

const RULE: &str = "git_guard";

/// A git invocation with its global options peeled off.
pub struct GitCall<'a> {
    /// Last `-C <dir>`.
    pub dir: Option<&'a str>,
    /// A global option sets `core.hooksPath`.
    pub hooks_override: bool,
    /// The global options, in order.
    pub globals: &'a [String],
    /// `-c alias.<name>=<value>` definitions.
    pub aliases: Vec<Alias<'a>>,
    /// An alias whose value is not on the command line (`--config-env`).
    pub hidden_alias: bool,
    /// A global option includes a configuration file (`include.path`).
    pub includes_file: bool,
    /// A global option sets a configuration key that is not literal.
    pub unknown_key: bool,
    pub sub: &'a str,
    pub rest: &'a [String],
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
    let mut hooks_override = false;
    let mut aliases = Vec::new();
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
            if let (Some(name), Some(value)) = (alias_name(key), value) {
                aliases.push(Alias {
                    at,
                    width,
                    name,
                    value,
                });
            }
        }
        let env_key = match arg.strip_prefix("--config-env") {
            Some("") => value,
            Some(attached) => attached.strip_prefix('='),
            None => None,
        };
        if let Some(pair) = env_key {
            let key = pair.rsplit_once('=').map_or(pair, |(key, _)| key);
            hidden_alias |= alias_name(key).is_some();
            includes_file |= is_include(key);
            unknown_key |= !is_literal(key);
        }
        at += width;
    }
    GitCall {
        dir,
        hooks_override,
        globals: args.get(..at).unwrap_or(args),
        aliases,
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
fn is_literal(key: &str) -> bool {
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
    let lower = branch.to_ascii_lowercase();
    let branch = lower.strip_prefix("refs/heads/").unwrap_or(&lower);
    !branch.is_ascii() || branch == "main" || branch == "master" || branch.starts_with("release/")
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

pub fn check(cmd: &Cmd, ctx: &Ctx) -> Option<Denial> {
    let deny = |(reason, fix): (String, &'static str)| Denial::new(RULE, reason, fix);
    if let Some(verdict) = env_override(cmd) {
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
        let moved = cmd
            .assigns
            .iter()
            .find(|(key, _)| key == "HOME" || key == "XDG_CONFIG_HOME");
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
    let verdict = match call.sub {
        "push" => push(&call, ctx),
        "commit" => commit(call.rest),
        "reset" => options(call.rest)
            .any(|a| abbrev(a, "--hard", 3))
            .then(|| {
                (
                    "reset --hard discards uncommitted work".to_string(),
                    "use git stash, or git reset --soft/--mixed",
                )
            })
            .or_else(|| no_verify(call.rest)),
        "branch" => branch_delete(call.rest).or_else(|| no_verify(call.rest)),
        "update-ref" => update_ref(call.rest),
        "config" => config(call.rest),
        _ => no_verify(call.rest),
    };
    verdict.map(deny)
}

/// `GIT_CONFIG_KEY_0=core.hooksPath …` does what `-c core.hooksPath=` does.
fn env_override(cmd: &Cmd) -> Verdict {
    let exported = matches!(cmd.name(), "export" | "declare" | "typeset" | "readonly");
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
                Some((
                    format!("{var} points git at a configuration file the hook cannot read"),
                    FIX_CONFIG,
                ))
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

fn push(call: &GitCall, ctx: &Ctx) -> Verdict {
    let (mut force, mut lease, mut delete, mut dry_run) = (false, false, false, false);
    let (mut skip_hooks, mut all, mut repo_option) = (false, false, false);
    let mut positional: Vec<&str> = Vec::new();
    let mut only_positional = false;
    let mut at = 0;
    while let Some(arg) = call.rest.get(at).map(String::as_str) {
        at += 1;
        if only_positional || !arg.starts_with('-') || arg == "-" {
            positional.push(arg);
        } else if arg == "--" {
            only_positional = true;
        } else if arg.starts_with("--") {
            let flag = arg.split('=').next().unwrap_or(arg);
            let is_repo = abbrev(flag, "--repo", 5);
            if is_repo
                || abbrev(flag, "--receive-pack", 6)
                || abbrev(flag, "--exec", 3)
                || abbrev(flag, "--push-option", 4)
            {
                // The value is attached or the next word; either way it is
                // not a flag, even when it looks like `--dry-run`.
                repo_option |= is_repo;
                if flag == arg {
                    at += 1;
                }
            } else if flag == "--dry-run" {
                dry_run = true;
            } else if flag.starts_with("--no-d") {
                // `--no-dry-run` and its abbreviations: the last one wins,
                // as in git. An ambiguous prefix is read the unsafe way.
                dry_run = false;
            } else if abbrev(flag, "--no-verify", 6) {
                skip_hooks = true;
            } else if flag.starts_with("--force-w") {
                lease = true;
            } else if flag.starts_with("--force-i") {
                // --force-if-includes only tightens a lease.
            } else if abbrev(flag, "--force", 3)
                || flag.starts_with("--force")
                || flag == "--mirror"
            {
                force = true;
            } else if abbrev(flag, "--delete", 5) {
                delete = true;
            } else if flag == "--all" || flag == "--branches" {
                all = true;
            }
        } else {
            for (index, flag) in arg.char_indices().skip(1) {
                match flag {
                    'f' => force = true,
                    'n' => dry_run = true,
                    'd' => delete = true,
                    'o' => {
                        if index + 1 == arg.len() {
                            at += 1;
                        }
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
    if dry_run {
        return None;
    }
    if skip_hooks {
        return no_verify(call.rest);
    }
    // With `--repo` naming the remote, no positional is known to be the
    // remote: each one is judged as a refspec.
    let first_refspec = if repo_option { 0 } else { 1 };
    let refspecs = positional.get(first_refspec..).unwrap_or(&[]);
    if force {
        return Some((
            "push --force overwrites remote history".to_string(),
            FIX_FORCE,
        ));
    }
    if let Some(spec) = refspecs.iter().find(|spec| spec.starts_with('+')) {
        return Some((format!("refspec {spec} is a force push"), FIX_FORCE));
    }
    let target = |spec: &str| -> String {
        let spec = spec.trim_start_matches('+');
        let dst = spec.rsplit(':').next().unwrap_or(spec);
        dst.strip_prefix("refs/heads/").unwrap_or(dst).to_string()
    };
    let deleted = refspecs
        .iter()
        .filter(|spec| delete || spec.starts_with(':'))
        .map(|spec| target(spec))
        .find(|branch| is_protected(branch));
    if let Some(branch) = deleted {
        return Some((
            format!("this deletes the protected branch {branch}"),
            FIX_PROTECTED,
        ));
    }
    if !lease {
        return None;
    }
    if all {
        return Some((
            "--force-with-lease on every branch includes protected ones".to_string(),
            FIX_PROTECTED,
        ));
    }
    let mut targets: Vec<String> = refspecs.iter().map(|spec| target(spec)).collect();
    if targets.is_empty() {
        targets.push("HEAD".to_string());
    }
    for branch in targets {
        let branch = if branch == "HEAD" || branch == "@" {
            match current_branch(call, ctx) {
                Some(current) => current,
                None => {
                    return Some((
                        "cannot tell which branch --force-with-lease would overwrite".to_string(),
                        "name the remote and the feature branch explicitly",
                    ));
                }
            }
        } else {
            branch
        };
        if branch.contains('$') || paths::has_glob(&branch) {
            return Some((
                format!("--force-with-lease target {branch} is not a literal branch"),
                "write the branch name literally",
            ));
        }
        if is_protected(&branch) {
            return Some((
                format!("--force-with-lease would overwrite the protected branch {branch}"),
                FIX_PROTECTED,
            ));
        }
    }
    None
}

/// Branch checked out where the push runs, read from `HEAD` (no git spawn).
fn current_branch(call: &GitCall, ctx: &Ctx) -> Option<String> {
    let dir = call.dir.map_or_else(|| ctx.cwd.clone(), |d| ctx.expand(d));
    let root = (ctx.env.repo_root)(&dir)?;
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        // A linked worktree: `.git` is a file pointing at the real git dir.
        let pointer = fs::read_to_string(&dot_git).ok()?;
        root.join(pointer.trim().strip_prefix("gitdir:")?.trim())
    };
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    head.trim()
        .strip_prefix("ref: refs/heads/")
        .map(str::to_string)
}

fn branch_delete(rest: &[String]) -> Verdict {
    let mut delete = false;
    let mut names = Vec::new();
    let mut only_names = false;
    for arg in rest.iter().map(String::as_str) {
        if only_names {
            names.push(arg);
        } else if arg == "--" {
            only_names = true;
        } else if arg.starts_with("--") {
            delete |= abbrev(arg, "--delete", 5);
        } else if is_cluster(arg) {
            delete |= arg.contains(['d', 'D']);
        } else {
            names.push(arg);
        }
    }
    let branch = names.into_iter().find(|name| is_protected(name))?;
    delete.then(|| {
        (
            format!("this deletes the protected branch {branch}"),
            FIX_PROTECTED,
        )
    })
}

fn update_ref(rest: &[String]) -> Verdict {
    let delete = options(rest).any(|a| a == "-d" || a == "--delete");
    // The ref may follow a literal `--`.
    let branch = rest
        .iter()
        .map(String::as_str)
        .find(|a| !a.starts_with('-') && is_protected(a))?;
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
    let (_, positional) = crate::git_persist::config_args(rest);
    let key = positional
        .iter()
        .position(|a| mentions_hooks(a) || is_include(a))?;
    let includes = positional.get(key).is_some_and(|a| is_include(a));
    let verb = positional.first().copied().unwrap_or("");
    let writes = matches!(verb, "set" | "unset")
        || rest.iter().any(|a| {
            matches!(
                a.as_str(),
                "--unset" | "--unset-all" | "--add" | "--replace-all"
            )
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
