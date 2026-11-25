//! Idle-machine probing for `aaf gate --full --when-idle`.
//!
//! A background/full gate should only start when the machine is genuinely idle:
//! no active or orphaned heavy slots, no active external Gradle/Kotlin/Unity/
//! xcodebuild processes, normal memory pressure, and calm CPU load.

use std::collections::HashSet;
use std::path::Path;

use aaf_domain::config::MachineConfig;

use super::probe::{self, LoadProbe, Pressure};

const ACTIVE_DAEMON_MIN_CPU_PERCENT: f64 = 5.0;

/// Snapshot of machine activity evaluated by [`evaluate_idle`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IdleSnapshot {
    pub active_slots: usize,
    pub draining: bool,
    pub external_builds: Vec<String>,
    pub pressure: Option<Pressure>,
    pub load_percent: Option<u32>,
}

/// Pure idle evaluator: empty vector means the machine is idle and ready.
pub fn evaluate_idle(snapshot: &IdleSnapshot, max_load_percent: u32) -> Vec<String> {
    let mut blockers = Vec::new();
    if snapshot.active_slots > 0 {
        blockers.push(format!(
            "{} active heavy slot(s) in use",
            snapshot.active_slots
        ));
    }
    if snapshot.draining {
        blockers.push("heavy slot queue is draining for an exclusive build".to_string());
    }
    for build in &snapshot.external_builds {
        blockers.push(format!("external build running: {build}"));
    }
    if let Some(p @ (Pressure::Warn | Pressure::Critical)) = snapshot.pressure {
        blockers.push(format!("memory pressure is {p:?}"));
    }
    if max_load_percent > 0
        && let Some(load) = snapshot.load_percent
        && load > max_load_percent
    {
        blockers.push(format!(
            "CPU load {load}% exceeds idle ceiling {max_load_percent}%"
        ));
    }
    blockers
}

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

fn is_tracked_exe(name: &str) -> bool {
    matches!(name, "java" | "gradle" | "gradlew")
        || name.eq_ignore_ascii_case("xcodebuild")
        || name.eq_ignore_ascii_case("unity")
}

fn is_shell_or_system_bin(word: &str) -> bool {
    if word.starts_with("/bin/")
        || word.starts_with("/usr/bin/")
        || word.starts_with("/sbin/")
        || word.starts_with("/usr/sbin/")
    {
        return true;
    }
    matches!(
        basename(word),
        "sh" | "bash" | "zsh" | "dash" | "fish" | "ksh" | "csh" | "tcsh" | "env"
    )
}

/// Resolves `(exe_basename, args)` from the command tokens of a `ps` line.
///
/// `ps -o command=` separates argv with spaces without quoting spaces inside
/// `argv[0]`, so an executable path containing spaces (such as
/// `/Applications/Android Studio.app/Contents/jbr/Contents/Home/bin/java`) is
/// split across multiple whitespace tokens. When `cmd_words[0]` is a path that
/// is not a shell/system binary, scans contiguous non-flag, non-quoted,
/// non-new-path continuation tokens for a tracked build executable.
pub(crate) fn resolve_exe_and_args<'a>(cmd_words: &'a [&'a str]) -> (&'a str, &'a [&'a str]) {
    let Some((&first, rest)) = cmd_words.split_first() else {
        return ("", &[]);
    };
    let mut best = (basename(first), rest);
    let can_be_split_path =
        (first.starts_with('/') || first.starts_with("./") || first.starts_with("../"))
            && !is_shell_or_system_bin(first);
    if !can_be_split_path {
        return best;
    }
    for (offset, &word) in rest.iter().enumerate() {
        if word.starts_with('-')
            || word.starts_with('/')
            || word.starts_with('\'')
            || word.starts_with('"')
            || word.starts_with("./")
            || word.starts_with("../")
        {
            break;
        }
        if word.contains('/') {
            let candidate = basename(word);
            if is_tracked_exe(candidate) {
                best = (candidate, &rest[offset + 1..]);
            }
        }
    }
    best
}

