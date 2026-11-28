//! A git alias is another spelling of a command: its value is judged as the
//! command it expands to, and refused when that cannot be worked out.

use std::path::PathBuf;

use crate::git_guard::GitCall;
use crate::shell::{self, Cmd};
use crate::{Ctx, Denial, cwd, fallback, judge};

/// Commands git runs itself whatever the configuration says: an alias can
/// never take one of these names, so they need no lookup. Only names every
/// supported git has built in; a builtin missing here costs one lookup, a
/// wrong entry would hide an alias.
const BUILTINS: &[&str] = &[
    "add",
    "am",
    "apply",
    "archive",
    "bisect",
    "blame",
    "branch",
    "bundle",
    "cat-file",
    "check-ignore",
    "checkout",
    "cherry-pick",
    "clean",
    "clone",
    "commit",
    "config",
    "describe",
    "diff",
    "diff-index",
    "diff-tree",
    "fetch",
    "for-each-ref",
    "format-patch",
    "fsck",
    "gc",
    "grep",
    "hash-object",
    "help",
    "init",
    "log",
    "ls-files",
    "ls-remote",
    "ls-tree",
    "merge",
    "merge-base",
    "mv",
    "notes",
    "pull",
    "push",
    "range-diff",
    "rebase",
    "reflog",
    "remote",
    "reset",
    "restore",
    "rev-list",
    "rev-parse",
    "revert",
    "rm",
    "shortlog",
    "show",
    "show-ref",
    "stash",
    "status",
    "submodule",
    "switch",
    "symbolic-ref",
    "tag",
    "update-index",
    "update-ref",
    "version",
    "worktree",
    "write-tree",
];

const RULE: &str = "git_guard";
pub const FIX: &str = "run the git command itself instead of defining an alias for it";

fn deny(name: &str, why: &str) -> Denial {
    Denial::new(RULE, format!("the git alias {name} {why}"), FIX)
}

/// Aliases defined with `-c alias.<name>=<value>`, `git config alias.…`, and
/// the alias the subcommand may be in the stored configuration.
pub fn check(call: &GitCall, cmd: &Cmd, ctx: &Ctx) -> Option<Denial> {
    if call.hidden_alias {
        return Some(deny(
            "from --config-env",
            "takes its value from the environment and cannot be analysed",
        ));
    }
    let shell_cwd = call.resolve_dir(ctx);
    for alias in &call.aliases {
        // The rest of the line belongs to the alias only when it is the one
        // invoked; a defined alias is judged either way.
        let used = call.sub.eq_ignore_ascii_case(alias.name);
        let extra = if used { call.rest } else { &[] };
        // Every other global option still applies to the expansion,
        // including the aliases it may refer to in turn.
        let globals: Vec<String> = call
            .globals
            .iter()
            .enumerate()
            .filter(|(at, _)| *at < alias.at || *at >= alias.at + alias.width)
            .map(|(_, option)| option.clone())
            .collect();
        if let Some(why) = expansion(alias.value, &globals, extra, ctx, shell_cwd.clone()) {
            return Some(deny(alias.name, &why));
        }
    }
    if call.sub == "config" {
        let moved = ctx.with_cwd(shell_cwd);
        return stored(call.rest, &moved);
    }
    configured(call, cmd, ctx)
}

/// The value of `alias.<name>` in a `git config --list -z` listing
/// (`key\nvalue\0` entries; the last one wins, as in git).
fn alias_value<'a>(listing: &'a str, name: &str) -> Option<&'a str> {
    let wanted = [format!("alias.{name}"), format!("alias.{name}.command")];
    listing
        .split('\0')
        .filter_map(|entry| {
            let (key, value) = entry.split_once('\n').unwrap_or((entry, ""));
            wanted
                .iter()
                .any(|wanted| key.eq_ignore_ascii_case(wanted))
                .then_some(value)
        })
        .next_back()
}

