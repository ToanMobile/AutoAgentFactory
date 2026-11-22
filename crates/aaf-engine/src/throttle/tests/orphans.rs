//! A group stopped by an `aaf` that died is continued by the next heavy run.

use std::collections::HashMap;
use std::fs;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tempfile::TempDir;

use super::super::orphans::{record_path, record_pause, resume_orphans};
use super::super::probe::{GroupState, Unreadable, group_state};
use super::super::{GroupControl, HeavyJob, Sleeper, ThreadSleeper};
use super::support::{Rig, answered};

const STARTED: &str = "Sun Nov 22 09:12:08 2026";
const LATER: &str = "Sun Nov 22 09:40:00 2026";
/// No group was continued.
const NONE: Vec<u32> = Vec::new();

fn stopped(leader_started: Option<&str>) -> GroupState {
    GroupState {
        leader_started: leader_started.map(str::to_string),
        stopped: true,
    }
}

/// Knows the groups it is told about and records which were continued.
#[derive(Default)]
struct Seen {
    groups: HashMap<u32, GroupState>,
    resumed: Mutex<Vec<u32>>,
    /// Reads of the process table still to fail, the way a `ps` that ran
    /// over its time does.
    unreadable: Mutex<u32>,
}

impl Seen {
    fn with(groups: &[(u32, GroupState)]) -> Seen {
        Seen {
            groups: groups.iter().cloned().collect(),
            ..Seen::default()
        }
    }

    fn resumed(&self) -> Vec<u32> {
        self.resumed.lock().unwrap().clone()
    }
}

impl GroupControl for Seen {
    fn rss_bytes(&self, _pgid: u32) -> Option<u64> {
        None
    }

    fn stop(&self, _pgid: u32) -> bool {
        true
    }

    fn resume(&self, pgid: u32) -> bool {
        self.resumed.lock().unwrap().push(pgid);
        true
    }

    fn group_state(&self, pgid: u32) -> Option<GroupState> {
        self.read_group_state(pgid).ok().flatten()
    }

    fn read_group_state(&self, pgid: u32) -> Result<Option<GroupState>, Unreadable> {
        let mut unreadable = self.unreadable.lock().unwrap();
        if *unreadable > 0 {
            *unreadable -= 1;
            return Err(Unreadable);
        }
        Ok(self.groups.get(&pgid).cloned())
    }
}

/// A process table that gave no answer decides nothing: the record stays
/// for the next heavy run, which continues the group once `ps` answers.
/// Before, the record was removed and the group stayed stopped for good.
#[test]
fn a_sweep_that_cannot_read_the_process_table_keeps_the_record() {
    let data = TempDir::new().unwrap();
    let control = Seen::with(&[(77, stopped(Some(STARTED)))]);
    *control.unreadable.lock().unwrap() = 1;
    record_pause(data.path(), 77, Some(STARTED))
        .unwrap()
        .abandon();

    assert_eq!(resume_orphans(data.path(), &control), 0);
    assert_eq!(control.resumed(), NONE);
    assert!(record_path(data.path(), 77).exists());

    assert_eq!(resume_orphans(data.path(), &control), 1);
    assert_eq!(control.resumed(), vec![77]);
    assert!(!record_path(data.path(), 77).exists());
}

#[test]
fn record_of_a_dead_owner_is_swept_and_a_live_one_is_left_alone() {
    let data = TempDir::new().unwrap();
    let control = Seen::with(&[(77, stopped(Some(STARTED))), (4242, stopped(Some(STARTED)))]);
    // Not even a locks directory yet.
    assert_eq!(resume_orphans(data.path(), &control), 0);

    let live = record_pause(data.path(), 4242, Some(STARTED)).unwrap();
    let dead = record_pause(data.path(), 77, Some(STARTED)).unwrap();
    for junk in ["paused.junk", "paused.0", "paused.1", "paused.9.tmp"] {
        fs::write(data.path().join("locks").join(junk), "").unwrap();
    }
    dead.abandon();
    assert!(record_path(data.path(), 77).exists());

    assert_eq!(resume_orphans(data.path(), &control), 1);
    assert_eq!(control.resumed(), vec![77]);
    assert!(!record_path(data.path(), 77).exists());
    assert!(record_path(data.path(), 4242).exists());

    live.clear();
    assert!(!record_path(data.path(), 4242).exists());
    assert_eq!(resume_orphans(data.path(), &control), 0);
    assert_eq!(control.resumed().len(), 1);
}

/// One abandoned record for group 77, swept against `now`; returns what was
/// continued.
fn sweep(recorded: Option<&str>, now: Option<GroupState>) -> Vec<u32> {
    let data = TempDir::new().unwrap();
    let control = match now {
        Some(state) => Seen::with(&[(77, state)]),
        None => Seen::default(),
    };
    record_pause(data.path(), 77, recorded).unwrap().abandon();
    assert_eq!(resume_orphans(data.path(), &control), 1);
    // Whatever was decided, the record is gone: nothing is retried for ever.
    assert!(!record_path(data.path(), 77).exists());
    control.resumed()
}

#[test]
fn group_id_reused_by_another_process_is_not_signalled() {
    // Same id, a leader that started later: not the group that was stopped.
    assert_eq!(sweep(Some(STARTED), Some(stopped(Some(LATER)))), NONE);
    // A record without a start time vouches for no leader.
    assert_eq!(sweep(None, Some(stopped(Some(STARTED)))), NONE);
    // The control case: the same leader, still stopped.
    assert_eq!(sweep(Some(STARTED), Some(stopped(Some(STARTED)))), vec![77]);
}

