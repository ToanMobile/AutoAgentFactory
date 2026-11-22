//! Approving a head, unblocking a task and unfreezing the factory are the
//! human's half of the factory. An agent that ran them would be approving
//! its own work, so the `aaf` subcommands behind them are refused.

use crate::shell::{Cmd, basename};
use crate::{Denial, paths};

const RULE: &str = "human_action_guard";
const FIX: &str =
    "stop and ask the user to run it: approve, board unblock and board unfreeze are human actions";

/// Words that name a human action wherever they stand, for text that could
/// not be split into commands.
pub const ACTION_WORDS: &[&str] = &["approve", "unblock", "unfreeze"];

/// aaf sets this for everything a factory run starts, and refuses `approve`,
/// `run`, `board unblock` and `board unfreeze` while it is set. An agent has
/// no reason to change it: unsetting, overwriting or setting it is refused,
/// and so is aaf under a wrapper that clears the environment. Reading it
/// (`echo $AAF_RUN_ID`) stays free.
const RUN_VAR: &str = "AAF_RUN_ID";
const FIX_RUN_VAR: &str =
    "leave AAF_RUN_ID as aaf set it and run aaf without env -i, exec -c or sudo";

/// Programs that run a command on a terminal of their own, or somewhere
/// else, and take it as words or as one string the splitter does not open
/// (`expect -c 'spawn aaf approve'`, `socat - EXEC:'aaf approve',pty`).
/// `aaf approve` asks for a terminal precisely so that a person is there.
const LAUNCHERS: &[&str] = &[
    "script",
    "expect",
    "spawn",
    "unbuffer",
    "socat",
    "zpty",
    "tmux",
    "screen",
    "ssh",
    "osascript",
];

/// aaf is named next to a human action word, in text that is not an argv.
pub fn names_action(tokens: &[&str]) -> bool {
    tokens.iter().any(|token| basename(token) == "aaf")
        && tokens.iter().any(|token| ACTION_WORDS.contains(token))
}

pub fn check(cmd: &Cmd) -> Option<Denial> {
    if let Some(reason) = hides_the_run(cmd) {
        return Some(Denial::new(RULE, reason, FIX_RUN_VAR));
    }
    let what = aaf_action(cmd).or_else(|| launched_action(cmd))?;
    Some(Denial::new(
        RULE,
        format!(
            "`aaf {what}` is a human action; an agent must not approve or unblock its own work"
        ),
        FIX,
    ))
}

fn hides_the_run(cmd: &Cmd) -> Option<&'static str> {
    let named = |word: &String| word.contains(RUN_VAR);
    if cmd.sets_var(RUN_VAR)
        // `env -u AAF_RUN_ID …`, `unset AAF_RUN_ID`.
        || cmd.wrapped.iter().any(named)
        || (cmd.name() == "unset" && cmd.args().iter().any(named))
    {
        return Some(
            "AAF_RUN_ID marks a factory run; changing it lifts the refusals aaf ties to it",
        );
    }
    (cmd.env_cleared && cmd.name() == "aaf")
        .then_some("aaf would run without the AAF_RUN_ID of the factory run it is part of")
}

/// The human action a launcher is asked to run, found in its words as text.
fn launched_action(cmd: &Cmd) -> Option<&'static str> {
    let argv = || cmd.wrapped.iter().chain(&cmd.words);
    if !argv().any(|word| LAUNCHERS.contains(&basename(word))) {
        return None;
    }
    let text = argv().map(String::as_str).collect::<Vec<_>>().join(" ");
    let tokens: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || ";|&()<>\"'`{}\\:,=".contains(c))
        .collect();
    names_action(&tokens).then_some("<a human action> on a terminal of its own")
}

fn aaf_action(cmd: &Cmd) -> Option<&'static str> {
    // A command word built at run time could be aaf; one typed literally is
    // aaf by its name, whatever directory it is in.
    let (args, certain) = if cmd.dynamic_name || cmd.name() == "aaf" {
        (cmd.args(), !cmd.dynamic_name)
    } else if cmd.name() == "cargo" {
        cargo_run_args(cmd.args())?
    } else {
        return None;
    };
    action(args, certain)
}

/// The human action `args` ask aaf for. With `certain` the program is known
/// to be aaf, and a subcommand that is not literal is refused as well.
fn action(args: &[String], certain: bool) -> Option<&'static str> {
    // `--json` is aaf's only global option and takes no value, so the first
    // word that is not an option is the subcommand.
    let mut positional = args
        .iter()
        .map(String::as_str)
        .filter(|arg| !arg.starts_with('-'));
    let unknown =
        |word: &str| certain && (word.is_empty() || word.contains('$') || paths::has_glob(word));
    match positional.next()? {
        "approve" => Some("approve"),
        "board" => match positional.next()? {
            "unblock" => Some("board unblock"),
            "unfreeze" => Some("board unfreeze"),
            sub if unknown(sub) => Some("board <action chosen at run time>"),
            _ => None,
        },
        sub if unknown(sub) => Some("<subcommand chosen at run time>"),
        _ => None,
    }
}

/// `cargo run [-p aaf-cli] -- <aaf args>`: the same CLI, built on the spot.
/// Returns the arguments the program gets and whether it is known to be aaf.
/// From inside the CLI's crate no option names it, so the run counts as aaf
/// unless `-p`/`--package`/`--bin` clearly pick something else.
fn cargo_run_args(args: &[String]) -> Option<(&[String], bool)> {
    let run = args.iter().position(|arg| arg == "run" || arg == "r")?;
    let rest = args.get(run + 1..)?;
    let split = match rest.iter().position(|arg| arg == "--") {
        Some(dashes) => (dashes, dashes + 1),
        // cargo hands the words after its options to the program without
        // `--` as well; option values make the first of them uncertain, so
        // the search starts at the word that would name a human action.
        None => {
            let at = rest
                .iter()
                .position(|arg| arg == "approve" || arg == "board")?;
            (at, at)
        }
    };
    let (options, passed) = (rest.get(..split.0)?, rest.get(split.1..)?);
    let mut selected = Vec::new();
    let mut at = 0;
    while let Some(option) = options.get(at).map(String::as_str) {
        at += 1;
        if matches!(option, "-p" | "--package" | "--bin") {
            selected.push(options.get(at).map_or("", String::as_str));
            at += 1;
        } else if let Some(glued) = ["--package=", "--bin=", "-p"]
            .iter()
            .find_map(|prefix| option.strip_prefix(prefix))
        {
            selected.push(glued);
        }
    }
    let mut before = args.iter().take(run + 1 + split.0);
    let names_aaf = before.any(|arg| arg.contains("aaf"));
    let unclear = |name: &&str| name.is_empty() || name.contains(['$', '`', '*', '?']);
    let other = !selected.is_empty() && !names_aaf && !selected.iter().any(unclear);
    // Without aaf on the line the program may be another project's: only the
    // literal actions are refused, not every argument built at run time.
    (!other).then_some((passed, names_aaf))
}