/// A subcommand git does not have built in may be an alias from the
/// configuration (`git fp` with `alias.fp = push --force`): it is looked up
/// and judged as its expansion. When it cannot be looked up it is refused,
/// because it could stand for anything.
fn configured(call: &GitCall, cmd: &Cmd, ctx: &Ctx) -> Option<Denial> {
    let name = call.sub;
    // A command word built at run time may not be git at all, and a
    // subcommand built at run time has no name to look up.
    if cmd.dynamic_name
        || cmd.name() != "git"
        || name.is_empty()
        || name.contains(['$', '`'])
        || BUILTINS.contains(&name)
        || call
            .aliases
            .iter()
            .any(|alias| alias.name.eq_ignore_ascii_case(name))
    {
        return None;
    }
    let unknown = |why: &str| {
        Denial::new(
            RULE,
            format!("`git {name}` may be an alias and {why}"),
            "run a plain git command from inside the repository",
        )
    };
    let elsewhere = call.globals.iter().any(|g| g.starts_with("--git-dir"))
        || ctx.line.git_dir_moved
        || cmd
            .assigns
            .iter()
            .any(|(key, _)| key == "GIT_DIR" || key == "GIT_COMMON_DIR");
    if elsewhere {
        return Some(unknown(
            "the command points git at a repository the hook does not look in",
        ));
    }
    let dir = call.resolve_dir(ctx);
    let Some(listing) = (ctx.env.git_config)(&dir) else {
        return Some(unknown("the git configuration could not be read"));
    };
    let value = alias_value(&listing, name)?;
    expansion(value, call.globals, call.rest, ctx, dir).map(|why| deny(name, &why))
}

/// `git config [set|--add|…] alias.<name> <value>`. Later commands could
/// use the alias, so a refused value is refused when it is stored.
fn stored(rest: &[String], ctx: &Ctx) -> Option<Denial> {
    // An option's value (`--file alias.x/…`) is not the key.
    let (_, positional) = crate::git_persist::config_args(rest);
    let mut positional = positional
        .into_iter()
        .skip_while(|arg| crate::git_guard::alias_name(arg).is_none());
    let name = positional.next()?;
    let value = positional.next()?;
    expansion(value, &[], &[], ctx, ctx.cwd.clone()).map(|why| deny(name, &why))
}

/// Why the command an alias value stands for is refused. `extra` are the
/// arguments the alias is invoked with; `shell_cwd` is the directory resolved
/// from any leading `git -C` flags for `!` shell aliases.
fn expansion(
    value: &str,
    globals: &[String],
    extra: &[String],
    ctx: &Ctx,
    shell_cwd: PathBuf,
) -> Option<String> {
    if ctx.depth >= shell::MAX_DEPTH {
        return Some("is nested too deeply to analyse".to_string());
    }
    let git_ctx = ctx.nested();
    let shell_ctx = git_ctx.with_cwd(shell_cwd);
    let refused = |denial: Denial| format!("expands to a refused command ({})", denial.reason);
    let (body, is_shell) = match value.strip_prefix('!') {
        Some(body) => (body, true),
        None => (value, false),
    };
    let mut script = shell::parse(body);
    if git_ctx.line.aaf_untrusted {
        // The line that rebinds `aaf` runs this alias in its environment.
        script.distrust_aaf();
    }
    let opaque_ctx = if is_shell { &shell_ctx } else { &git_ctx };
    if let Some(denial) = script
        .opaque
        .iter()
        .find_map(|text| fallback::check(text, opaque_ctx))
    {
        return Some(refused(denial));
    }
    if !script.opaque.is_empty() {
        return Some("has a value that cannot be analysed".to_string());
    }
    if is_shell {
        // git appends the arguments to the script. Where they end up inside
        // it (`f() { git push "$@"; }; f`) is unknown, so every command of
        // the script is also judged with them appended, while tracking
        // `cd`/`pushd`/`popd` and subshell scopes across the alias body.
        return cwd::eval_script(&script, shell_ctx, |cmd, cur| {
            let mut with_extra = cmd.clone();
            with_extra.words.extend(extra.iter().cloned());
            with_extra.refresh_name();
            judge(cmd, cur).or_else(|| judge(&with_extra, cur).filter(|_| !extra.is_empty()))
        })
        .map(refused);
    }
    let plain = match script.cmds.as_slice() {
        [cmd] if !cmd.dynamic_name && cmd.assigns.is_empty() && cmd.redirects.is_empty() => cmd,
        _ => return Some("is not one plain git command".to_string()),
    };
    let mut words = vec!["git".to_string()];
    words.extend(globals.iter().cloned());
    words.extend(plain.words.iter().cloned());
    words.extend(extra.iter().cloned());
    let cmd = Cmd::from_words(words);
    judge(&cmd, &git_ctx).map(refused)
}