#[test]
fn group_that_is_not_stopped_or_is_gone_is_not_signalled() {
    let running = GroupState {
        leader_started: Some(STARTED.to_string()),
        stopped: false,
    };
    assert_eq!(sweep(Some(STARTED), Some(running)), NONE);
    assert_eq!(sweep(Some(STARTED), None), NONE);
}

#[test]
fn stopped_members_that_outlived_their_leader_are_continued() {
    // No process has the group's id as pid any more; the id cannot have
    // been given away while members carry it.
    assert_eq!(sweep(Some(STARTED), Some(stopped(None))), vec![77]);
}

#[test]
fn group_state_is_read_from_the_ps_listing() {
    let listing = "\
  501   501 S    Sun Nov 22 09:00:00 2026
 9000  9000 T    Sun Nov  1 09:12:08 2026
 9001  9000 S    Sun Nov 22 09:12:09 2026
 9100  9100 Ss   Sun Nov 22 09:13:00 2026
 9201  9200 T+   Sun Nov 22 09:14:00 2026
garbage
";
    assert_eq!(
        group_state(listing, 9000),
        Some(stopped(Some("Sun Nov 1 09:12:08 2026")))
    );
    assert_eq!(
        group_state(listing, 9100),
        Some(GroupState {
            leader_started: Some("Sun Nov 22 09:13:00 2026".to_string()),
            stopped: false,
        })
    );
    // Only a member is left of group 9200.
    assert_eq!(group_state(listing, 9200), Some(stopped(None)));
    assert_eq!(group_state(listing, 4), None);
    assert_eq!(group_state("", 9000), None);
}

fn state(pid: u32) -> String {
    let out = Command::new("ps")
        .args(["-o", "state=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Polls `ps` until the process is (not) stopped; false after ten seconds.
fn becomes_stopped(pid: u32, stopped: bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if state(pid).starts_with('T') == stopped {
            return true;
        }
        // Between two looks, so the loop does not spin on `ps`.
        ThreadSleeper.sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn group_left_stopped_by_a_dead_aaf_is_really_continued_by_the_next_heavy_run() {
    let data = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let mut rig = Rig::calm();
    rig.real_group = true;
    rig.machine.low_priority = false;
    let throttle = rig.throttle();
    let mut orphan = Command::new("sleep")
        .arg("60")
        .process_group(0)
        .spawn()
        .unwrap();
    let pgid = orphan.id();

    let read = || throttle.control.read_group_state(pgid).ok();
    // What a killed aaf leaves behind: the record unlocked, the group stopped.
    let before = answered("before the stop", read);
    let started = before.clone().and_then(|state| state.leader_started);
    record_pause(data.path(), pgid, started.as_deref())
        .unwrap()
        .abandon();
    let stopped = throttle.control.stop(pgid) && becomes_stopped(pgid, true);
    let while_stopped = answered("while stopped", read);
    let job = HeavyJob {
        repo_id: "repo",
        label: "test_full",
        command: "true",
        cwd: work.path(),
        timeout: Duration::from_secs(120),
        log_tail_bytes: 64,
    };
    // A heavy run whose `ps` got no answer leaves the record to the next
    // one (`a_sweep_that_cannot_read_the_process_table_keeps_the_record`):
    // heavy runs until one has swept it, at most ten.
    let mut runs = Vec::new();
    while record_path(data.path(), pgid).exists() && runs.len() < 10 {
        runs.push(throttle.run_heavy(data.path(), &*rig.clock, &job));
    }
    let continued = becomes_stopped(pgid, false);
    // Ended before any assertion, so a failure never leaves it behind.
    orphan.kill().unwrap();
    orphan.wait().unwrap();

    assert!(stopped, "the group was never stopped");
    // The real `ps` gives the leader a start time and sees the stop.
    let before = before.expect("ps did not list the group");
    assert!(
        before.leader_started.is_some() && !before.stopped,
        "{before:?}"
    );
    assert_eq!(
        while_stopped,
        Some(GroupState {
            stopped: true,
            ..before
        })
    );
    for run in runs {
        assert_eq!(run.unwrap().exit_code, Some(0));
    }
    assert!(continued, "the group is still stopped");
    assert!(!record_path(data.path(), pgid).exists());
}

#[test]
fn real_group_with_another_start_time_is_left_stopped() {
    let data = TempDir::new().unwrap();
    let mut rig = Rig::calm();
    rig.real_group = true;
    let throttle = rig.throttle();
    let mut other = Command::new("sleep")
        .arg("60")
        .process_group(0)
        .spawn()
        .unwrap();
    let pgid = other.id();

    // A record for this id from a group that started at another time.
    record_pause(data.path(), pgid, Some("Thu Jan  1 00:00:00 1970"))
        .unwrap()
        .abandon();
    let stopped = throttle.control.stop(pgid) && becomes_stopped(pgid, true);
    // Sweeps until one got an answer from `ps`, at most ten: one without
    // leaves the record and sweeps nothing.
    let mut swept = 0;
    for _ in 0..10 {
        swept = resume_orphans(data.path(), throttle.control);
        if swept > 0 {
            break;
        }
    }
    // Read from the same listing the sweep uses, after it is done: a
    // SIGCONT would show here at once.
    let after = answered("after the sweep", || {
        throttle.control.read_group_state(pgid).ok()
    });
    other.kill().unwrap();
    other.wait().unwrap();

    assert!(stopped, "the group was never stopped");
    assert_eq!(swept, 1);
    assert!(after.is_some_and(|state| state.stopped), "it was continued");
    assert!(!record_path(data.path(), pgid).exists());
}
