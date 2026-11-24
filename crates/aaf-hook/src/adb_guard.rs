//! Keeps adb/fastboot away from devices the owner excluded and blocks the
//! commands that can brick or wipe a device.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use crate::shell::{Cmd, basename};
use crate::{Ctx, Denial};

const RULE: &str = "adb_guard";
const FIX_SERIAL: &str =
    "pass -s <serial> with a literal serial that the adb allowlist/denylist permits";
const FIX_DESTRUCTIVE: &str = "skip destructive device commands (flash/wipe/bootloader/recovery) \
                               and use safe adb operations (install, logcat, shell pm clear, \
                               instrumentation tests) instead; this needs a human: finish the \
                               task without it and name the step in your final message";

/// Subcommands that talk to the adb server only, never to a device.
const NO_DEVICE: &[&str] = &[
    "devices",
    "version",
    "help",
    "start-server",
    "kill-server",
    "--version",
    "--help",
];

struct Call<'a> {
    serial: Option<&'a str>,
    wipe_flag: bool,
    rest: &'a [String],
}

fn parse(args: &[String]) -> Call<'_> {
    let mut serial = None;
    let mut wipe_flag = false;
    let mut at = 0;
    while let Some(arg) = args.get(at).map(String::as_str) {
        if !arg.starts_with('-') {
            break;
        }
        at += 1;
        if matches!(arg, "-t" | "-H" | "-P" | "-L" | "--one-device") {
            at += 1;
            continue;
        }
        wipe_flag |= is_wipe(arg);
        // `-s SERIAL`, `-sSERIAL` and, in a fastboot cluster, `-vs SERIAL`
        // and `-vsSERIAL`: the serial is what follows the first `s`. An `S`
        // (fastboot's sparse size) before it takes the rest instead.
        let valued = cluster(arg).and_then(|letters| {
            let at = letters.find(['s', 'S'])?;
            Some(letters.split_at(at))
        });
        if let Some((_, option)) = valued {
            let attached = &option[1..];
            let value = if attached.is_empty() {
                at += 1;
                args.get(at - 1).map(String::as_str)
            } else {
                Some(attached)
            };
            if option.starts_with('s') {
                serial = value;
            }
        }
    }
    Call {
        serial,
        wipe_flag,
        rest: args.get(at..).unwrap_or(&[]),
    }
}

/// The letters of a short-option word (`vw` for `-vw`).
fn cluster(arg: &str) -> Option<&str> {
    arg.strip_prefix('-')
        .filter(|letters| !letters.is_empty() && !letters.starts_with('-'))
}

/// fastboot's wipe flag, alone or in a cluster (`-w`, `-vw`). Letters after
/// an option that takes a value (`-s`, `-S`) are that value, not flags.
fn is_wipe(arg: &str) -> bool {
    let flags = cluster(arg).and_then(|letters| letters.split(['s', 'S']).next());
    flags.is_some_and(|flags| flags.contains('w'))
}

pub fn check(cmd: &Cmd, ctx: &Ctx) -> Option<Denial> {
    // `$ADB`, `${FASTBOOT}`: an expansion that names the tool is the tool.
    let word = cmd
        .words
        .first()
        .map_or("", String::as_str)
        .to_ascii_lowercase();
    let is = |tool: &str| cmd.name() == tool || (cmd.dynamic_name && word.contains(tool));
    let (is_adb, is_fastboot) = (is("adb"), is("fastboot"));
    if !is_adb && !is_fastboot {
        return None;
    }
    let call = parse(cmd.args());
    let destructive = if is_fastboot {
        fastboot_destructive(&call)
    } else {
        adb_destructive(&call)
    };
    if let Some(what) = destructive {
        return Some(Denial::new(RULE, what, FIX_DESTRUCTIVE));
    }
    serial_rule(cmd, &call, ctx).map(|reason| Denial::new(RULE, reason, FIX_SERIAL))
}

fn fastboot_destructive(call: &Call) -> Option<String> {
    // Every word is looked at: fastboot options take values (`--slot a`), so
    // the first word after the options need not be the subcommand.
    let word_at = |at: usize| call.rest.get(at).map_or("", String::as_str);
    let sub = (0..call.rest.len())
        .map(word_at)
        .enumerate()
        .find(|(at, word)| match *word {
            "flashing" | "oem" => word_at(at + 1).starts_with("unlock"),
            "erase" => true,
            _ if is_wipe(word) => true,
            _ => {
                !word.contains(['/', '.'])
                    && (word.starts_with("flash") || word.starts_with("format"))
            }
        });
    let sub = sub
        .map(|(_, word)| word)
        .or(call.wipe_flag.then_some("-w"))?;
    Some(format!(
        "fastboot {sub} rewrites, wipes or unlocks the device"
    ))
}

fn adb_destructive(call: &Call) -> Option<String> {
    let sub = call.rest.first().map_or("", String::as_str);
    let next = call.rest.get(1).map_or("", String::as_str);
    match sub {
        "reboot-bootloader" => Some("adb reboot-bootloader leaves Android".to_string()),
        "reboot" if matches!(next, "bootloader" | "recovery" | "fastboot" | "sideload") => {
            Some(format!("adb reboot {next} leaves Android"))
        }
        "shell" | "exec-out" => {
            let remote = call.rest.get(1..).unwrap_or(&[]).join(" ");
            remote_destructive(&remote).map(|what| format!("adb shell would run {what}"))
        }
        _ => None,
    }
}

