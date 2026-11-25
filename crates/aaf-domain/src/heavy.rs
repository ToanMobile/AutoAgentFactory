//! Shared heavy-build tool tables used by both `aaf-engine` (machine-queue
//! scheduler) and `aaf-hook` (PreToolUse guard) so the two cannot drift.
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

/// Evaluates whether `(program, args)` invokes a heavy mobile build command,
/// unwrapping common runners (`npx`, `bunx`, `yarn`, `pnpm [exec]`,
/// `bundle exec`).
pub fn is_mobile_heavy_command(program: &str, args: &[&str]) -> bool {
    let base = strip_basename(program);
    if base.eq_ignore_ascii_case("npx") || base.eq_ignore_ascii_case("bunx") {
        let mut idx = 0;
        while let Some(&arg) = args.get(idx) {
            if arg == "--" {
                idx += 1;
                break;
            }
            if matches!(arg, "-p" | "--package" | "-c" | "--call") {
                idx += 2;
                continue;
            }
            if arg.starts_with('-') {
                idx += 1;
                continue;
            }
            break;
        }
        let Some(&sub_prog) = args.get(idx) else {
            return false;
        };
        return is_mobile_heavy_command(sub_prog, &args[idx + 1..]);
    }
    if base.eq_ignore_ascii_case("pnpm") || base.eq_ignore_ascii_case("yarn") {
        let mut idx = 0;
        while let Some(&arg) = args.get(idx) {
            if arg == "--" {
                idx += 1;
                break;
            }
            if arg.starts_with('-') {
                idx += 1;
                continue;
            }
            if arg.eq_ignore_ascii_case("exec") || arg.eq_ignore_ascii_case("dlx") {
                idx += 1;
                continue;
            }
            break;
        }
        let Some(&sub_prog) = args.get(idx) else {
            return false;
        };
        return is_mobile_heavy_command(sub_prog, &args[idx + 1..]);
    }
    if base.eq_ignore_ascii_case("bundle") || base.eq_ignore_ascii_case("bundler") {
        let mut idx = 0;
        while let Some(&arg) = args.get(idx) {
            if arg.starts_with('-') {
                idx += 1;
                continue;
            }
            if arg.eq_ignore_ascii_case("exec") {
                let Some(&sub_prog) = args.get(idx + 1) else {
                    return false;
                };
                return is_mobile_heavy_command(sub_prog, &args[idx + 2..]);
            }
            return false;
        }
        return false;
    }

    if !is_mobile_candidate(base) {
        return false;
    }
    let Some(&subcmd) = args.iter().find(|arg| !arg.starts_with('-')) else {
        return false;
    };
    is_mobile_heavy_subcommand(base, subcmd)
}
