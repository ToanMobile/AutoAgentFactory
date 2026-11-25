//! Subcommand-level classification for heavy mobile build tools (`flutter build`,
//! `expo run:*`, `react-native run-*`, `pod install`), including invocations
//! launched via `npx`, `bunx`, `pnpm [exec]`, `yarn`, or `bundle exec`.

use aaf_domain::heavy::{is_mobile_candidate, is_mobile_heavy_subcommand};

use super::basename;

#[derive(Debug, Clone, Copy, Default)]
enum Stage {
    #[default]
    Idle,
    /// Saw `npx` or `bunx`; skipping its options until the delegated binary.
    Npx { skip_next: bool },
    /// Saw `pnpm` or `yarn`; skipping options and optional `exec`/`dlx`.
    NodePkg,
    /// Saw `bundle`/`bundler`; waiting for `exec`.
    Bundle,
    /// Saw `bundle exec`; next non-flag is the delegated binary.
    BundleExec,
    /// Saw a mobile CLI (`flutter`, `expo`, `react-native`, `pod`); waiting for
    /// its first non-flag subcommand.
    Candidate(&'static str),
    /// Finished deciding this command; ignore remaining arguments.
    Done,
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct MobileScan {
    stage: Stage,
}

impl MobileScan {
    pub(super) fn reset(&mut self) {
        self.stage = Stage::Idle;
    }

    /// Called when the command's main executable (`base`) is recognized.
    pub(super) fn on_command(&mut self, base: &str) {
        self.stage = Self::classify_head(base);
    }

    fn classify_head(base: &str) -> Stage {
        if base.eq_ignore_ascii_case("flutter") {
            Stage::Candidate("flutter")
        } else if base.eq_ignore_ascii_case("expo") {
            Stage::Candidate("expo")
        } else if base.eq_ignore_ascii_case("react-native") {
            Stage::Candidate("react-native")
        } else if base.eq_ignore_ascii_case("pod") {
            Stage::Candidate("pod")
        } else if base.eq_ignore_ascii_case("npx") || base.eq_ignore_ascii_case("bunx") {
            Stage::Npx { skip_next: false }
        } else if base.eq_ignore_ascii_case("pnpm") || base.eq_ignore_ascii_case("yarn") {
            Stage::NodePkg
        } else if base.eq_ignore_ascii_case("bundle") || base.eq_ignore_ascii_case("bundler") {
            Stage::Bundle
        } else {
            Stage::Done
        }
    }

    /// Feeds one argument word of the active command. Returns `true` when the
    /// command is confirmed to be a heavy mobile build.
    pub(super) fn on_arg(&mut self, word: &str) -> bool {
        match self.stage {
            Stage::Idle | Stage::Done => false,
            Stage::Npx { skip_next } => {
                if skip_next {
                    self.stage = Stage::Npx { skip_next: false };
                    return false;
                }
                if word == "--" {
                    return false;
                }
                if matches!(word, "-p" | "--package" | "-c" | "--call") {
                    self.stage = Stage::Npx { skip_next: true };
                    return false;
                }
                if word.starts_with('-') {
                    return false;
                }
                let head = basename(word);
                self.stage = if is_mobile_candidate(head) {
                    Self::classify_head(head)
                } else {
                    Stage::Done
                };
                false
            }
            Stage::NodePkg => {
                if word == "--" || word.starts_with('-') {
                    return false;
                }
                if word.eq_ignore_ascii_case("exec") || word.eq_ignore_ascii_case("dlx") {
                    return false;
                }
                let head = basename(word);
                self.stage = if is_mobile_candidate(head) {
                    Self::classify_head(head)
                } else {
                    Stage::Done
                };
                false
            }
            Stage::Bundle => {
                if word.starts_with('-') {
                    return false;
                }
                self.stage = if word.eq_ignore_ascii_case("exec") {
                    Stage::BundleExec
                } else {
                    Stage::Done
                };
                false
            }
            Stage::BundleExec => {
                if word.starts_with('-') {
                    return false;
                }
                let head = basename(word);
                self.stage = if is_mobile_candidate(head) {
                    Self::classify_head(head)
                } else {
                    Stage::Done
                };
                false
            }
            Stage::Candidate(program) => {
                if word.starts_with('-') {
                    return false;
                }
                self.stage = Stage::Done;
                is_mobile_heavy_subcommand(program, word)
            }
        }
    }
}
