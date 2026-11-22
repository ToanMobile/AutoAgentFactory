//! Heavy builds (Gradle, xcodebuild, Unity in batch mode) each want the
//! whole machine; a handful started side by side freeze it. They run through
//! the machine queue instead: `aaf slot -- <command>`, or the gate, which
//! queues its own builds.

use crate::Denial;
use crate::shell::{Cmd, DECLARERS, SHELLS, basename};

const RULE: &str = "heavy_cmd_guard";
const REASON: &str = "heavy builds must share the machine queue";
/// The reader is an agent. Inside a factory run `aaf gate` cannot work (the
/// factory runs the gate outside the sandbox), so it is not offered as a
/// way to build; a command under `aaf gate` is still allowed by the rule.
const FIX: &str = "aaf slot -- <command> (aaf gate is run by the factory or the operator, not by \
                   an agent)";

const GRADLE: &[&str] = &["gradlew", "gradle"];
/// A Gradle call is light only when every argument is one of these. `help`,
/// `tasks`, `projects` and `properties` are not: they configure the project
/// and leave a daemon behind like any other task.
const GRADLE_LIGHT: &[&str] = &[
    "--version",
    "-v",
    "--status",
    "--stop",
    "--help",
    "-h",
    "-?",
];
const XCODEBUILD: &str = "xcodebuild";
/// Options that make xcodebuild print and exit.
const XCODEBUILD_LIGHT: &[&str] = &[
    "-version",
    "-showsdks",
    "-list",
    "-help",
    "-showBuildSettings",
];
/// The options that only say what to print about, each with one value. An
/// xcodebuild call is light when it has a light option and nothing but these
/// around it: any other word is an action or an option that starts work
/// (`-list -resolvePackageDependencies`, `-showBuildSettings -exportArchive`).
const XCODEBUILD_SELECTORS: &[&str] = &[
    "-project",
    "-workspace",
    "-scheme",
    "-target",
    "-configuration",
    "-sdk",
    "-arch",
    "-destination",
];
const XCODEBUILD_PLAIN: &[&str] = &["-json", "-quiet", "-verbose"];
const UNITY: &[&str] = &["unity", "unity-editor"];
/// Unity without one of these opens the editor window, which is not a
/// queued build. Unity reads its options without regard to case.
const UNITY_HEAVY: &[&str] = &["-batchmode", "-runtests", "-executemethod"];

fn named(program: &str, names: &[&str]) -> bool {
    // The default macOS file system finds `./Gradlew` too.
    names.iter().any(|name| name.eq_ignore_ascii_case(program))
}

/// `aaf slot` marks the commands it starts with this variable and lets a
/// command that carries it run at once. Only aaf sets it.
const SLOT_VAR: &str = "AAF_HEAVY_SLOT";
const SLOT_VAR_REASON: &str =
    "AAF_HEAVY_SLOT is set by aaf slot alone; setting it by hand jumps the machine queue";
const SLOT_VAR_FIX: &str = "drop the AAF_HEAVY_SLOT assignment and run aaf slot -- <command>";

/// Every value is refused, so the format aaf gives the variable does not
/// matter here. Reading it (`echo $AAF_HEAVY_SLOT`) and `unset` stay free.
pub fn check(cmd: &Cmd) -> Option<Denial> {
    if cmd.sets_var(SLOT_VAR) {
        return Some(Denial::new(RULE, SLOT_VAR_REASON, SLOT_VAR_FIX));
    }
    if cmd.slotted {
        return None;
    }
    let (program, args) = invoked(cmd);
    // A command word built at run time could be a wrapper (`env`, a forged
    // `aaf slot --`): a heavy tool among its arguments is then the command.
    let heavy = is_heavy(program, args, cmd.xargs) || (cmd.dynamic_name && mentions(args));
    heavy.then(|| Denial::new(RULE, REASON, FIX))
}

/// Judged on the whole line: `export "$V"=0` hides which variable it sets,
/// so next to a command the queue vouches for it is taken for the slot
/// variable. Without a slot on the line it is ordinary shell.
pub fn check_script(cmds: &[Cmd]) -> Option<Denial> {
    let hidden = |cmd: &Cmd| {
        DECLARERS.contains(&cmd.name())
            && cmd.args().iter().any(|arg| {
                let name = arg.split('=').next().unwrap_or(arg);
                !arg.starts_with('-') && (name.is_empty() || name.contains(['$', '`']))
            })
    };
    (cmds.iter().any(|cmd| cmd.slotted) && cmds.iter().any(hidden)).then(|| {
        Denial::new(
            RULE,
            "a variable named at run time is set next to aaf slot; it could be AAF_HEAVY_SLOT",
            SLOT_VAR_FIX,
        )
    })
}

/// The program a command runs and its arguments: for `sh gradlew build` the
/// script the shell is handed. (`sh -c '…'` is split by the shell module.)
fn invoked(cmd: &Cmd) -> (&str, &[String]) {
    let args = cmd.args();
    if !SHELLS.contains(&cmd.name()) {
        return (cmd.name(), args);
    }
    let mut at = 0;
    while let Some(arg) = args.get(at) {
        if arg == "-o" || arg == "+o" {
            at += 2;
        } else if arg.starts_with(['-', '+']) {
            at += 1;
        } else {
            return (basename(arg), args.get(at + 1..).unwrap_or(&[]));
        }
    }
    ("", &[])
}

/// `xargs`: further arguments come from its input and could be anything.
fn is_heavy(program: &str, args: &[String], xargs: bool) -> bool {
    let is = |arg: &String, set: &[&str]| set.contains(&arg.as_str());
    if named(program, GRADLE) {
        xargs || args.is_empty() || !args.iter().all(|arg| is(arg, GRADLE_LIGHT))
    } else if named(program, &[XCODEBUILD]) {
        xargs || !xcodebuild_prints_only(args)
    } else if named(program, UNITY) {
        // An argument filled in at run time could be one of the batch ones.
        xargs
            || args
                .iter()
                .any(|arg| arg.is_empty() || arg.contains('$') || named(arg, UNITY_HEAVY))
    } else {
        false
    }
}

/// A light option, and every other word accounted for: a selector with its
/// value, a plain output option, or a build setting (`KEY=value`). A word
/// filled in at run time could be anything, an action included.
fn xcodebuild_prints_only(args: &[String]) -> bool {
    let mut light = false;
    let mut words = args.iter().map(String::as_str);
    while let Some(word) = words.next() {
        if word.is_empty() || word.contains(['$', '`']) {
            return false;
        } else if XCODEBUILD_LIGHT.contains(&word) {
            light = true;
        } else if XCODEBUILD_SELECTORS.contains(&word) {
            // The value is not read as an option, whatever it looks like.
            if words.next().is_none_or(|value| value.contains(['$', '`'])) {
                return false;
            }
        } else if !(XCODEBUILD_PLAIN.contains(&word)
            || (!word.starts_with('-') && word.contains('=')))
        {
            return false;
        }
    }
    light
}

/// Text-level check for input that could not be split into commands: one of
/// the tools is named, whatever its arguments.
pub fn mentions<S: AsRef<str>>(tokens: &[S]) -> bool {
    tokens.iter().any(|token| {
        let program = basename(token.as_ref());
        named(program, GRADLE) || named(program, &[XCODEBUILD]) || named(program, UNITY)
    })
}
