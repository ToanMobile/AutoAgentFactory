//! Build daemons: switched off through the environment, the build tool
//! runs alone whatever was measured of it, and a daemon that is left
//! running is reported.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;

use super::super::daemons::{
    BuildDaemon, GRADLE_DAEMON_OFF, KOTLIN_IN_PROCESS, gradle_opts, parse_daemons, read,
    started_since,
};
use super::super::run::Inherited;
use super::super::slots::{Admission, Busy, try_acquire};
use super::super::stats::{HeavySample, record};
use super::super::token::SlotToken;
use super::super::{Ask, HeavyJob, HeavyRun, heavy_tool};
use super::support::{GIB, Rig, answered};

const REPO: &str = "repo";
const SMALL: Admission = Admission::Measured { peak_rss_bytes: 1 };

fn plenty() -> Option<u64> {
    Some(64 * GIB)
}

fn job<'a>(cwd: &'a Path, command: &'a str) -> HeavyJob<'a> {
    HeavyJob {
        repo_id: REPO,
        label: "test_full",
        command,
        cwd,
        timeout: Duration::from_secs(120),
        log_tail_bytes: 4096,
    }
}

/// A directory with a `gradlew` that does nothing: the name is what the
/// throttle goes by, and no real Gradle is ever started.
fn fake_gradle_project() -> TempDir {
    let work = TempDir::new().unwrap();
    let gradlew = work.path().join("gradlew");
    fs::write(&gradlew, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&gradlew, fs::Permissions::from_mode(0o755)).unwrap();
    work
}

fn daemon(pid: u32, kind: &'static str) -> BuildDaemon {
    BuildDaemon {
        pid,
        kind,
        started: "Sun Nov 22 09:12:08 2026".to_string(),
        rss_bytes: 4 * GIB,
    }
}

#[test]
fn gradle_opts_keep_what_was_there_and_add_each_switch_once() {
    let both = format!("{GRADLE_DAEMON_OFF} {KOTLIN_IN_PROCESS}");
    assert_eq!(gradle_opts(None), OsString::from(&both));
    assert_eq!(gradle_opts(Some(OsStr::new(""))), OsString::from(&both));
    assert_eq!(
        gradle_opts(Some(OsStr::new("-Xmx2g -Dfile.encoding=UTF-8"))),
        OsString::from(format!("-Xmx2g -Dfile.encoding=UTF-8 {both}"))
    );
    // A caller that enables the daemon is overruled by the later flag.
    assert_eq!(
        gradle_opts(Some(OsStr::new("-Dorg.gradle.daemon=true"))),
        OsString::from(format!("-Dorg.gradle.daemon=true {both}"))
    );
    // Its own result goes through unchanged (an `aaf slot` inside a gate).
    assert_eq!(gradle_opts(Some(OsStr::new(&both))), OsString::from(&both));
}

#[test]
fn heavy_command_really_gets_the_daemon_switches_after_its_own_gradle_opts() {
    let data = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let rig = Rig::calm();
    let print = job(
        work.path(),
        "printf '%s|%s' \"$GRADLE_OPTS\" \"$AAF_HEAVY_SLOT\"",
    );
    let output = |inherited: Inherited| {
        let run = rig
            .throttle()
            .run_heavy_with(data.path(), &*rig.clock, &print, &inherited)
            .unwrap();
        assert_eq!(run.exit_code, Some(0), "{run:?}");
        let output = String::from_utf8(run.output).unwrap();
        // The slot comes with the token of the run: `0:<token>`.
        let (opts, slot) = output.split_once('|').unwrap();
        let token = slot.strip_prefix("0:").and_then(SlotToken::parse);
        assert!(token.is_some(), "{output}");
        format!("{opts}|0")
    };

    let kept = output(Inherited {
        gradle_opts: Some("-Xmx2g -Dfoo=bar".into()),
    });
    assert_eq!(
        kept,
        format!("-Xmx2g -Dfoo=bar {GRADLE_DAEMON_OFF} {KOTLIN_IN_PROCESS}|0")
    );
    let bare = output(Inherited::default());
    assert_eq!(bare, format!("{GRADLE_DAEMON_OFF} {KOTLIN_IN_PROCESS}|0"));
}

/// Runs `command` on a machine with two slots, plenty of memory and a small
/// measured command in slot 0 that leaves at the first poll. The command
/// itself was measured as tiny.
fn run_beside_a_small_command(command: &str) -> (HeavyRun, Rig) {
    let data = TempDir::new().unwrap();
    let work = fake_gradle_project();
    let rig = Rig::calm();
    let sample = HeavySample {
        peak_rss_bytes: 1024,
        duration_ms: 1,
    };
    record(data.path(), REPO, "test_full", sample, &rig.sleeper).unwrap();
    let holder = try_acquire(data.path(), 2, SMALL, &plenty)
        .unwrap()
        .unwrap();
    let holder = Arc::new(Mutex::new(Some(holder)));
    let release = Arc::clone(&holder);
    rig.sleeper.on_sleep(move |_| {
        release.lock().unwrap().take();
    });
    let run = rig
        .throttle()
        .run_heavy(data.path(), &*rig.clock, &job(work.path(), command))
        .unwrap();
    assert_eq!(run.exit_code, Some(0), "{run:?}");
    (run, rig)
}

