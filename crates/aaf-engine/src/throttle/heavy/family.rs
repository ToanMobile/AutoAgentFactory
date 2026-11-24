//! Command-family classification for peak-RSS stats sharing across custom labels.

use std::ffi::OsStr;

use super::{Scanner, Step, Token, shell_script, tokens};

/// Build/test command family for sharing peak-RSS measurements across custom
/// `--label` invocations in the same repository.
pub fn stats_command_family(command: &str) -> Option<&'static str> {
    let mut scanner = Scanner::new(false);
    for token in tokens(command) {
        match token {
            Token::Break => scanner.start_command(),
            Token::Word(word) => match scanner.word(&word) {
                Step::Heavy(tool) => return Some(tool.name()),
                Step::Family(family) => return Some(family),
                Step::InlineScript(script) => {
                    if let Some(family) = stats_command_family(script) {
                        return Some(family);
                    }
                }
                Step::Script | Step::Next => {}
            },
        }
    }
    None
}

/// [`stats_command_family`] for an argument vector (`aaf slot --label x -- sh -c 'cargo test'`).
pub fn stats_command_family_argv<S: AsRef<OsStr>>(argv: &[S]) -> Option<&'static str> {
    let mut scanner = Scanner::new(false);
    let mut words = argv.iter().map(|arg| arg.as_ref().to_string_lossy());
    while let Some(word) = words.next() {
        match scanner.word(&word) {
            Step::Heavy(tool) => return Some(tool.name()),
            Step::Family(family) => return Some(family),
            Step::Script => {
                let script = if scanner.shell {
                    shell_script(&word, &mut words)
                } else {
                    words.next()
                };
                if let Some(family) = script.and_then(|script| stats_command_family(&script)) {
                    return Some(family);
                }
                scanner.arguments_only();
            }
            Step::InlineScript(script) => {
                if let Some(family) = stats_command_family(script) {
                    return Some(family);
                }
                scanner.arguments_only();
            }
            Step::Next => {}
        }
    }
    None
}
