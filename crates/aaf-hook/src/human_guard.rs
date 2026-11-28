//! Approving a head, unblocking a task and unfreezing the factory are the
//! human's half of the factory.
//!
//! Two tiers:
//! - **Tier-A (always refused, even interactively)**: approvals, unblocks,
//!   unfreezes, plan approvals, undo/steer, SOP approvals/send-back,
//!   non-`--dry-run` `aaf merge`, `update`/`upgrade`, `telegram`/`ui`/`cockpit`,
//!   `AAF_RUN_ID` tampering, dynamic subcommands, and privilege-escalating flags
//!   on Tier-B commands.
//! - **Tier-B (refused inside a factory run `in_factory_run == true`, allowed
//!   interactively when free of Tier-A flags)**: `aaf run`, `aaf init`,
//!   `aaf plan`, `aaf gc`, `aaf skills sync`, `aaf sop run|init`, and
//!   `aaf board add|ready|split|dep|import-issues|sync-github|rework`.

use crate::shell::{Cmd, fold_name};
use crate::{Ctx, Denial, paths};

mod cargo;
use cargo::cargo_run_args;

const RULE: &str = "human_action_guard";
const FIX_WORKER: &str = "skip this human-only command and continue your task (the factory \
                          orchestrator handles approvals, board state, and run lifecycle outside \
                          the worker); this needs a human: finish the task without it and name \
                          the step in your final message";
const FIX_INTERACTIVE: &str = "this command or privilege-escalating flag requires the human \
                               operator directly; run the safe non-escalated subcommand or report \
                               what approval is needed";

/// Words that name a Tier-A human action wherever they stand alongside `aaf`,
/// for text that could not be split into commands.
pub const ACTION_WORDS: &[&str] = &[
    "approve",
    "unblock",
    "unfreeze",
    "approve-plan",
    "undo",
    "steer",
    "send-back",
];

/// aaf sets this for everything a factory run starts, and refuses human actions
/// while it is set. An agent has no reason to change it: unsetting, overwriting
/// or setting it is refused, and so is aaf under a wrapper that clears the
/// environment. Reading it (`echo $AAF_RUN_ID`) stays free.
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

fn is_dynamic_word(word: &str) -> bool {
    word.is_empty() || word.contains('$') || paths::has_glob(word)
}

fn token_matches_flag(tok: &str, flag: &str) -> bool {
    tok == flag
        || tok
            .strip_prefix(flag)
            .is_some_and(|rest| rest.starts_with('='))
}

fn tokens_have_flag(tokens: &[&str], flags: &[&str]) -> bool {
    tokens
        .iter()
        .any(|tok| flags.iter().any(|flag| token_matches_flag(tok, flag)))
}

fn is_escalated_land_value(val: &str) -> bool {
    val.is_empty()
        || val.eq_ignore_ascii_case("auto")
        || val.eq_ignore_ascii_case("pr-merge")
        || val.contains('$')
        || paths::has_glob(val)
}

fn tokens_have_escalated_land(tokens: &[&str]) -> bool {
    for (i, &tok) in tokens.iter().enumerate() {
        if tok == "--land" {
            let next = tokens.get(i + 1).copied();
            if next.is_none_or(is_escalated_land_value) {
                return true;
            }
        } else if let Some(val) = tok.strip_prefix("--land=")
            && is_escalated_land_value(val)
        {
            return true;
        }
    }
    false
}