#[test]
fn measured_gradle_command_still_waits_until_no_other_slot_is_busy() {
    // The control: any other measured command shares the machine at once.
    let (run, rig) = run_beside_a_small_command("true");
    assert_eq!((run.slot, rig.sleeper.count()), (1, 0));

    for command in ["./gradlew test", "sh -c 'cd . && ./gradlew test'"] {
        let (run, rig) = run_beside_a_small_command(command);
        assert_eq!(run.slot, 0, "{command}");
        assert_eq!(rig.sleeper.count(), 1, "{command}");
        assert!(run.waited_secs >= 1);
        assert!(
            rig.events()[0].starts_with("WaitingForSlot") && rig.events()[0].contains("RunsAlone"),
            "{:?}",
            rig.events()
        );
    }
}

#[test]
fn running_gradle_command_blocks_every_other_command() {
    let data = TempDir::new().unwrap();
    let rig = Rig::calm();
    let sample = HeavySample {
        peak_rss_bytes: 1024,
        duration_ms: 1,
    };
    record(data.path(), REPO, "test_full", sample, &rig.sleeper).unwrap();
    let ask = Ask {
        repo_id: REPO,
        label: "test_full",
        tool: heavy_tool("./gradlew test"),
        cmd_family: Some("gradle"),
        exclusive: heavy_tool("./gradlew test").is_some(),
        wait_limit_secs: None,
    };
    let admitted = rig
        .throttle()
        .admit_ask(data.path(), &*rig.clock, &ask)
        .unwrap()
        .unwrap();
    assert_eq!(admitted.slot.index(), 0);

    let second = try_acquire(data.path(), 2, SMALL, &plenty).unwrap();
    assert_eq!(second.unwrap_err(), Busy::SlotsTaken);
    drop(admitted);
    assert!(try_acquire(data.path(), 2, SMALL, &plenty).unwrap().is_ok());
}

#[test]
fn daemon_started_during_a_gradle_command_is_reported_and_others_are_not() {
    let data = TempDir::new().unwrap();
    let work = fake_gradle_project();
    let rig = Rig::calm();
    let old = daemon(100, "gradle");
    let listings = || {
        vec![
            vec![old.clone()],
            vec![old.clone(), daemon(4321, "gradle"), daemon(4400, "kotlin")],
        ]
    };

    *rig.group.daemons.lock().unwrap() = listings().into();
    let run = rig
        .throttle()
        .run_heavy(
            data.path(),
            &*rig.clock,
            &job(work.path(), "./gradlew test"),
        )
        .unwrap();
    assert_eq!(
        run.leftover_daemons,
        vec![daemon(4321, "gradle"), daemon(4400, "kotlin")]
    );
    assert!(rig.group.daemons.lock().unwrap().is_empty());

    // A command that starts no Gradle is not blamed for somebody's daemon,
    // and no listing is taken for it.
    *rig.group.daemons.lock().unwrap() = listings().into();
    let run = rig
        .throttle()
        .run_heavy(data.path(), &*rig.clock, &job(work.path(), "true"))
        .unwrap();
    assert_eq!(run.leftover_daemons, vec![]);
    assert_eq!(rig.group.daemons.lock().unwrap().len(), 2);
}

#[test]
fn daemons_are_read_from_the_ps_listing() {
    let listing = "\
  100  4194304 Sun Nov  1 09:12:08 2026 /usr/bin/java -Xmx4g -cp /g/lib/gradle-daemon-main-8.9.jar org.gradle.launcher.daemon.bootstrap.GradleDaemon 8.9
  200   524288 Sun Nov 22 10:00:00 2026 /usr/bin/java -cp /k/kotlin-compiler.jar org.jetbrains.kotlin.daemon.KotlinCompileDaemon --daemon-runFilesPath /tmp
  300    10240 Sun Nov 22 10:00:01 2026 /usr/bin/java -jar app.jar
  bad     1024 Sun Nov 22 10:00:02 2026 java GradleDaemon
  400 GradleDaemon
";
    assert_eq!(
        parse_daemons(listing),
        vec![
            BuildDaemon {
                pid: 100,
                kind: "gradle",
                started: "Sun Nov 1 09:12:08 2026".to_string(),
                rss_bytes: 4 * GIB,
            },
            BuildDaemon {
                pid: 200,
                kind: "kotlin",
                started: "Sun Nov 22 10:00:00 2026".to_string(),
                rss_bytes: GIB / 2,
            },
        ]
    );
    assert_eq!(parse_daemons(""), vec![]);
}