/// Parses `ps -A -ww -o pid=,ppid=,pcpu=,command=` and returns human-readable
/// descriptions of active external build processes.
///
/// Unlike `daemons::parse_daemons`, this checks the actual executable name and
/// main class (avoiding false positives like `/bin/sh ... GradleDaemon`) and
/// ignores idle persistent `KotlinCompileDaemon` / `GradleDaemon` JVMs whose
/// CPU is below 5% unless their parent is a live `gradlew`/`gradle` launcher.
pub fn parse_active_external_builds(ps_output: &str) -> Vec<String> {
    struct ProcRow<'a> {
        pid: u32,
        ppid: u32,
        pcpu: f64,
        exe: &'a str,
        args: &'a [&'a str],
    }

    let mut rows: Vec<ProcRow<'_>> = Vec::new();
    let mut words_buf: Vec<Vec<&str>> = Vec::new();
    for line in ps_output.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.len() >= 4 {
            words_buf.push(words);
        }
    }
    for words in &words_buf {
        let Ok(pid) = words[0].parse::<u32>() else {
            continue;
        };
        let Ok(ppid) = words[1].parse::<u32>() else {
            continue;
        };
        let pcpu = words[2].parse::<f64>().unwrap_or(0.0);
        let (exe, args) = resolve_exe_and_args(&words[3..]);
        rows.push(ProcRow {
            pid,
            ppid,
            pcpu,
            exe,
            args,
        });
    }

    let gradle_launchers: HashSet<u32> = rows
        .iter()
        .filter(|r| {
            matches!(r.exe, "gradle" | "gradlew")
                || (r.exe == "java"
                    && r.args.iter().any(|a| {
                        a.contains("gradle-wrapper.jar")
                            || *a == "org.gradle.wrapper.GradleWrapperMain"
                    }))
        })
        .map(|r| r.pid)
        .collect();

    let mut active = Vec::new();
    for r in &rows {
        if r.exe.eq_ignore_ascii_case("xcodebuild") {
            active.push(format!("xcodebuild (pid={})", r.pid));
            continue;
        }
        if r.exe.eq_ignore_ascii_case("unity")
            && r.args
                .iter()
                .any(|a| matches!(*a, "-batchmode" | "-runTests" | "-executeMethod"))
        {
            active.push(format!("Unity batchmode (pid={})", r.pid));
            continue;
        }
        if r.exe == "java" {
            let is_gradle_daemon = r
                .args
                .contains(&"org.gradle.launcher.daemon.bootstrap.GradleDaemon");
            let is_kotlin_daemon = r
                .args
                .contains(&"org.jetbrains.kotlin.daemon.KotlinCompileDaemon");
            if is_gradle_daemon || is_kotlin_daemon {
                let busy =
                    r.pcpu >= ACTIVE_DAEMON_MIN_CPU_PERCENT || gradle_launchers.contains(&r.ppid);
                if busy {
                    let kind = if is_gradle_daemon {
                        "GradleDaemon"
                    } else {
                        "KotlinCompileDaemon"
                    };
                    active.push(format!("{kind} (pid={}, cpu={:.1}%)", r.pid, r.pcpu));
                }
            }
        }
    }
    active
}

/// Probes the live machine for active external builds via `ps`.
pub fn active_external_builds() -> Vec<String> {
    probe::tool_output(probe::PS, &["-A", "-ww", "-o", "pid=,ppid=,pcpu=,command="])
        .map(|out| parse_active_external_builds(&out))
        .unwrap_or_default()
}

