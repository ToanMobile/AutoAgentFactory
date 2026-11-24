//! Command-family classifier for sharing learned peak-RSS statistics across
//! custom `--label` invocations of non-daemon build/test tools (e.g.
//! `aaf slot --label fix-g6 -- cargo test ...` reusing `@cmd:cargo` instead of
//! falling back to `Admission::Unmeasured` and triggering a machine-wide drain).
//!
//! Kept separate from `heavy::HEAVY_PROGRAMS` so daemon-exclusive policy and
//! `aaf-hook` rules remain unchanged.

use std::ffi::OsStr;

use super::heavy::{Scanner, Step, Token, shell_script, tokens};

const STATS_FAMILIES: &[(&str, &str)] = &[
    ("cargo", "cargo"),
    ("rustc", "cargo"),
    ("gradle", "gradle"),
    ("gradlew", "gradle"),
    ("gradlew.bat", "gradle"),
    ("xcodebuild", "xcodebuild"),
    ("unity", "unity"),
    ("unity.exe", "unity"),
    ("unity-editor", "unity"),
    ("mvn", "maven"),
    ("mvnw", "maven"),
    ("flutter", "flutter"),
    ("dart", "flutter"),
    ("npm", "node"),
    ("npx", "node"),
    ("pnpm", "node"),
    ("yarn", "node"),
    ("bun", "node"),
    ("pytest", "python"),
    ("uv", "python"),
    ("go", "go"),
    ("swift", "swift"),
    ("bazel", "bazel"),
    ("bazelisk", "bazel"),
    ("make", "make"),
    ("cmake", "make"),
    ("ninja", "make"),
    ("dotnet", "dotnet"),
    ("sbt", "sbt"),
    ("zig", "zig"),
];

pub(super) fn lookup_family(word: &str) -> Option<&'static str> {
    STATS_FAMILIES
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(word))
        .map(|(_, family)| *family)
}

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