/// Text-level check (for unparsable input and terminal launchers) respecting
/// the Tier-A / Tier-B split based on `in_factory_run`.
pub fn names_action_in(tokens: &[&str], in_factory_run: bool) -> bool {
    let non_flags: Vec<&str> = tokens
        .iter()
        .copied()
        .filter(|t| !t.is_empty() && !t.starts_with('-'))
        .collect();
    if !non_flags.iter().any(|token| fold_name(token) == "aaf") {
        return false;
    }
    // Tier-A action words: always refused alongside `aaf`.
    if non_flags.iter().any(|token| ACTION_WORDS.contains(token)) {
        return true;
    }
    // Tier-A subcommand pairs and dynamic subcommands: always refused.
    let has_tier_a_pair = non_flags.windows(2).any(|pair| {
        let (first, second) = (pair[0], pair[1]);
        (fold_name(first) == "aaf"
            && (matches!(
                second,
                "approve" | "cockpit" | "telegram" | "ui" | "update" | "upgrade" | "merge"
            ) || is_dynamic_word(second)))
            || (first == "sop"
                && (matches!(second, "approve" | "send-back") || is_dynamic_word(second)))
            || (first == "proof" && !matches!(second, "verify" | "show" | "list"))
            || (first == "skills" && is_dynamic_word(second))
            || (first == "board"
                && (matches!(
                    second,
                    "unblock" | "unfreeze" | "approve-plan" | "undo" | "steer"
                ) || is_dynamic_word(second)))
    });
    if has_tier_a_pair {
        return true;
    }
    // Truncated/unparsable `aaf proof 'oops` (without a safe subcommand like `verify`/`show`).
    if non_flags.last() == Some(&"proof") {
        return true;
    }
    // Tier-A flag escalations on Tier-B commands: always refused.
    let has_pair = |a: &str, b: &str| {
        non_flags.windows(2).any(|pair| {
            let matches_first = if a == "aaf" {
                fold_name(pair[0]) == a
            } else {
                pair[0] == a
            };
            matches_first && pair[1] == b
        })
    };
    let tier_a_flag = (has_pair("aaf", "run")
        && (tokens_have_flag(
            tokens,
            &[
                "--unsandboxed",
                "--dangerously-bypass-permissions",
                "--auto-merge",
                "--auto",
            ],
        ) || tokens_have_escalated_land(tokens)))
        || (has_pair("aaf", "init")
            && tokens_have_flag(tokens, &["--force", "--commit", "--auto"]))
        || (has_pair("aaf", "gc") && tokens_have_flag(tokens, &["--force", "--all"]))
        || (has_pair("sop", "run") && tokens_have_flag(tokens, &["--auto"]))
        || (has_pair("sop", "init") && tokens_have_flag(tokens, &["--force"]))
        || (has_pair("board", "add") && tokens_have_flag(tokens, &["--repro-cmd", "--repro-arg"]))
        || (has_pair("skills", "sync")
            && tokens_have_flag(tokens, &["--force", "--dir", "--global"]));
    if tier_a_flag {
        return true;
    }
    if !in_factory_run {
        return false;
    }
    // Tier-B subcommand pairs: refused only inside factory worker runs.
    non_flags.windows(2).any(|pair| {
        let (first, second) = (pair[0], pair[1]);
        (fold_name(first) == "aaf" && matches!(second, "run" | "init" | "plan" | "gc"))
            || (first == "sop" && matches!(second, "run" | "init"))
            || (first == "skills" && second == "sync")
            || (first == "board"
                && matches!(
                    second,
                    "add" | "ready" | "rework" | "import-issues" | "sync-github" | "split" | "dep"
                ))
    })
}

/// Fail-closed wrapper for callers without a session mode (assumes factory run).
pub fn names_action(tokens: &[&str]) -> bool {
    names_action_in(tokens, true)
}

pub fn check(cmd: &Cmd, ctx: &Ctx) -> Option<Denial> {
    if let Some(reason) = hides_the_run(cmd) {
        return Some(Denial::new(RULE, reason, FIX_RUN_VAR));
    }
    let in_factory_run = ctx.env.in_factory_run;
    let what = aaf_action(cmd, ctx).or_else(|| launched_action(cmd, in_factory_run))?;
    let fix = if in_factory_run {
        FIX_WORKER
    } else {
        FIX_INTERACTIVE
    };
    Some(Denial::new(
        RULE,
        format!(
            "`aaf {what}` is a human action; an agent must not approve or unblock its own work"
        ),
        fix,
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
fn launched_action(cmd: &Cmd, in_factory_run: bool) -> Option<&'static str> {
    let argv = || cmd.wrapped.iter().chain(&cmd.words);
    if !argv().any(|word| LAUNCHERS.contains(&fold_name(word).as_str())) {
        return None;
    }
    let text = argv().map(String::as_str).collect::<Vec<_>>().join(" ");
    let tokens: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || ";|&()<>\"'`{}\\:,=".contains(c))
        .filter(|t| !t.is_empty())
        .collect();
    names_action_in(&tokens, in_factory_run).then_some("<a human action> on a terminal of its own")
}

fn aaf_action(cmd: &Cmd, ctx: &Ctx) -> Option<&'static str> {
    // A command word built at run time could be aaf; one typed literally is
    // aaf by its name, whatever directory it is in.
    let (args, certain) = if cmd.dynamic_name || cmd.name() == "aaf" {
        (cmd.args(), !cmd.dynamic_name)
    } else if cmd.name() == "cargo" {
        cargo_run_args(cmd.args(), ctx)?
    } else {
        return None;
    };
    action(args, certain, ctx.env.in_factory_run)
}

fn has_flag(args: &[String], flag: &str) -> bool {
    for arg in args.iter().map(String::as_str) {
        if arg == "--" {
            break;
        }
        if token_matches_flag(arg, flag) {
            return true;
        }
    }
    false
}

fn has_any_flag(args: &[String], flags: &[&str]) -> bool {
    flags.iter().any(|flag| has_flag(args, flag))
}

