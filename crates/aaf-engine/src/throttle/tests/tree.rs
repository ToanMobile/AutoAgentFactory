//! The process tree of a heavy command. Measured on a real Unity run: the
//! command's process group held 1.29 GB while the tree (12 shader compilers,
//! licensing client, package manager, ILPP runner in groups of their own,
//! VBCSCompiler reparented to pid 1) held 3.96 GB. What is sampled and what
//! a timeout ends has to be the tree.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tempfile::TempDir;

use super::super::probe::{ProcId, parse_tree, tree_members, tree_rss};
use super::super::stats::{HeavySample, KEEP_RUNS, record, recorded_peak, samples};
use super::super::watchdog::{SIGNAL_READS, TreeSample, signal_verified};
use super::super::{GroupControl, HeavyJob, SystemGroup, Throttle};
use super::support::{GIB, Rig};

/// `ps -A -o pid=,ppid=,pgid=,rss=,lstart=` in the shape macOS prints it.
/// 100 is the command (`sh -c`), 101 its child in the same group; 200 and
/// 201 are a helper with a group of its own and that helper's child; 300 is
/// a compiler server still below 101; 900 is somebody else's process.
const FIRST: &str = "\
  100     1   100   1000 Sun Nov 22 14:00:00 2026
  101   100   100   2000 Sun Nov 22 14:00:01 2026
  200   101   200  50000 Sun Nov 22 14:00:02 2026
  201   200   200  60000 Sun Nov 22 14:00:03 2026
  300   101   300  70000 Sun Nov 22 14:00:04 2026
  900     1   900 999999 Sun Nov 22 13:00:00 2026
";
/// A moment later: 101 has exited, so 200 and 300 now belong to pid 1, and
/// 300 has started a child.
const LATER: &str = "\
  100     1   100   1000 Sun Nov 22 14:00:00 2026
  200     1   200  50000 Sun Nov 22 14:00:02 2026
  201   200   200  60000 Sun Nov 22 14:00:03 2026
  300     1   300  70000 Sun Nov 22 14:00:04 2026
  301   300   300   5000 Sun Nov 22 14:00:09 2026
  900     1   900 999999 Sun Nov 22 13:00:00 2026
";
/// After the command: its group is gone, the reparented helpers live on,
/// and pid 300 now names an unrelated process.
const AFTER: &str = "\
  200     1   200  50000 Sun Nov 22 14:00:02 2026
  201   200   200  60000 Sun Nov 22 14:00:03 2026
  300     1   300  80000 Sun Nov 22 15:30:00 2026
  900     1   900 999999 Sun Nov 22 13:00:00 2026
";

fn pids(members: &[ProcId]) -> Vec<u32> {
    let mut pids: Vec<u32> = members.iter().map(|m| m.pid).collect();
    pids.sort_unstable();
    pids
}

#[test]
fn the_tree_is_the_group_and_everything_below_it_in_any_group() {
    let rows = parse_tree(FIRST);
    let members = tree_members(&rows, 100, &[]);
    assert_eq!(pids(&members), [100, 101, 200, 201, 300]);
    assert_eq!(
        tree_rss(&rows, &members),
        Some((1000 + 2000 + 50000 + 60000 + 70000) * 1024)
    );
}

#[test]
fn a_helper_reparented_to_pid_1_stays_in_the_tree_with_its_children() {
    let known = tree_members(&parse_tree(FIRST), 100, &[]);
    let rows = parse_tree(LATER);

    // Without the earlier sample only the group itself is found.
    assert_eq!(pids(&tree_members(&rows, 100, &[])), [100]);
    let members = tree_members(&rows, 100, &known);
    assert_eq!(pids(&members), [100, 200, 201, 300, 301]);
    assert_eq!(
        tree_rss(&rows, &members),
        Some((1000 + 50000 + 60000 + 70000 + 5000) * 1024)
    );
}

#[test]
fn a_pid_given_to_another_process_is_not_in_the_tree_any_more() {
    let first = tree_members(&parse_tree(FIRST), 100, &[]);
    let known = tree_members(&parse_tree(LATER), 100, &first);
    let rows = parse_tree(AFTER);

    let members = tree_members(&rows, 100, &known);

    // 300 has another start time now: it is not the helper that was seen.
    assert_eq!(pids(&members), [200, 201]);
    assert_eq!(tree_rss(&rows, &members), Some((50000 + 60000) * 1024));
    assert_eq!(tree_rss(&rows, &[]), None);
}

#[test]
fn pid_1_never_pulls_the_rest_of_the_machine_into_the_tree() {
    // A command whose group id is 1 cannot exist; a listing that says so
    // must still not make every orphan on the machine a member.
    let rows = parse_tree(FIRST);
    assert_eq!(pids(&tree_members(&rows, 1, &[])), Vec::<u32>::new());
    assert_eq!(parse_tree("garbage\n1 2\n").len(), 0);
}

fn job<'a>(cwd: &'a Path, command: &'a str, timeout: Duration) -> HeavyJob<'a> {
    HeavyJob {
        repo_id: "repo",
        label: "test_full",
        command,
        cwd,
        timeout,
        log_tail_bytes: 4096,
    }
}

