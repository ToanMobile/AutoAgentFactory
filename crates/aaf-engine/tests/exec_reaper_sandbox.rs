#![allow(clippy::unwrap_used, clippy::expect_used)]
//! A process an agent run leaves behind with no trace `ps` can read (started
//! through `env -i`, its own process group, re-parented to pid 1) is still in
//! the run's Seatbelt sandbox. The reaper asks the kernel which sandbox each
//! process is in (owner decision, SECURITY.md §10) and kills the ones in this
//! run's sandbox only: another run's sandbox and unsandboxed processes stay.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use aaf_domain::ports::SandboxPolicy;
use aaf_engine::executors::reaper::{RunMarks, is_alive, reap_in};
use aaf_engine::executors::sandbox::{SandboxPaths, seatbelt_profile};

/// Kills the stand-in if an assertion fails first.
struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn marks(dir: &Path, tag: &str) -> RunMarks {
    let marks = RunMarks::at(dir, &format!("sbx-{tag}-{}", std::process::id())).unwrap();
    marks.create().unwrap();
    marks
}

/// `env -i /bin/sleep 600` inside a profile carrying `marks`.
fn sandboxed_sleep(marks: &RunMarks) -> Guard {
    let extra = SandboxPaths {
        run_mark: Some(marks.mark.clone()),
        ..SandboxPaths::default()
    };
    let profile = seatbelt_profile(&SandboxPolicy::default(), &extra).unwrap();
    let child = Command::new("/usr/bin/sandbox-exec")
        .args(["-p", &profile, "/usr/bin/env", "-i", "/bin/sleep", "600"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    Guard(child)
}

fn plain_sleep() -> Guard {
    let child = Command::new("/usr/bin/env")
        .args(["-i", "/bin/sleep", "600"])
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    Guard(child)
}

fn ends_within(pid: u32, limit: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if !is_alive(pid).unwrap() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn run_id(marks: &RunMarks) -> String {
    Path::new(&marks.mark)
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

#[test]
fn the_reaper_kills_what_is_left_in_this_runs_sandbox_and_nothing_else() {
    if !Path::new("/usr/bin/sandbox-exec").exists() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path().canonicalize().unwrap();
    let mine = marks(&dir, "mine");
    let other = marks(&dir, "other");
    let left = sandboxed_sleep(&mine);
    let neighbour = sandboxed_sleep(&other);
    let outside = plain_sleep();
    // sandbox-exec execs the profile's command: give it time to apply.
    std::thread::sleep(Duration::from_millis(500));
    for guard in [&left, &neighbour, &outside] {
        assert!(is_alive(guard.0.id()).unwrap(), "stand-in did not start");
    }

    let killed = reap_in(&run_id(&mine), Some(&mine)).unwrap();

    assert!(killed.contains(&left.0.id()), "killed {killed:?}");
    assert!(ends_within(left.0.id(), Duration::from_secs(5)));
    assert!(
        is_alive(neighbour.0.id()).unwrap(),
        "another run's sandbox was reaped"
    );
    assert!(
        is_alive(outside.0.id()).unwrap(),
        "an unsandboxed process was reaped"
    );
    mine.remove().unwrap();
    other.remove().unwrap();
    assert!(!Path::new(&mine.mark).exists() && !Path::new(&mine.control).exists());
}

#[test]
fn the_profile_lets_the_run_write_its_mark_and_nothing_else_about_it() {
    let extra = SandboxPaths {
        run_mark: Some("/data/marks/run-1.mark".to_string()),
        ..SandboxPaths::default()
    };
    let profile = seatbelt_profile(&SandboxPolicy::default(), &extra).unwrap();
    // The last rule, data only: the mark can be neither removed nor replaced.
    assert!(
        profile.ends_with("(allow file-write-data (literal \"/data/marks/run-1.mark\"))\n"),
        "{profile}"
    );
    for bad in ["relative.mark", "/a\"b.mark"] {
        let extra = SandboxPaths {
            run_mark: Some(bad.to_string()),
            ..SandboxPaths::default()
        };
        assert!(
            seatbelt_profile(&SandboxPolicy::default(), &extra).is_err(),
            "{bad:?}"
        );
    }
    assert!(RunMarks::at(Path::new("/data"), "../x").is_err());
}