/// Matches destructive wipe commands and flags (`wipe`, `wipefs`, `wipe-data`,
/// `wipe_data`, `--wipe_data`, `--wipe_cache`, `factory-wipe`, `data_wipe`)
/// without falsely matching words like `swipe` or `SwipeTest`.
pub(crate) fn is_wipe_token(token: &str) -> bool {
    let name = basename(token).to_ascii_lowercase();
    let lower = token.to_ascii_lowercase();
    name == "wipe"
        || name == "wipefs"
        || lower == "--wipe_data"
        || lower == "--wipe_cache"
        || name
            .strip_prefix("wipe")
            .is_some_and(|rest| rest.starts_with(['-', '_', '.']))
        || lower.contains("_wipe")
        || lower.contains("-wipe")
}

/// Looks for destructive commands in the text the device shell will run.
/// Text-based on purpose: the device re-parses the joined arguments.
pub fn remote_destructive(remote: &str) -> Option<&'static str> {
    for segment in remote.split([';', '|', '&', '\n']) {
        let tokens: Vec<&str> = segment
            .split(|c: char| c.is_whitespace() || "()\"'`".contains(c))
            .filter(|t| !t.is_empty())
            .collect();
        for (at, token) in tokens.iter().enumerate() {
            let name = basename(token);
            let after = &tokens[at + 1..];
            if is_wipe_token(token) {
                return Some("a wipe");
            }
            if name == "dd" {
                return Some("dd");
            }
            if name.starts_with("mkfs") {
                return Some("mkfs");
            }
            if name == "reboot" && after.first() == Some(&"-p") {
                return Some("reboot -p (power off)");
            }
            // `run-as <pkg> rm …` runs in the app's own data directory: a
            // plain relative path there is the app's data, not a device root.
            // Only when `run-as <pkg>` is what runs `rm`, and no substitution
            // could run an `rm` of its own.
            let app_dir = at >= 2
                && basename(tokens[at - 2]) == "run-as"
                && !segment.contains("$(")
                && !segment.contains('`');
            if name == "rm"
                && after
                    .iter()
                    .any(|a| a.starts_with('-') && a.contains(['r', 'R']))
                && after
                    .iter()
                    .any(|a| is_device_root(a) && !(app_dir && in_app_dir(a)))
            {
                return Some("rm -r on a top-level device directory");
            }
        }
    }
    None
}

/// `/`, `/*`, `/sdcard`, `/data/*`: at most one real path component. A
/// target the device resolves (a variable, `~`, a `..` step, or a path
/// relative to a working directory that may be `/`) could be one too.
fn is_device_root(target: &str) -> bool {
    // An option, or a redirection the device shell applies.
    if target.starts_with('-') || target.contains(['<', '>']) {
        return false;
    }
    if !target.starts_with('/')
        || target.contains(['$', '`', '~'])
        || target.split('/').any(|part| part == "..")
    {
        return true;
    }
    target
        .split('/')
        .filter(|part| !part.is_empty() && *part != "*" && *part != ".")
        .count()
        <= 1
}

/// A relative path that stays where it starts: no `..` step, variable,
/// `~` or substitution the device would resolve.
fn in_app_dir(target: &str) -> bool {
    !target.starts_with(['/', '-'])
        && !target.contains(['$', '`', '~', '<', '>'])
        && !target.split('/').any(|part| part == "..")
}

fn serial_rule(cmd: &Cmd, call: &Call, ctx: &Ctx) -> Option<String> {
    let root = ctx.root();
    let lists = read_list(root.as_deref(), ctx, "denylist")
        .and_then(|deny| Ok((deny, read_list(root.as_deref(), ctx, "allowlist")?)));
    let (deny, allow) = match lists {
        Ok(lists) => lists,
        Err(file) => return Some(format!("cannot read the adb list {file}")),
    };
    if deny.is_empty() && allow.is_empty() {
        return None;
    }
    let sub = call.rest.first().map_or("", String::as_str);
    if call.rest.is_empty() || NO_DEVICE.contains(&sub) {
        return None;
    }
    let from_env = cmd
        .assigns
        .iter()
        .find(|(key, _)| key == "ANDROID_SERIAL")
        .map(|(_, value)| value.as_str());
    let Some(serial) = call.serial.or(from_env) else {
        return Some("no -s <serial> while an adb denylist/allowlist is in force".to_string());
    };
    if serial.is_empty() || serial.contains(['$', '`', '*', '?']) {
        return Some(format!("serial {serial} is not a literal value"));
    }
    if deny.iter().any(|s| s == serial) {
        return Some(format!("device {serial} is on the adb denylist"));
    }
    if !allow.is_empty() && !allow.iter().any(|s| s == serial) {
        return Some(format!("device {serial} is not on the adb allowlist"));
    }
    None
}

/// Serials from the repo's `.adb-<kind>` and the global `adb-<kind>`.
/// A list that exists but cannot be read is an error, never "empty".
fn read_list(root: Option<&Path>, ctx: &Ctx, kind: &str) -> Result<Vec<String>, String> {
    let mut files = Vec::new();
    if let Some(root) = root {
        files.push(root.join(format!(".adb-{kind}")));
    }
    if let Some(config) = &ctx.env.config_dir {
        files.push(config.join(format!("adb-{kind}")));
        files.push(config.join(format!(".adb-{kind}")));
    }
    let mut serials = Vec::new();
    for file in files {
        match fs::read_to_string(&file) {
            Ok(text) => serials.extend(
                text.lines()
                    .map(|line| line.split('#').next().unwrap_or("").trim().to_string())
                    .filter(|line| !line.is_empty()),
            ),
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(_) => return Err(file.display().to_string()),
        }
    }
    Ok(serials)
}