#[test]
fn only_a_new_process_counts_as_started_since() {
    let before = vec![daemon(100, "gradle"), daemon(200, "kotlin")];
    // Pid 200 was given to a new daemon in the meantime.
    let reused = BuildDaemon {
        started: "Sun Nov 22 11:00:00 2026".to_string(),
        ..daemon(200, "kotlin")
    };
    let after = vec![daemon(100, "gradle"), reused.clone(), daemon(300, "gradle")];
    assert_eq!(
        started_since(&before, after),
        vec![reused, daemon(300, "gradle")]
    );
    assert_eq!(started_since(&before, before.clone()), vec![]);
}

#[test]
fn real_ps_lists_a_process_that_looks_like_a_gradle_daemon() {
    // A shell loop whose argument list carries the daemon's class name.
    let mut child = Command::new("/bin/sh")
        .args(["-c", "while :; do sleep 1; done", "GradleDaemon"])
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    // The listing `list` reads; a `ps` that gave no answer is asked again.
    let found = answered("daemon listing", read);
    child.kill().unwrap();
    child.wait().unwrap();

    let mine = found.iter().find(|daemon| daemon.pid == child.id());
    let mine = mine.unwrap_or_else(|| panic!("not listed: {found:?}"));
    assert_eq!(mine.kind, "gradle");
    assert!(
        mine.rss_bytes > 0 && mine.started.split(' ').count() == 5,
        "{mine:?}"
    );
}

/// Seen on a machine where another session built the same app in another
/// checkout: its daemon started during this command and the gate's receipt
/// said this command's builds "are not using --no-daemon".
#[test]
fn daemon_of_a_build_in_another_directory_is_not_blamed_on_this_command() {
    let data = TempDir::new().unwrap();
    let work = fake_gradle_project();
    let elsewhere = TempDir::new().unwrap();
    let rig = Rig::calm();
    let mine = work.path().join("app");
    fs::create_dir(&mine).unwrap();
    // Where the daemons really work, read with `lsof -a -p <pid> -d cwd`
    // on macOS (2026-11-22): a Gradle daemon started by `gradlew` in the
    // directory `gradlew` was called from; a Gradle daemon that outlived
    // its client (ppid 1) in `<gradle home>/daemon/<version>`; a Kotlin
    // compile daemon in `~/Library/Application Support/kotlin/daemon`,
    // whatever project it compiles.
    let home = TempDir::new().unwrap();
    let kotlin_home = home
        .path()
        .join("Library/Application Support/kotlin/daemon");
    let gradle_home = home.path().join(".gradle/daemon/9.8.0");
    fs::create_dir_all(&kotlin_home).unwrap();
    fs::create_dir_all(&gradle_home).unwrap();
    *rig.group.cwds.lock().unwrap() = [
        (4321, elsewhere.path().to_path_buf()),
        (4400, kotlin_home),
        // 4500: its directory cannot be read.
        (4600, gradle_home),
        (4700, mine),
    ]
    .into();
    *rig.group.daemons.lock().unwrap() = vec![
        Vec::new(),
        vec![
            daemon(4321, "gradle"),
            daemon(4400, "kotlin"),
            daemon(4500, "gradle"),
            daemon(4600, "gradle"),
            daemon(4700, "gradle"),
        ],
    ]
    .into();
    let run = rig
        .throttle()
        .run_heavy(
            data.path(),
            &*rig.clock,
            &job(work.path(), "./gradlew test"),
        )
        .unwrap();
    // Left out: only the Gradle daemon working in somebody else's project.
    // Kept: the Kotlin daemon (its directory never names a project), the
    // one nothing is known about (an unknown daemon is still worth a
    // line), the one idling in Gradle's own directory (it names no project
    // either), and the one working below this command's directory.
    assert_eq!(
        run.leftover_daemons,
        vec![
            daemon(4400, "kotlin"),
            daemon(4500, "gradle"),
            daemon(4600, "gradle"),
            daemon(4700, "gradle"),
        ]
    );
}

#[test]
fn real_working_directory_of_a_process_is_read() {
    use super::super::watchdog::{GroupControl, SystemGroup};

    let dir = TempDir::new().unwrap();
    let mut child = Command::new("sleep")
        .arg("30")
        .current_dir(dir.path())
        .spawn()
        .unwrap();
    let cwd = SystemGroup.process_cwd(child.id());
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(cwd, Some(fs::canonicalize(dir.path()).unwrap()));
    // No such process: nothing to say.
    assert_eq!(SystemGroup.process_cwd(child.id()), None);
}
