//! Shared heavy-build tool tables and mobile-subcommand scanner used by both
//! `aaf-engine` (machine-queue scheduler) and `aaf-hook` (PreToolUse guard)
//! so the two cannot drift.
//!
//! Invariant: `aaf-engine` MUST classify a superset of every command that
//! `aaf-hook` rejects under `heavy_cmd_guard`.

/// Gradle executables (case-insensitive basename match).
pub const GRADLE_PROGRAMS: &[&str] = &["gradle", "gradlew", "gradlew.bat"];

/// Xcodebuild executables (case-insensitive basename match).
pub const XCODEBUILD_PROGRAMS: &[&str] = &["xcodebuild"];

/// Unity editor executables (case-insensitive basename match).
pub const UNITY_PROGRAMS: &[&str] = &["unity", "unity.exe", "unity-editor"];

/// Unity flags that launch headless/batch heavy builds or tests (case-insensitive).
pub const UNITY_HEAVY_FLAGS: &[&str] = &["-batchmode", "-runTests", "-executeMethod"];

/// Returns `true` when `program` (basename) is a mobile build CLI whose
/// heavy subcommands must run through `aaf slot`.
pub fn is_mobile_candidate(program: &str) -> bool {
    program.eq_ignore_ascii_case("flutter")
        || program.eq_ignore_ascii_case("expo")
        || program.eq_ignore_ascii_case("react-native")
        || program.eq_ignore_ascii_case("pod")
}

/// Returns `true` when `subcommand` is a heavy build subcommand of `program`:
/// - `flutter build`
/// - `expo run:*` (e.g. `run:ios`, `run:android`)
/// - `react-native run-*` (e.g. `run-ios`, `run-android`)
/// - `pod install`
pub fn is_mobile_heavy_subcommand(program: &str, subcommand: &str) -> bool {
    if program.eq_ignore_ascii_case("flutter") {
        subcommand.eq_ignore_ascii_case("build")
    } else if program.eq_ignore_ascii_case("expo") {
        subcommand
            .get(..4)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("run:"))
            && subcommand.len() > 4
    } else if program.eq_ignore_ascii_case("react-native") {
        subcommand
            .get(..4)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("run-"))
            && subcommand.len() > 4
    } else if program.eq_ignore_ascii_case("pod") {
        subcommand.eq_ignore_ascii_case("install")
    } else {
        false
    }
}

fn strip_basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

const NODE_VALUED_OPTS: &[&str] = &[
    "-p",
    "--package",
    "--prefix",
    "-w",
    "--workspace",
    "-C",
    "--dir",
    "--cwd",
    "--filter",
    "-F",
    "--include",
    "--exclude",
    "--from",
    "-j",
    "--jobs",
];

const MISE_VALUED_OPTS: &[&str] = &[
    "-C",
    "--cd",
    "-j",
    "--jobs",
    "-E",
    "--env",
    "-P",
    "--profile",
    "--log-level",
    "--raw",
];

#[derive(Debug, Clone, Copy, Default)]
enum Stage {
    #[default]
    Idle,
    /// `npx` / `bunx`: skip options until delegated binary.
    DirectNode { skip_next: bool },
    /// `npm` (`exec`/`x`), `bun` (`x`), `pnpm`/`yarn` (`exec`/`dlx`/`run`).
    SubcmdNode {
        subcmds: &'static [&'static str],
        allow_direct: bool,
        skip_next: bool,
    },
    /// `bundle`/`bundler`/`rbenv`/`mise`/`direnv`/`asdf` waiting for `exec`/`x`.
    ExecLauncher {
        subcmds: &'static [&'static str],
        valued: &'static [&'static str],
        skip_dir_after_exec: bool,
        skip_next: bool,
    },
    /// Saw `direnv exec`; skip the `<dir>` positional argument before the binary.
    DirenvDir,
    /// Saw `exec`/`x` of a launcher; next non-flag is the delegated binary.
    AfterExec,
    /// Saw a mobile CLI (`flutter`, `expo`, `react-native`, `pod`); waiting for
    /// its first non-flag subcommand.
    Candidate(&'static str),
    /// Finished deciding this command; ignore remaining arguments.
    Done,
}

/// Incremental scanner for heavy mobile subcommands, shared by `aaf-engine`
/// and `aaf-hook` so both agree on every wrapper and option form.
#[derive(Debug, Clone, Copy, Default)]
pub struct MobileScan {
    stage: Stage,
}

impl MobileScan {
    pub fn reset(&mut self) {
        self.stage = Stage::Idle;
    }

    /// True once a mobile CLI head (`flutter`, `expo`, `react-native`, `pod`)
    /// has been recognized and we are still looking for its subcommand.
    pub fn awaiting_subcommand(&self) -> bool {
        matches!(self.stage, Stage::Candidate(_))
    }

