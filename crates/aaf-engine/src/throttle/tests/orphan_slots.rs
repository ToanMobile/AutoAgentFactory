//! A slot whose holder was killed while its command runs on: the lock is
//! free, the lock file still names the command's process group. The process
//! table is a closure here; the real kill is in `aaf-cli/tests/cli_gate_orphan.rs`.

use std::fs;

use tempfile::TempDir;

use super::super::paths::token_path;
use super::super::probe::GroupState;
use super::super::slots::{
    Admission, Busy, MachineView, holds_token_in, is_held, slot_path, try_acquire_in,
};
use super::super::token::SlotToken;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const STARTED: &str = "Sun Nov 22 11:00:00 2026";
const PGID: u32 = 4242;

/// What a holder killed in mid-run leaves in slot `index`.
fn killed_holder(data: &TempDir, index: usize, reserved: &str, started: Option<&str>) {
    let path = slot_path(data.path(), index);
    let token = token_path(data.path(), index);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::create_dir_all(token.parent().unwrap()).unwrap();
    let started = started.map_or(String::new(), |s| format!("lstart={s}\n"));
    fs::write(&token, format!("token={TOKEN}\n")).unwrap();
    fs::write(
        &path,
        format!("reserved={reserved}\npgid={PGID}\n{started}"),
    )
    .unwrap();
}

fn running(leader_started: Option<&str>) -> Option<GroupState> {
    Some(GroupState {
        leader_started: leader_started.map(str::to_string),
        stopped: false,
    })
}

fn acquire(
    data: &TempDir,
    ceiling: usize,
    admission: Admission,
    group: &dyn Fn(u32) -> Option<GroupState>,
) -> Result<usize, Busy> {
    let view = MachineView {
        free_memory: &|| Some(u64::MAX),
        group_rss: &|_| None,
        group_state: group,
    };
    try_acquire_in(data.path(), ceiling, admission, &view)
        .unwrap()
        .map(|guard| guard.index())
}

const SMALL: Admission = Admission::Measured { peak_rss_bytes: 1 };

#[test]
fn slot_of_a_killed_holder_stays_taken_while_its_command_runs() {
    let data = TempDir::new().unwrap();
    killed_holder(&data, 0, "unknown", Some(STARTED));
    let alive = |pgid: u32| (pgid == PGID).then(|| running(Some(STARTED))).flatten();

    for admission in [Admission::Unmeasured, SMALL] {
        assert_eq!(
            acquire(&data, 1, admission, &alive),
            Err(Busy::Orphaned { pgid: PGID }),
            "{admission:?}"
        );
    }
    // Looked at, not taken and not wiped: the next look must find it again.
    assert!(!is_held(data.path(), 0));
    let text = fs::read_to_string(slot_path(data.path(), 0)).unwrap();
    let token = fs::read_to_string(token_path(data.path(), 0)).unwrap();
    assert!(
        token.contains(TOKEN) && text.contains("pgid=4242"),
        "{token:?} {text:?}"
    );

    // The command ended: the slot is free and nothing of it is left.
    assert_eq!(acquire(&data, 1, Admission::Unmeasured, &|_| None), Ok(0));
    let text = fs::read_to_string(slot_path(data.path(), 0)).unwrap();
    assert_eq!(text, "");
    let token = fs::read_to_string(token_path(data.path(), 0)).unwrap();
    assert_eq!(token, "");
}

#[test]
fn members_that_outlive_the_group_leader_still_hold_the_slot() {
    let data = TempDir::new().unwrap();
    killed_holder(&data, 0, "unknown", Some(STARTED));
    // `sh` is gone, the build it started is not.
    let leaderless = |_: u32| running(None);
    assert_eq!(
        acquire(&data, 1, Admission::Unmeasured, &leaderless),
        Err(Busy::Orphaned { pgid: PGID })
    );
}

#[test]
fn another_process_that_got_the_same_group_id_does_not_hold_the_slot() {
    let data = TempDir::new().unwrap();
    killed_holder(&data, 0, "unknown", Some(STARTED));
    // The id was given out again: its leader started at another time.
    let reused = |_: u32| running(Some("Sun Nov 22 12:34:56 2026"));
    assert_eq!(acquire(&data, 1, Admission::Unmeasured, &reused), Ok(0));
}

// Until C3 this case was part of the test above and asserted `Ok(0)`: "with
// no recorded start nothing proves it is the command". That was fail-open.
// The start time is written one `ps` run (up to 2 s under memory pressure)
// after the group id; an `aaf` killed in between left exactly this record,
// and the next command was admitted on top of the one still running.
#[test]
fn a_holder_killed_before_the_start_time_was_recorded_still_holds_its_slot() {
    let data = TempDir::new().unwrap();
    killed_holder(&data, 0, "unknown", None);
    let alive = |pgid: u32| (pgid == PGID).then(|| running(Some(STARTED))).flatten();

    for admission in [Admission::Unmeasured, SMALL] {
        assert_eq!(
            acquire(&data, 1, admission, &alive),
            Err(Busy::Orphaned { pgid: PGID }),
            "{admission:?}"
        );
    }
    // The group ended: the slot is free again.
    assert_eq!(acquire(&data, 1, Admission::Unmeasured, &|_| None), Ok(0));
}

#[test]
fn measured_command_shares_the_machine_with_an_orphan_only_by_its_reservation() {
    let alive = |_: u32| running(Some(STARTED));
    // An orphan of unknown size: nothing starts beside it.
    let data = TempDir::new().unwrap();
    killed_holder(&data, 0, "unknown", Some(STARTED));
    assert_eq!(acquire(&data, 2, SMALL, &alive), Err(Busy::SlotsTaken));
    // An orphan with a reservation is counted like a running command.
    let data = TempDir::new().unwrap();
    killed_holder(&data, 0, "1024", Some(STARTED));
    assert_eq!(acquire(&data, 2, SMALL, &alive), Ok(1));
    // A command that runs alone never starts beside it.
    assert_eq!(
        acquire(&data, 2, Admission::Unmeasured, &alive),
        Err(Busy::Orphaned { pgid: PGID })
    );
}

#[test]
fn process_below_an_orphaned_command_is_still_inside_its_slot() {
    let data = TempDir::new().unwrap();
    killed_holder(&data, 0, "unknown", Some(STARTED));
    let token = SlotToken::parse(TOKEN).unwrap();
    let other = SlotToken::parse("ffffffffffffffffffffffffffffffff").unwrap();
    let alive = |_: u32| running(Some(STARTED));

    // It would otherwise queue behind its own ancestor, for ever.
    assert!(holds_token_in(data.path(), 0, &token, &alive));
    assert!(!holds_token_in(data.path(), 0, &other, &alive));
    // The command is gone: its token opens nothing, as before.
    assert!(!holds_token_in(data.path(), 0, &token, &|_| None));
    // Nor without a look at the process table.
    assert!(!super::super::slots::holds_token(data.path(), 0, &token));
}