fn member(pid: u32) -> ProcId {
    ProcId {
        pid,
        started: "Sun Nov 22 14:00:00 2026".to_string(),
    }
}

#[test]
fn the_learned_peak_is_the_trees_not_the_groups() {
    let data = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let rig = Rig::calm();
    *rig.group.tree.lock().unwrap() = Some(TreeSample {
        rss_bytes: 4 * GIB,
        members: vec![member(100), member(200)],
    });
    rig.group.rss.lock().unwrap().push_back(Some(GIB));

    let run = rig
        .throttle()
        .run_heavy(
            data.path(),
            &*rig.clock,
            &job(work.path(), "sleep 0.2", Duration::from_secs(120)),
        )
        .unwrap();

    assert_eq!(run.exit_code, Some(0), "{run:?}");
    assert_eq!(run.peak_rss_bytes, Some(4 * GIB));
    assert_eq!(
        recorded_peak(data.path(), "repo", "test_full"),
        Some(4 * GIB)
    );
    // A command that ended by itself: nothing of its tree is signalled.
    assert!(rig.group.signalled.lock().unwrap().is_empty());
}

#[test]
fn a_tree_that_cannot_be_read_falls_back_to_the_group() {
    let data = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let rig = Rig::calm();
    rig.group.rss.lock().unwrap().push_back(Some(GIB));

    let run = rig
        .throttle()
        .run_heavy(
            data.path(),
            &*rig.clock,
            &job(work.path(), "sleep 0.2", Duration::from_secs(120)),
        )
        .unwrap();

    assert_eq!(run.peak_rss_bytes, Some(GIB));
}

/// A command that ends between the check that it still runs and the `ps` of
/// the sample is listed exited but not yet reaped, with 0 bytes resident.
/// That reading is no measurement: it neither becomes the command's figure
/// (admitted beside others as if it took nothing) nor pushes the real
/// figures of earlier runs out of the remembered ones.
#[test]
fn a_reading_of_exited_processes_is_no_sample() {
    let data = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let rig = Rig::calm();
    let measured = HeavySample {
        peak_rss_bytes: 3 * GIB,
        duration_ms: 60_000,
    };
    assert!(record(data.path(), "repo", "test_full", measured, &rig.sleeper).unwrap());
    *rig.group.tree.lock().unwrap() = Some(TreeSample {
        rss_bytes: 0,
        members: vec![member(100)],
    });

    for _ in 0..KEEP_RUNS {
        let run = rig
            .throttle()
            .run_heavy(
                data.path(),
                &*rig.clock,
                // Long enough to be sampled: every sample reads the zombie.
                &job(work.path(), "sleep 0.2", Duration::from_secs(120)),
            )
            .unwrap();
        assert_eq!(run.exit_code, Some(0), "{run:?}");
        assert_eq!(run.peak_rss_bytes, None, "{run:?}");
    }

    assert_eq!(samples(data.path(), "repo", "test_full"), [measured]);
    assert_eq!(
        recorded_peak(data.path(), "repo", "test_full"),
        Some(3 * GIB)
    );
}

#[test]
fn a_timeout_ends_the_sampled_tree_with_term_and_then_kill() {
    let data = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let rig = Rig::calm();
    let members = vec![member(100), member(200), member(300)];
    *rig.group.tree.lock().unwrap() = Some(TreeSample {
        rss_bytes: GIB,
        members: members.clone(),
    });

    let run = rig
        .throttle()
        .run_heavy(
            data.path(),
            &*rig.clock,
            &job(work.path(), "sleep 300", Duration::from_millis(300)),
        )
        .unwrap();

    assert!(run.timed_out);
    let signalled = rig.group.signalled.lock().unwrap().clone();
    assert_eq!(
        signalled,
        [
            ("-TERM".to_string(), members.clone()),
            ("-KILL".to_string(), members)
        ]
    );
    // The grace between the two is waited through the sleeper.
    assert_eq!(*rig.sleeper.slept.lock().unwrap(), [Duration::from_secs(2)]);
}

fn alive(pid: &str) -> bool {
    Command::new("ps")
        .args(["-o", "pid=", "-p", pid])
        .output()
        .is_ok_and(|out| !String::from_utf8_lossy(&out.stdout).trim().is_empty())
}

/// Real processes: with job control on, the shell puts its background job
/// into a process group of its own, the way Unity's helpers are. The
/// group kill of the timeout does not reach it.
#[test]
fn a_real_timeout_also_ends_a_helper_in_another_process_group() {
    let data = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let mut rig = Rig::calm();
    rig.real_group = true;
    let command = "set -m; sleep 300 & echo $! > helper.pid; wait";

    let run = rig
        .throttle()
        .run_heavy(
            data.path(),
            &*rig.clock,
            &job(work.path(), command, Duration::from_millis(1500)),
        )
        .unwrap();

    assert!(run.timed_out, "{run:?}");
    let helper = fs::read_to_string(work.path().join("helper.pid")).unwrap();
    let helper = helper.trim();
    let deadline = Instant::now() + Duration::from_secs(20);
    while alive(helper) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let survived = alive(helper);
    if survived {
        // Never leave the stand-in behind, whatever the assertion says.
        let _ = Command::new("kill").args(["-KILL", helper]).status();
    }
    assert!(!survived, "helper {helper} outlived the timeout");
}