    /// Called when the command's main executable (`base`) is recognized.
    pub fn on_command(&mut self, base: &str) {
        self.stage = Self::classify_head(strip_basename(base));
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
            Stage::DirectNode { skip_next: false }
        } else if base.eq_ignore_ascii_case("npm") {
            Stage::SubcmdNode {
                subcmds: &["exec", "x"],
                allow_direct: false,
                skip_next: false,
            }
        } else if base.eq_ignore_ascii_case("bun") {
            Stage::SubcmdNode {
                subcmds: &["x"],
                allow_direct: false,
                skip_next: false,
            }
        } else if base.eq_ignore_ascii_case("pnpm") || base.eq_ignore_ascii_case("yarn") {
            Stage::SubcmdNode {
                subcmds: &["exec", "dlx", "run"],
                allow_direct: true,
                skip_next: false,
            }
        } else if base.eq_ignore_ascii_case("bundle")
            || base.eq_ignore_ascii_case("bundler")
            || base.eq_ignore_ascii_case("rbenv")
        {
            Stage::ExecLauncher {
                subcmds: &["exec", "x"],
                valued: &[],
                skip_dir_after_exec: false,
                skip_next: false,
            }
        } else if base.eq_ignore_ascii_case("mise") {
            Stage::ExecLauncher {
                subcmds: &["exec", "x"],
                valued: MISE_VALUED_OPTS,
                skip_dir_after_exec: false,
                skip_next: false,
            }
        } else if base.eq_ignore_ascii_case("direnv") {
            Stage::ExecLauncher {
                subcmds: &["exec"],
                valued: &[],
                skip_dir_after_exec: true,
                skip_next: false,
            }
        } else if base.eq_ignore_ascii_case("asdf") {
            Stage::ExecLauncher {
                subcmds: &["exec"],
                valued: &[],
                skip_dir_after_exec: false,
                skip_next: false,
            }
        } else {
            Stage::Done
        }
    }

    fn enter_candidate(&mut self, word: &str) {
        let head = strip_basename(word);
        self.stage = if is_mobile_candidate(head) {
            Self::classify_head(head)
        } else {
            Stage::Done
        };
    }

    /// Feeds one argument word of the active command. Returns `true` when the
    /// command is confirmed to be a heavy mobile build.
    pub fn on_arg(&mut self, word: &str) -> bool {
        match self.stage {
            Stage::Idle | Stage::Done => false,
            Stage::DirectNode { skip_next } => {
                if skip_next {
                    self.stage = Stage::DirectNode { skip_next: false };
                    return false;
                }
                if word == "--" {
                    return false;
                }
                if NODE_VALUED_OPTS.contains(&word) {
                    self.stage = Stage::DirectNode { skip_next: true };
                    return false;
                }
                if word.starts_with('-') {
                    return false;
                }
                self.enter_candidate(word);
                false
            }
            Stage::SubcmdNode {
                subcmds,
                allow_direct,
                skip_next,
            } => {
                if skip_next {
                    self.stage = Stage::SubcmdNode {
                        subcmds,
                        allow_direct,
                        skip_next: false,
                    };
                    return false;
                }
                if word == "--" {
                    return false;
                }
                if NODE_VALUED_OPTS.contains(&word) {
                    self.stage = Stage::SubcmdNode {
                        subcmds,
                        allow_direct,
                        skip_next: true,
                    };
                    return false;
                }
                if word.starts_with('-') {
                    return false;
                }
                if subcmds.iter().any(|s| s.eq_ignore_ascii_case(word)) {
                    self.stage = Stage::AfterExec;
                    return false;
                }
                if allow_direct {
                    self.enter_candidate(word);
                } else {
                    self.stage = Stage::Done;
                }
                false
            }
            Stage::ExecLauncher {
                subcmds,
                valued,
                skip_dir_after_exec,
                skip_next,
            } => {
                if skip_next {
                    self.stage = Stage::ExecLauncher {
                        subcmds,
                        valued,
                        skip_dir_after_exec,
                        skip_next: false,
                    };
                    return false;
                }
                if valued.contains(&word) {
                    self.stage = Stage::ExecLauncher {
                        subcmds,
                        valued,
                        skip_dir_after_exec,
                        skip_next: true,
                    };
                    return false;
                }
                if word.starts_with('-') {
                    return false;
                }
                self.stage = if subcmds.iter().any(|s| s.eq_ignore_ascii_case(word)) {
                    if skip_dir_after_exec {
                        Stage::DirenvDir
                    } else {
                        Stage::AfterExec
                    }
                } else {
                    Stage::Done
                };
                false
            }
            Stage::DirenvDir => {
                if !word.starts_with('-') {
                    self.stage = Stage::AfterExec;
                }
                false
            }
            Stage::AfterExec => {
                if word == "--" || word.starts_with('-') {
                    return false;
                }
                self.enter_candidate(word);
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

/// Evaluates whether `(program, args)` invokes a heavy mobile build command,
/// using the exact same [`MobileScan`] state machine as `aaf-engine`.
pub fn is_mobile_heavy_command(program: &str, args: &[&str]) -> bool {
    let mut scan = MobileScan::default();
    scan.on_command(program);
    args.iter().any(|arg| scan.on_arg(arg))
}
