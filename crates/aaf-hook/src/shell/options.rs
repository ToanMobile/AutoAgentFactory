//! Option parsers of the wrappers `shell::command` strips: where a value
//! or a carried command line sits in their words.

use super::{Redirect, Tok};

/// The file the external `time` writes its report to (`-o <file>`,
/// `-ao<file>`, `--output[=]<file>`), as a redirection.
pub(super) fn time_output(options: &[Tok]) -> Option<Redirect> {
    let mut target = None;
    for (at, tok) in options.iter().enumerate() {
        let text = tok.text.as_str();
        let next = || options.get(at + 1).map(|next| next.text.clone());
        if let Some(long) = text.strip_prefix("--output") {
            target = match long.strip_prefix('=') {
                Some(glued) => Some(glued.to_string()),
                None if long.is_empty() => next(),
                None => target,
            };
        } else if text.starts_with('-') && !text.starts_with("--") {
            // In a cluster the first letter that takes a value takes the rest.
            if let Some(o) = text
                .find(['o', 'f'])
                .filter(|&o| text.as_bytes()[o] == b'o')
            {
                target = match &text[o + 1..] {
                    "" => next(),
                    glued => Some(glued.to_string()),
                };
            }
        }
    }
    target.map(|target| Redirect {
        op: ">".to_string(),
        target,
    })
}

/// `aaf [--json] slot [options] -- <command>`: where the command starts, so
/// the guards judge it like one typed bare. Without the `--` no word is
/// known to be a command and `aaf` stays the command.
pub(super) fn slot_payload(words: &[Tok]) -> Option<usize> {
    let sub = 1 + words
        .iter()
        .skip(1)
        .position(|tok| !tok.text.starts_with('-'))?;
    let slot = words.get(sub).filter(|tok| !tok.dynamic)?;
    if slot.text != "slot" {
        return None;
    }
    let dashes = words
        .iter()
        .skip(sub + 1)
        .position(|tok| tok.text == "--")?;
    Some(sub + dashes + 2)
}

/// For an `env` option that carries a command line (`-S`, a cluster ending
/// in or holding `S`, `--split-string[=…]`): the part glued to it, empty
/// when the command line is the next word.
pub(super) fn split_string(text: &str) -> Option<&str> {
    if let Some(long) = text.strip_prefix("--split-string") {
        return match long.strip_prefix('=') {
            Some(glued) => Some(glued),
            None => long.is_empty().then_some(""),
        };
    }
    if !text.starts_with('-') || text.starts_with("--") {
        return None;
    }
    // `u`, `C` and `P` come first when they do: `-uS` unsets a variable named S,
    // `-P/dirS` sets the search path to `/dirS`.
    let at = text.find(['S', 'u', 'C', 'P'])?;
    text[at..].strip_prefix('S')
}

/// For BSD `env -P <dirs>`, `-P<dirs>`, or a cluster ending in `P` (`-iP <dirs>`):
/// returns the glued directory, or `""` when the directory is the next word.
pub(super) fn env_altpath(text: &str) -> Option<&str> {
    if !text.starts_with('-') || text.starts_with("--") {
        return None;
    }
    let at = text.find(['P', 'u', 'C', 'S'])?;
    text[at..].strip_prefix('P')
}

/// For util-linux `script -c <cmd>`, `-c<cmd>`, `-qc<cmd>`, or `--command[=<cmd>]`:
/// returns the glued command, or `""` when the command is the next word.
pub(super) fn script_command(text: &str) -> Option<&str> {
    if let Some(long) = text.strip_prefix("--command") {
        return match long.strip_prefix('=') {
            Some(glued) => Some(glued),
            None => long.is_empty().then_some(""),
        };
    }
    if !text.starts_with('-') || text.starts_with("--") {
        return None;
    }
    let at = text.find(['c', 't', 'T', 'I', 'O', 'B', 'm', 'E', 'o'])?;
    text[at..].strip_prefix('c')
}
