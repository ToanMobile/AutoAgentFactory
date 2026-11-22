#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Reaper tests against real orphaned processes.

use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use aaf_domain::ports::PortError;
use aaf_engine::executors::reaper::{
    ProcessTracker, Signal, is_alive, kill_group, reap, select_victims, validate_run_id,
};

const HELPER_FLAG: &str = "AAF_EXEC_TEST_HELPER";

/// Not a test: the long-running process the orphan tests start by re-running
/// this test binary. `ps` hides the environment of Apple's own binaries
/// (`/bin/sleep`), so the orphan has to be a binary built here.
#[test]
#[ignore = "helper process for the orphan tests"]
fn helper_block() {
    if std::env::var(HELPER_FLAG).as_deref() != Ok("block") {
        return;
    }
    println!("ready");
    // Bounded so a failed test cannot leave it behind for long.
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        std::thread::park_timeout(Duration::from_secs(120));
    }
}

struct Orphan {
    pid: u32,
    /// Read end of a pipe the orphan holds open: EOF means it is dead.
    pipe: BufReader<std::process::ChildStdout>,
}

impl Orphan {
    fn wait_dead(mut self) {
        let mut rest = Vec::new();
        self.pipe.read_to_end(&mut rest).unwrap();
        assert_gone(self.pid);
    }
}

/// A shell backgrounds the helper and exits, leaving it re-parented to pid 1.
fn spawn_orphan(run_id: &str) -> Orphan {
    let exe = std::env::current_exe().unwrap();
    let mut shell = Command::new("/bin/sh")
        .args([
            "-c",
            "\"$0\" helper_block --exact --ignored --nocapture & echo pid=$!",
        ])
        .arg(exe)
        .env("AAF_RUN_ID", run_id)
        .env(HELPER_FLAG, "block")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut pipe = BufReader::new(shell.stdout.take().unwrap());
    let (mut pid, mut ready) = (None, false);
    while pid.is_none() || !ready {
        let mut line = String::new();
        assert!(
            pipe.read_line(&mut line).unwrap() > 0,
            "helper never started"
        );
        if let Some(value) = line.trim().strip_prefix("pid=") {
            pid = Some(value.parse().unwrap());
        }
        ready |= line.trim() == "ready";
    }
    assert!(shell.wait().unwrap().success());
    Orphan {
        pid: pid.unwrap(),
        pipe,
    }
}

fn assert_gone(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while is_alive(pid).unwrap() {
        assert!(Instant::now() < deadline, "pid {pid} is still alive");
        std::thread::yield_now();
    }
}

fn run_id() -> String {
    format!("test-{}", uuid::Uuid::new_v4())
}

#[test]
fn reap_kills_a_real_orphan_of_the_run_and_spares_other_runs() {
    let (mine, other) = (run_id(), run_id());
    let orphan = spawn_orphan(&mine);
    let bystander = spawn_orphan(&other);
    assert!(is_alive(orphan.pid).unwrap());

    let killed = reap(&mine).unwrap();
    assert_eq!(killed, vec![orphan.pid]);
    orphan.wait_dead();
    assert!(is_alive(bystander.pid).unwrap(), "another run was reaped");

    assert_eq!(reap(&other).unwrap(), vec![bystander.pid]);
    bystander.wait_dead();
    assert_eq!(reap(&mine).unwrap(), Vec::<u32>::new());
}

#[test]
fn tracker_kills_a_descendant_that_left_group_and_parent() {
    // perl forks a child that calls setsid and execs sleep, then waits for
    // stdin to close. The child is in another session and group, and `ps`
    // shows no environment for /bin/sleep: only the tracker knows it.
    let mut root = Command::new("/usr/bin/perl")
        .args([
            "-MPOSIX",
            "-e",
            "$p = fork; if (!$p) { setsid; exec '/bin/sleep', '300' } $| = 1; print \"child=$p\\n\"; <STDIN>;",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(root.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let child: u32 = line.trim().strip_prefix("child=").unwrap().parse().unwrap();

    let mut tracker = ProcessTracker::new(root.id());
    tracker.observe().unwrap();
    assert!(tracker.tracked().contains(&root.id()));
    assert!(tracker.tracked().contains(&child));

    // Closing stdin ends the parent; the child is now an orphan under pid 1.
    drop(root.stdin.take());
    root.wait().unwrap();
    tracker.observe().unwrap();
    assert!(is_alive(child).unwrap());

    assert_eq!(tracker.kill_survivors().unwrap(), vec![child]);
    assert_gone(child);
}

#[test]
fn tracker_does_not_adopt_unrelated_processes() {
    let mut unrelated = Command::new("/bin/sleep").arg("300").spawn().unwrap();
    let mut root = Command::new("/bin/sleep").arg("300").spawn().unwrap();
    let mut tracker = ProcessTracker::new(root.id());
    tracker.observe().unwrap();
    assert_eq!(tracker.tracked(), vec![root.id()]);
    assert_eq!(tracker.kill_survivors().unwrap(), vec![root.id()]);
    root.wait().unwrap();
    assert!(is_alive(unrelated.id()).unwrap());
    unrelated.kill().unwrap();
    unrelated.wait().unwrap();
}

#[test]
fn kill_group_refuses_dangerous_targets() {
    // `kill -- -1` would signal every process of the user.
    for pgid in [0, 1] {
        assert!(matches!(
            kill_group(pgid, Signal::Term),
            Err(PortError::Invalid(_))
        ));
    }
}

#[test]
fn run_id_alphabet_is_enforced() {
    for ok in ["a", "run-1_B", &run_id()] {
        validate_run_id(ok).unwrap();
    }
    for bad in ["", "a b", "a=b", "a\nb", "a.*", "$(id)", &"x".repeat(129)] {
        assert!(validate_run_id(bad).is_err(), "{bad:?} accepted");
        assert!(reap(bad).is_err(), "{bad:?} reaped");
    }
}

const LISTING: &str = "\
    1     0 /sbin/launchd
  100     1 /bin/zsh -l TERM=xterm
  200   100 /usr/local/bin/aaf run LANG=C
  300   200 /opt/agent --flag LANG=C AAF_RUN_ID=abc TERM=xterm
  301   300 /bin/sh -c build
  302   301 /bin/sleep 5
  400     1 /opt/orphan AAF_RUN_ID=abc
  500     1 /opt/other AAF_RUN_ID=abcd
  501     1 /opt/other XAAF_RUN_ID=abc
  502     1 /opt/other AAF_RUN_ID=abc/x
  600     1 /bin/sleep 9
garbage line without numbers
";

#[test]
fn select_victims_matches_whole_token_and_adds_descendants() {
    assert_eq!(
        select_victims(LISTING, "abc", 200),
        vec![300, 301, 302, 400]
    );
    assert_eq!(select_victims(LISTING, "abcd", 200), vec![500]);
    assert_eq!(select_victims(LISTING, "nope", 200), Vec::<u32>::new());
}

#[test]
fn select_victims_never_picks_self_or_its_ancestors() {
    // Seen from inside the agent's own tree (aaf nested in a run): the caller
    // and everything above it survive.
    let listing = "\
  1 0 /sbin/launchd
 10 1 /opt/agent AAF_RUN_ID=abc
 20 10 /usr/local/bin/aaf AAF_RUN_ID=abc
 30 20 /bin/ps AAF_RUN_ID=abc
 40 10 /opt/sibling
";
    assert_eq!(select_victims(listing, "abc", 20), vec![30, 40]);
}