fn has_escalated_land(args: &[String]) -> bool {
    let mut i = 0;
    while let Some(arg) = args.get(i).map(String::as_str) {
        if arg == "--" {
            break;
        }
        if arg == "--land" {
            let val = args.get(i + 1).map(String::as_str);
            if val.is_none_or(is_escalated_land_value) {
                return true;
            }
            i += 2;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--land=")
            && is_escalated_land_value(val)
        {
            return true;
        }
        i += 1;
    }
    false
}

fn has_dry_run_flag(args: &[String]) -> bool {
    let mut dry_run = false;
    for arg in args.iter().map(String::as_str) {
        if arg == "--" {
            break;
        }
        if arg == "--dry-run" {
            dry_run = true;
        } else if arg == "--no-dry-run" {
            dry_run = false;
        }
    }
    dry_run
}

/// Finds the first positional (non-flag) argument in `args`, returning it and
/// the trailing slice after it.
fn first_positional(args: &[String]) -> Option<(&str, &[String])> {
    let pos = args.iter().position(|arg| !arg.starts_with('-'))?;
    Some((args[pos].as_str(), args.get(pos + 1..).unwrap_or(&[])))
}

/// The human action `args` ask aaf for. With `certain` the program is known
/// to be aaf, and a subcommand that is not literal is refused as well.
fn action(args: &[String], certain: bool, in_factory_run: bool) -> Option<&'static str> {
    let (sub, rest) = first_positional(args)?;
    let unknown = |word: &str| certain && is_dynamic_word(word);
    match sub {
        // Tier-A: always refused (interactive and worker).
        "approve" => Some("approve"),
        "cockpit" => Some("cockpit"),
        "telegram" => Some("telegram"),
        "ui" => Some("ui"),
        "update" => Some("update"),
        "upgrade" => Some("upgrade"),
        "merge" => {
            if in_factory_run || !has_dry_run_flag(rest) {
                Some("merge")
            } else {
                None
            }
        }
        // Tier-B with Tier-A flag escalations.
        "run" => {
            if in_factory_run {
                Some("run")
            } else if has_flag(rest, "--unsandboxed") {
                Some("run --unsandboxed")
            } else if has_flag(rest, "--dangerously-bypass-permissions") {
                Some("run --dangerously-bypass-permissions")
            } else if has_flag(rest, "--auto-merge") {
                Some("run --auto-merge")
            } else if has_flag(rest, "--auto") {
                Some("run --auto")
            } else if has_escalated_land(rest) {
                Some("run --land")
            } else {
                None
            }
        }
        "init" => {
            if in_factory_run {
                Some("init")
            } else if has_flag(rest, "--force") {
                Some("init --force")
            } else if has_flag(rest, "--commit") {
                Some("init --commit")
            } else if has_flag(rest, "--auto") {
                Some("init --auto")
            } else {
                None
            }
        }
        "plan" => in_factory_run.then_some("plan"),
        "gc" => {
            if in_factory_run {
                Some("gc")
            } else if has_flag(rest, "--force") {
                Some("gc --force")
            } else if has_flag(rest, "--all") {
                Some("gc --all")
            } else {
                None
            }
        }
        "sop" => {
            let (sub2, rest2) = first_positional(rest)?;
            match sub2 {
                "approve" => Some("sop approve"),
                "send-back" => Some("sop send-back"),
                "run" => {
                    if in_factory_run {
                        Some("sop run")
                    } else if has_flag(rest2, "--auto") {
                        Some("sop run --auto")
                    } else {
                        None
                    }
                }
                "init" => {
                    if in_factory_run {
                        Some("sop init")
                    } else if has_flag(rest2, "--force") {
                        Some("sop init --force")
                    } else {
                        None
                    }
                }
                sub2 if unknown(sub2) => Some("sop <action chosen at run time>"),
                _ => None,
            }
        }
        "proof" => {
            let (sub2, _) = first_positional(rest)?;
            match sub2 {
                "record" => Some("proof record"),
                sub2 if unknown(sub2) => Some("proof <action chosen at run time>"),
                _ => None,
            }
        }
        "skills" => {
            let (sub2, rest2) = first_positional(rest)?;
            match sub2 {
                "sync" => {
                    if in_factory_run {
                        Some("skills sync")
                    } else if has_any_flag(rest2, &["--force", "--dir", "--global"]) {
                        Some("skills sync (escalated)")
                    } else {
                        None
                    }
                }
                sub2 if unknown(sub2) => Some("skills <action chosen at run time>"),
                _ => None,
            }
        }
        "board" => {
            let (sub2, rest2) = first_positional(rest)?;
            match sub2 {
                // Tier-A board actions: always refused.
                "unblock" => Some("board unblock"),
                "unfreeze" => Some("board unfreeze"),
                "approve-plan" => Some("board approve-plan"),
                "undo" => Some("board undo"),
                "steer" => Some("board steer"),
                // Tier-B board actions: refused inside factory runs; `add` also
                // blocks `--repro-cmd` / `--repro-arg` interactively.
                "add" => {
                    if in_factory_run {
                        Some("board add")
                    } else if has_any_flag(rest2, &["--repro-cmd", "--repro-arg"]) {
                        Some("board add --repro-cmd/--repro-arg")
                    } else {
                        None
                    }
                }
                "ready" => in_factory_run.then_some("board ready"),
                "rework" => in_factory_run.then_some("board rework"),
                "import-issues" => in_factory_run.then_some("board import-issues"),
                "sync-github" => in_factory_run.then_some("board sync-github"),
                "split" => in_factory_run.then_some("board split"),
                "dep" => in_factory_run.then_some("board dep"),
                sub2 if unknown(sub2) => Some("board <action chosen at run time>"),
                _ => None,
            }
        }
        sub if unknown(sub) => Some("<subcommand chosen at run time>"),
        _ => None,
    }
}
