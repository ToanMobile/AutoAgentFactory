//! Git state that outlives the command and that the gate and the user's own
//! git calls later trust: stored configuration that names a program, hook
//! templates, and where the protected branches point.

use crate::git_guard::{Verdict, abbrev, is_protected, options, split};
use crate::shell::Cmd;
use crate::{Ctx, Denial};

const RULE: &str = "git_guard";
const FIX_PROGRAM: &str = "leave the configuration keys that run a program to the user; an ordinary \
                           setting (`git config user.name …`) and a boolean value are fine";
const FIX_TEMPLATE: &str = "run git init or clone without a template directory";
const FIX_HOME: &str = "run git with the HOME and XDG_CONFIG_HOME of the session";
const FIX_PROTECTED: &str = "leave main, master and release/* where they are; work on your own \
                             branch and open a pull request";

/// The last part of a configuration key whose value git runs as a program,
/// in any section (`core.fsmonitor`, `filter.<name>.clean`, `diff.external`,
/// `credential.helper`, `merge.<name>.driver`, …). Matching on the variable
/// alone also covers a section renamed afterwards.
const PROGRAM_VARS: &[&str] = &[
    "fsmonitor",
    "sshcommand",
    "pager",
    "editor",
    "askpass",
    "gitproxy",
    "external",
    "textconv",
    "command",
    "cmd",
    "driver",
    "helper",
    "program",
    "clean",
    "smudge",
    "process",
    "difffilter",
    "templatedir",
    "receivepack",
    "uploadpack",
    "packobjectshook",
    "sendmailcmd",
    "tocmd",
    "cccmd",
    "headercmd",
    "vcs",
    "browser",
];
/// Sections where `<tool>.path` is the program to run.
const PATH_SECTIONS: &[&str] = &["difftool", "mergetool", "browser", "man"];
/// Values that switch a feature on or off and name no program.
const BOOLEANS: &[&str] = &["", "true", "false", "yes", "no", "on", "off", "0", "1"];

pub(crate) fn runs_program(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    let Some((section, _)) = key.split_once('.') else {
        return false;
    };
    // An alias is judged as the command it expands to (`git_alias`).
    if section == "alias" {
        return false;
    }
    let var = key.rsplit('.').next().unwrap_or("");
    PROGRAM_VARS.contains(&var)
        // `pager.<command>` is a pager program or a boolean.
        || section == "pager"
        || (section == "submodule" && var == "update")
        || (var == "path" && PATH_SECTIONS.contains(&section))
}

/// `ext::<command>`: a remote "URL" that is a command line for git to run.
pub(crate) fn is_ext(word: &str) -> bool {
    word.get(..5)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("ext::"))
}

/// A branch name or ref of a protected branch. Ignoring case: on the default
/// macOS file system `refs/heads/Main` is the file of `main`.
fn protected(name: &str) -> bool {
    is_protected(&name.to_ascii_lowercase()) || crate::git_files::is_aaf_ref_text(name)
}

fn positional(rest: &[String]) -> impl Iterator<Item = &str> {
    rest.iter()
        .map(String::as_str)
        .filter(|arg| !arg.starts_with('-'))
}

/// `GIT_TEMPLATE_DIR=…`, or `init.templateDir` handed over in `GIT_CONFIG_*`:
/// the next `git init` or `clone` copies its hooks from there.
fn env_names_template(cmd: &Cmd) -> bool {
    let assigned = cmd
        .assigns
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()));
    let words = cmd.words.iter().filter_map(|word| word.split_once('='));
    cmd.sets_var("GIT_TEMPLATE_DIR")
        || assigned.chain(words).any(|(key, value)| {
            key.starts_with("GIT_CONFIG") && value.to_ascii_lowercase().contains("templatedir")
        })
}