/// Collects live machine state and returns every reason the machine is not idle.
pub fn probe_idle(data_dir: &Path, machine: &MachineConfig, probe: &dyn LoadProbe) -> Vec<String> {
    let throttle = super::inspect::inspect_throttle(data_dir);
    let snapshot = IdleSnapshot {
        active_slots: throttle.throttle_slots,
        draining: throttle.drain_lock_present,
        external_builds: active_external_builds(),
        pressure: probe.pressure(),
        load_percent: probe.load_percent(),
    };
    evaluate_idle(&snapshot, machine.idle_max_load_percent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluate_idle_returns_empty_only_when_all_clear() {
        let calm = IdleSnapshot {
            active_slots: 0,
            draining: false,
            external_builds: vec![],
            pressure: Some(Pressure::Normal),
            load_percent: Some(45),
        };
        assert!(evaluate_idle(&calm, 100).is_empty());

        let busy = IdleSnapshot {
            active_slots: 1,
            draining: true,
            external_builds: vec!["GradleDaemon (pid=12, cpu=90.0%)".to_string()],
            pressure: Some(Pressure::Warn),
            load_percent: Some(180),
        };
        let reasons = evaluate_idle(&busy, 100);
        assert_eq!(reasons.len(), 5, "{reasons:?}");
    }

    #[test]
    fn parse_active_external_builds_ignores_shell_strings_and_idle_daemons() {
        let ps = "\
  86698     1   0.0 /bin/sh -c while :; do sleep 1; done GradleDaemon
  96641     1   0.0 /Library/Java/bin/java -Xmx2g org.jetbrains.kotlin.daemon.KotlinCompileDaemon
  11820  7697   0.1 /opt/jdk/bin/java -jar /repo/gradle/wrapper/gradle-wrapper.jar :app:lintDebug
  11844 11820   2.0 /opt/jdk/bin/java -Xmx4g org.gradle.launcher.daemon.bootstrap.GradleDaemon 9.8.0
  22000     1 120.5 /opt/jdk/bin/java -Xmx4g org.gradle.launcher.daemon.bootstrap.GradleDaemon 9.8.0
  33000     1   1.0 /Applications/Unity/Hub/Editor/6000.6.0f1/Unity.app/Contents/MacOS/Unity -batchmode -runTests
";
        let found = parse_active_external_builds(ps);
        assert_eq!(found.len(), 3, "{found:?}");
        assert!(found[0].contains("GradleDaemon (pid=11844"));
        assert!(found[1].contains("GradleDaemon (pid=22000"));
        assert!(found[2].contains("Unity batchmode (pid=33000"));
    }

    #[test]
    fn resolve_exe_and_args_handles_spaces_in_jdk_and_build_tool_paths() {
        let ps = "\
  44000     1  85.0 /Applications/Android Studio.app/Contents/jbr/Contents/Home/bin/java -Xmx4g org.gradle.launcher.daemon.bootstrap.GradleDaemon 9.8.0
  44100     1   1.0 /Applications/Xcode 16.app/Contents/Developer/usr/bin/xcodebuild -scheme App
  44200     1   1.0 /Applications/Unity Hub/Editor/6000.6/Unity.app/Contents/MacOS/Unity -batchmode -runTests
  44300     1   0.1 /Users/alice/My Cool App/gradlew :app:testDebug
  44301 44300   1.0 /Applications/Android Studio.app/Contents/jbr/Contents/Home/bin/java -Xmx4g org.gradle.launcher.daemon.bootstrap.GradleDaemon 9.8.0
  44400     1  50.0 /bin/sh -c '/Applications/Android Studio.app/Contents/jbr/Contents/Home/bin/java org.gradle.launcher.daemon.bootstrap.GradleDaemon'
  44500     1  50.0 /bin/sh -c 'echo GradleDaemon'
";
        let found = parse_active_external_builds(ps);
        assert_eq!(found.len(), 4, "{found:?}");
        assert!(found[0].contains("GradleDaemon (pid=44000"));
        assert!(found[1].contains("xcodebuild (pid=44100"));
        assert!(found[2].contains("Unity batchmode (pid=44200"));
        assert!(found[3].contains("GradleDaemon (pid=44301"));

        let (exe, args) = resolve_exe_and_args(&[
            "/Applications/Android",
            "Studio.app/Contents/jbr/Contents/Home/bin/java",
            "-Xmx4g",
            "org.gradle.launcher.daemon.bootstrap.GradleDaemon",
        ]);
        assert_eq!(exe, "java");
        assert_eq!(
            args,
            &[
                "-Xmx4g",
                "org.gradle.launcher.daemon.bootstrap.GradleDaemon"
            ]
        );

        let (sh_exe, _) = resolve_exe_and_args(&["/bin/sh", "-c", "echo GradleDaemon"]);
        assert_eq!(sh_exe, "sh");
    }
}