/// Every `kill` of [`signal_verified`]: the signal and the pids.
type Sent = Mutex<Vec<(String, Vec<String>)>>;

fn sender(sent: &Sent) -> impl Fn(&str, &[String]) + '_ {
    |signal, pids| {
        sent.lock()
            .unwrap()
            .push((signal.to_string(), pids.to_vec()))
    }
}

/// `ps` that never answers (seen under load: one `ps` past its two
/// seconds while dozens of others ran) must not leave the tree running: the
/// members as the last sample saw them are signalled by pid.
#[test]
fn a_signal_without_an_answer_from_ps_still_reaches_the_members_last_seen() {
    let members = vec![member(200), member(201)];
    let sent = Sent::default();
    let reads = AtomicUsize::new(0);
    let none = || {
        reads.fetch_add(1, Ordering::SeqCst);
        None
    };

    assert_eq!(signal_verified(&none, &sender(&sent), "-TERM", &members), 2);

    assert_eq!(
        *sent.lock().unwrap(),
        [(
            "-TERM".to_string(),
            vec!["200".to_string(), "201".to_string()]
        )]
    );
    // Asked again before falling back, a bounded number of times.
    assert_eq!(reads.load(Ordering::SeqCst), SIGNAL_READS);
}

/// A later answer is used as before: a pid now held by another process (a
/// different start time) is spared, and so is pid 1.
#[test]
fn a_signal_asks_ps_again_and_still_spares_a_pid_that_names_another_process() {
    let started = |pid: u32, at: &str| ProcId {
        pid,
        started: at.to_string(),
    };
    let members = vec![
        started(200, "Sun Nov 22 14:00:02 2026"),
        started(300, "Sun Nov 22 14:00:04 2026"),
        started(1, "Sun Nov 22 13:00:00 2026"),
    ];
    let sent = Sent::default();
    let reads = AtomicUsize::new(0);
    let second = || (reads.fetch_add(1, Ordering::SeqCst) > 0).then(|| parse_tree(AFTER));

    assert_eq!(
        signal_verified(&second, &sender(&sent), "-KILL", &members),
        1
    );

    assert_eq!(
        *sent.lock().unwrap(),
        [("-KILL".to_string(), vec!["200".to_string()])]
    );
}

/// The real group, except that `ps` gives no answer until `until`: a
/// loaded machine on which every sample of the run went unanswered.
struct UnansweredUntil {
    until: Instant,
}

impl UnansweredUntil {
    fn answers(&self) -> bool {
        Instant::now() >= self.until
    }
}

impl GroupControl for UnansweredUntil {
    fn rss_bytes(&self, pgid: u32) -> Option<u64> {
        self.answers()
            .then(|| SystemGroup.rss_bytes(pgid))
            .flatten()
    }

    fn tree(&self, pgid: u32, known: &[ProcId]) -> Option<TreeSample> {
        self.answers()
            .then(|| SystemGroup.tree(pgid, known))
            .flatten()
    }

    fn stop(&self, pgid: u32) -> bool {
        SystemGroup.stop(pgid)
    }

    fn resume(&self, pgid: u32) -> bool {
        SystemGroup.resume(pgid)
    }

    fn signal_members(&self, signal: &str, members: &[ProcId]) -> usize {
        SystemGroup.signal_members(signal, members)
    }
}

/// No sample saw the helper while the command ran. The tree is read once
/// more when the timeout has passed, before the group is killed: the last
/// moment the helper is still below the command. Without that read the
/// helper was never known and outlived the timeout.
#[test]
fn a_helper_no_sample_saw_is_still_ended_by_the_timeout() {
    let data = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let mut rig = Rig::calm();
    // One sample, at the start; the next would be long after the timeout.
    rig.sample_every = Duration::from_secs(600);
    let timeout = Duration::from_millis(1500);
    let pid_file = work.path().join("helper.pid");
    let command = "set -m; sleep 300 & echo $! > helper.pid; wait";
    // Taken before the run: the kill comes at least `timeout` after it.
    let control = UnansweredUntil {
        until: Instant::now() + timeout,
    };

    let run = Throttle {
        control: &control,
        ..rig.throttle()
    }
    .run_heavy(
        data.path(),
        &*rig.clock,
        &job(work.path(), command, timeout),
    )
    .unwrap();

    let helper = fs::read_to_string(&pid_file).unwrap_or_default();
    let helper = helper.trim().to_string();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !helper.is_empty() && alive(&helper) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let survived = !helper.is_empty() && alive(&helper);
    if survived {
        // Never leave the stand-in behind, whatever the assertion says.
        let _ = Command::new("kill").args(["-KILL", &helper]).status();
    }
    assert!(run.timed_out, "{run:?}");
    assert!(!helper.is_empty(), "the command never started its helper");
    assert!(!survived, "helper {helper} outlived the timeout");
}