pub fn check(cmd: &Cmd, ctx: &Ctx) -> Option<Denial> {
    let deny = |(reason, fix): (String, &'static str)| Denial::new(RULE, reason, fix);
    if env_names_template(cmd) {
        return Some(deny((
            "a template directory is set for git init and clone, which copy its hooks".to_string(),
            FIX_TEMPLATE,
        )));
    }
    if let Some(denial) = crate::git_files::check_shell_aaf_refs(cmd, ctx) {
        return Some(denial);
    }
    // As in `git_guard`: a command word built at run time could be git.
    if cmd.name() != "git" && !cmd.dynamic_name {
        return None;
    }
    if !cmd.dynamic_name && ctx.line.home_moved {
        // The same-command prefix (`HOME=… git …`) is refused by
        // `git_guard`; this is the variable set earlier on the line.
        return Some(deny((
            "HOME or XDG_CONFIG_HOME is set on this line, which moves git's global configuration \
             to a place the hook cannot read"
                .to_string(),
            FIX_HOME,
        )));
    }
    let call = split(cmd.args());
    if call
        .globals
        .iter()
        .any(|option| option.to_ascii_lowercase().contains("init.templatedir"))
    {
        return Some(deny((
            "a global option sets init.templateDir".to_string(),
            FIX_TEMPLATE,
        )));
    }
    if call.rest.iter().any(|arg| is_ext(arg)) {
        return Some(deny((
            "an ext:: remote makes git run the command it spells".to_string(),
            FIX_PROGRAM,
        )));
    }
    let verdict = match call.sub {
        "config" => config(call.rest),
        "init" | "clone" => options(call.rest)
            .any(|arg| abbrev(arg, "--template", 3))
            .then(|| {
                (
                    "--template makes git copy hooks from a directory of your choosing".to_string(),
                    FIX_TEMPLATE,
                )
            }),
        "update-ref" => update_ref(call.rest),
        "branch" => branch(call.rest),
        "checkout" | "switch" | "worktree" => force_create(call.rest),
        "symbolic-ref" => symbolic_ref(call.rest),
        "fetch" | "pull" | "push" => refspecs(call.sub, call.rest),
        _ => None,
    };
    verdict.map(deny)
}

/// `git config` storing a key that runs a program, opening the file in an
/// editor, or renaming a section (which can turn a harmless key into one
/// git obeys). Reading and unsetting stay free.
fn config(rest: &[String]) -> Verdict {
    let (flags, pos) = config_args(rest);
    let verb = pos.first().copied().unwrap_or("");
    if flags
        .iter()
        .any(|arg| *arg == "-e" || abbrev(arg, "--edit", 4))
        || verb == "edit"
    {
        return Some((
            "config --edit rewrites the configuration through an editor the hook cannot follow"
                .to_string(),
            FIX_PROGRAM,
        ));
    }
    if flags.iter().any(|arg| abbrev(arg, "--rename-section", 5)) || verb == "rename-section" {
        return Some((
            "renaming a configuration section can turn a stored value into a key git runs"
                .to_string(),
            FIX_PROGRAM,
        ));
    }
    let reads = [
        "get",
        "get-all",
        "get-regexp",
        "get-urlmatch",
        "list",
        "unset",
        "unset-all",
        "remove-section",
    ];
    if reads.contains(&verb)
        || flags.iter().any(|arg| {
            arg.starts_with("--get")
                || arg.starts_with("--unset")
                || *arg == "--list"
                || *arg == "-l"
        })
    {
        return None;
    }
    let mut words = pos.into_iter().skip_while(|word| !runs_program(word));
    let key = words.next()?;
    let value = words.next()?;
    (!BOOLEANS.contains(&value.to_ascii_lowercase().as_str())).then(|| {
        (
            format!("config stores {key}, a key whose value git runs as a program"),
            FIX_PROGRAM,
        )
    })
}

/// `git config` options whose value is the next word, with the shortest
/// abbreviation git takes for each.
const CONFIG_VALUED: &[(&str, usize)] = &[
    ("--comment", 3),
    ("--type", 3),
    ("--default", 3),
    ("--value", 3),
    ("--file", 5),
    ("--blob", 4),
];

/// The options and the positional words of `git config`. The value of an
/// option that takes one is neither (`--comment --list core.editor x`
/// stores core.editor with the comment `--list`).
pub(crate) fn config_args(rest: &[String]) -> (Vec<&str>, Vec<&str>) {
    let (mut flags, mut pos) = (Vec::new(), Vec::new());
    let mut args = rest.iter().map(String::as_str);
    while let Some(arg) = args.next() {
        if arg == "--" {
            pos.extend(args.by_ref());
        } else if !arg.starts_with('-') || arg == "-" {
            pos.push(arg);
        } else {
            flags.push(arg);
            let valued = if arg.starts_with("--") {
                !arg.contains('=')
                    && CONFIG_VALUED
                        .iter()
                        .any(|(full, min)| abbrev(arg, full, *min))
            } else {
                // `-f <file>`, also last in a cluster; `-ffile` is glued.
                arg.ends_with('f')
            };
            if valued {
                args.next();
            }
        }
    }
    (flags, pos)
}

fn moves(what: &str, branch: &str) -> (String, &'static str) {
    (
        format!("{what} moves the protected branch {branch}, which the gate compares against"),
        FIX_PROTECTED,
    )
}

/// `update-ref [-m <reason>] <ref> <new>`: the ref is the first word that is
/// not an option or the reason. Deleting is `git_guard`'s.
fn update_ref(rest: &[String]) -> Verdict {
    if options(rest).any(|arg| arg == "--stdin") {
        return Some((
            "update-ref --stdin takes its refs from input the hook cannot read".to_string(),
            FIX_PROTECTED,
        ));
    }
    let mut after_reason = false;
    let target = rest.iter().map(String::as_str).find(|arg| {
        let reason = std::mem::replace(&mut after_reason, *arg == "-m");
        !reason && !arg.starts_with('-')
    })?;
    protected(target).then(|| moves("update-ref", target))
}

/// `branch -f <branch> [<start>]` resets the branch; `-m`/`-M [<old>] <new>`
/// renames one away or onto another; `-c`/`-C [<old>] <new>` writes the new
/// one. The start point and the copied branch are only read.
fn branch(rest: &[String]) -> Verdict {
    let (mut force, mut rename, mut copy) = (false, false, false);
    for arg in options(rest).filter(|arg| arg.starts_with('-')) {
        if arg.starts_with("--") {
            force |= abbrev(arg, "--force", 6);
            rename |= abbrev(arg, "--move", 4);
            copy |= abbrev(arg, "--copy", 5);
        } else {
            force |= arg.contains('f');
            rename |= arg.contains(['m', 'M']);
            copy |= arg.contains(['c', 'C']);
        }
    }
    let names: Vec<&str> = positional(rest).collect();
    let written = if rename {
        names.as_slice()
    } else if copy {
        names.get(names.len().saturating_sub(1)..).unwrap_or(&[])
    } else if force {
        names.get(..1).unwrap_or(&[])
    } else {
        &[]
    };
    let target = written.iter().find(|name| protected(name))?;
    Some(moves("branch -f/-m/-c", target))
}

/// `checkout -B <branch>`, `switch -C <branch>`, `worktree add -B <branch>`:
/// the branch is reset to the start point, whatever it held.
fn force_create(rest: &[String]) -> Verdict {
    let mut args = options(rest);
    while let Some(arg) = args.next() {
        let glued = ["-B", "-C", "--force-create="]
            .iter()
            .find_map(|flag| arg.strip_prefix(flag))
            .filter(|name| !name.is_empty());
        let named = match glued {
            Some(name) => Some(name),
            // Also last in a cluster (`-qB main`).
            None if (arg.starts_with('-')
                && !arg.starts_with("--")
                && arg.ends_with(['B', 'C']))
                || abbrev(arg, "--force-create", 9) =>
            {
                args.next()
            }
            None => None,
        };
        if let Some(name) = named.filter(|name| protected(name)) {
            return Some(moves("-B/-C", name));
        }
    }
    None
}

/// `symbolic-ref <ref> <target>` and `-d <ref>`; reading one is fine.
fn symbolic_ref(rest: &[String]) -> Verdict {
    let mut names = positional(rest);
    let target = names.next().filter(|name| protected(name))?;
    let writes = names.next().is_some() || options(rest).any(|arg| abbrev(arg, "--delete", 3));
    (writes || rest.iter().any(|arg| arg == "-d")).then(|| moves("symbolic-ref", target))
}

/// A refspec `<src>:<dst>` whose destination is a protected local branch.
/// `fetch origin main:main` only follows the remote and stays free; another
/// source (`fetch origin feature:main`), or a "remote" that is a directory
/// (`fetch . HEAD:main`, `push . HEAD:main`), moves the branch to commits of
/// the agent's choosing. A push to a real remote is the server's to refuse.
fn refspecs(sub: &str, rest: &[String]) -> Verdict {
    // `--repo` names the remote out of sight of the positional words.
    let directory = options(rest).any(|arg| abbrev(arg, "--repo", 5))
        || positional(rest)
            .any(|word| word.starts_with(['.', '/', '~']) || word.starts_with("file:"));
    let branch = |name: &str| {
        let name = name.trim_start_matches('+');
        name.strip_prefix("refs/heads/").unwrap_or(name).to_string()
    };
    positional(rest).find_map(|spec| {
        let (src, dst) = spec.rsplit_once(':')?;
        let (src, dst) = (branch(src), branch(dst));
        (protected(&dst) && (directory || (sub != "push" && src != dst)))
            .then(|| moves(&format!("{sub} {spec}"), &dst))
    })
}
