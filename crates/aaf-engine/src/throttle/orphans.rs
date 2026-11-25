//! Process groups this tool stopped and then lost. If `aaf` is killed while
//! its heavy command is SIGSTOPped, nobody is left to continue the command,
//! and without `unsafe` there is no signal handler to do it on the way out.
//! So every stop leaves a record that stays locked for as long as its owner
//! lives, and the next heavy run continues the groups whose owner is gone.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use super::paths::{closed_dir, ensure_closed_dir, guest_dir, is_guest, open_guest_file};
use super::probe::{GroupState, ProcId, Unreadable};
use super::slot_file::unlock_all;
use super::watchdog::GroupControl;
use crate::db::private;

const PREFIX: &str = "paused.";
/// Cap on reading a pause record (matches `paths::GUEST_FILE_MAX`) so a
/// malicious guest cannot force unbounded memory allocation.
const MAX_RECORD_BYTES: u64 = 1 << 20;

/// The records this process writes and sweeps: in the closed part of the
/// data dir, and for a guest ([`super::paths`]) where it can write. The two
/// never mix: a record an agent makes up is read by `aaf slot` runs of
/// agents only, and gets a group continued that an agent could continue
/// with a signal of its own.
fn records_dir(data_dir: &Path) -> PathBuf {
    if is_guest(data_dir) {
        guest_dir(data_dir)
    } else {
        closed_dir(data_dir)
    }
}

pub fn record_path(data_dir: &Path, pgid: u32) -> PathBuf {
    records_dir(data_dir).join(format!("{PREFIX}{pgid}"))
}

/// Says that this process stopped a group. The lock on the file is the sign
/// of life: the kernel drops it when the process dies, and the file stays.
#[derive(Debug)]
pub(crate) struct PauseRecord {
    path: PathBuf,
    _locked: File,
}

pub(super) const STARTED_KEY: &str = "lstart=";
pub const MEMBER_KEY: &str = "member=";
pub const MAX_RECORDED_MEMBERS: usize = 256;

/// `leader_started` is the start time of the group's leader as `ps` prints
/// it. The sweep compares it before it signals: by then the group may be
/// gone and its id given to an unrelated process.
#[cfg(test)]
pub(crate) fn record_pause(
    data_dir: &Path,
    pgid: u32,
    leader_started: Option<&str>,
) -> io::Result<PauseRecord> {
    record_pause_with_members(data_dir, pgid, leader_started, &[])
}

/// Writes a pause record with `leader_started` and up to
/// [`MAX_RECORDED_MEMBERS`] tracked process tree members (`member=<pid> <lstart>`).
pub(crate) fn record_pause_with_members(
    data_dir: &Path,
    pgid: u32,
    leader_started: Option<&str>,
    members: &[ProcId],
) -> io::Result<PauseRecord> {
    let guest = is_guest(data_dir);
    let path = record_path(data_dir, pgid);
    // Locked under a name the sweep ignores, then renamed: a record is never
    // visible without its lock, so a sweep cannot take a fresh one for lost.
    let draft = path.with_file_name(format!("{PREFIX}{pgid}.tmp"));
    let mut file = if guest {
        open_guest_file(&draft)?
    } else {
        ensure_closed_dir(data_dir)?;
        private::open_file(&draft)?
    };
    // A draft left by a run that died between create and rename.
    file.set_len(0)?;
    file.try_lock().map_err(io::Error::other)?;
    if let Some(started) = leader_started
        && !started.contains(['\n', '\r'])
    {
        let started = started.trim();
        if !started.is_empty() {
            file.write_all(format!("{STARTED_KEY}{started}\n").as_bytes())?;
        }
    }
    let mut written = 0usize;
    for member in members {
        if written >= MAX_RECORDED_MEMBERS {
            break;
        }
        if member.pid <= 1 || member.started.contains(['\n', '\r']) {
            continue;
        }
        let started = member.started.trim();
        if started.is_empty() {
            continue;
        }
        file.write_all(format!("{MEMBER_KEY}{} {started}\n", member.pid).as_bytes())?;
        written += 1;
    }
    fs::rename(&draft, &path)?;
    Ok(PauseRecord {
        path,
        _locked: file,
    })
}

impl PauseRecord {
    /// The group runs again: nothing is left to recover.
    pub(crate) fn clear(self) {
        // A record that cannot be removed is swept by the next heavy run,
        // which at worst continues a group that is already running.
        let _ = fs::remove_file(&self.path);
    }

    /// What the death of the owner leaves: the file without its lock.
    /// Unlocked explicitly, because a child that another test thread is
    /// spawning right now holds a copy of the descriptor until it execs.
    #[cfg(test)]
    pub(crate) fn abandon(self) {
        self._locked.unlock().unwrap();
    }
}

/// True when the group `state` describes is the one the record was written
/// for, and still stopped.
fn is_the_stopped_group(recorded_start: Option<&str>, state: Option<&GroupState>) -> bool {
    match state {
        Some(GroupState {
            stopped: true,
            leader_started: Some(now),
        }) => recorded_start == Some(now.as_str()),
        // The leader is gone and members remain. The kernel gives a pid to
        // no new process while a group still carries it as its id, so these
        // are members of the group that was stopped.
        Some(GroupState {
            stopped: true,
            leader_started: None,
        }) => true,
        // Running (somebody continued it, or it is another group by now),
        // or no such group.
        _ => false,
    }
}

#[cfg(unix)]
fn is_plain_record_file(at_path: &fs::Metadata, opened: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    at_path.is_file()
        && !at_path.file_type().is_symlink()
        && opened.is_file()
        && at_path.dev() == opened.dev()
        && at_path.ino() == opened.ino()
        && opened.nlink() == 1
}

#[cfg(not(unix))]
fn is_plain_record_file(at_path: &fs::Metadata, opened: &fs::Metadata) -> bool {
    at_path.is_file() && !at_path.file_type().is_symlink() && opened.is_file()
}

fn read_record_bounded(file: &File) -> String {
    let mut buf = Vec::new();
    let _ = file.take(MAX_RECORD_BYTES).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

struct ParsedRecord<'a> {
    leader_started: Option<&'a str>,
    members: Vec<ProcId>,
}

fn parse_member_line(rest: &str) -> Option<ProcId> {
    let (pid_str, started_raw) = rest.split_once(' ')?;
    let pid = pid_str.parse::<u32>().ok().filter(|pid| *pid > 1)?;
    let started = started_raw.trim();
    if started.is_empty() {
        return None;
    }
    Some(ProcId {
        pid,
        started: started.to_string(),
    })
}

fn parse_record(text: &str) -> ParsedRecord<'_> {
    let mut leader_started = None;
    let mut members = Vec::new();
    for line in text.lines() {
        if leader_started.is_none()
            && let Some(started) = line.strip_prefix(STARTED_KEY)
        {
            let started = started.trim();
            if !started.is_empty() {
                leader_started = Some(started);
            }
            continue;
        }
        if members.len() >= MAX_RECORDED_MEMBERS {
            continue;
        }
        if let Some(rest) = line.strip_prefix(MEMBER_KEY)
            && let Some(member) = parse_member_line(rest)
        {
            members.push(member);
        }
    }
    ParsedRecord {
        leader_started,
        members,
    }
}

/// Continues every group whose record outlived its owner, when it is still
/// that group and still stopped, and wakes any stopped helper members
/// recorded in it before removing the record. Returns how many records were
/// swept.
pub fn resume_orphans(data_dir: &Path, control: &dyn GroupControl) -> usize {
    // No directory: nothing was ever stopped from this data dir.
    let Ok(entries) = fs::read_dir(records_dir(data_dir)) else {
        return 0;
    };
    let mut swept = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pgid) = name
            .to_str()
            .and_then(|name| name.strip_prefix(PREFIX))
            .and_then(|pgid| pgid.parse::<u32>().ok())
            // 0 and 1 are never a command's group; as a kill target they
            // would mean this process's own group and init.
            .filter(|pgid| *pgid > 1)
        else {
            continue;
        };
        let path = entry.path();
        // Never through a symlink or hardlink: a guest can put any entry into `guest/`.
        let Ok(before) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !before.is_file() || before.file_type().is_symlink() {
            continue;
        }
        let Ok(file) = File::open(&path) else {
            continue;
        };
        let Ok(opened) = file.metadata() else {
            continue;
        };
        let Ok(after) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !is_plain_record_file(&before, &opened) || !is_plain_record_file(&after, &opened) {
            continue;
        }
        // Still locked: the owner lives and continues the group itself.
        if file.try_lock().is_err() {
            continue;
        }
        let gone = sweep_one(&file, &path, pgid, control);
        // Unlocked explicitly ([`unlock_all`]): a record that stays would
        // otherwise read as held by a live owner to the next sweep until
        // each child being spawned at this moment has exec'ed.
        unlock_all(std::slice::from_ref(&file));
        swept += usize::from(gone);
    }
    swept
}

/// Continues the group of the record at `path`, which the caller has
/// locked, when it is still the stopped one, and wakes any stopped helper
/// members recorded in it; true when the record is gone.
fn sweep_one(file: &File, path: &Path, pgid: u32, control: &dyn GroupControl) -> bool {
    // No answer from the process table decides nothing: the record
    // stays for the next heavy run. Removing it here left the group
    // stopped for good whenever `ps` ran over its time under load.
    let Ok(state) = control.read_group_state(pgid) else {
        return false;
    };
    let recorded = read_record_bounded(file);
    let parsed = parse_record(&recorded);
    if !parsed.members.is_empty() && control.resume_stopped_members(&parsed.members).is_err() {
        return false;
    }
    if is_the_stopped_group(parsed.leader_started, state.as_ref()) {
        // If the signal fails nothing more can be done for the group;
        // either way the record goes.
        let _ = control.resume(pgid);
    }
    fs::remove_file(path).is_ok()
}

// --- A command that runs on after its `aaf` was killed (not stopped) ---

/// True when the group `state` describes is the one a dead holder recorded
/// for its command, and it still has members.
fn is_the_recorded_group(recorded_start: Option<&str>, state: Option<&GroupState>) -> bool {
    match state {
        // The leader lives: it is that command if it started when the record
        // says. A record without a start time is what a holder leaves that
        // was killed between writing the group id and the `ps` run that
        // reads the start time (up to 2 s under memory pressure): the group
        // counts as the command then. Reading that case as "free" admitted
        // the next command on top of the one still running. The price: if
        // the command has ended too and its id went to an unrelated group
        // leader, the slot waits for that process; the message names the
        // group so a person can tell.
        Some(GroupState {
            leader_started: Some(now),
            ..
        }) => recorded_start.is_none_or(|recorded| recorded == now.as_str()),
        // The leader is gone and members remain. The kernel gives a pid to
        // no new process while a group still carries it as its id, so these
        // are members of the recorded group, unless that group ended and a
        // later leader with the same id left members behind in turn.
        Some(GroupState {
            leader_started: None,
            ..
        }) => true,
        None => false,
    }
}

/// The process group named in a lock file nobody holds, when that group
/// still runs: the command of a holder that was killed.
pub(super) fn orphan_in(
    text: &str,
    group_state: &dyn Fn(u32) -> Option<GroupState>,
) -> Option<u32> {
    orphan_reading(text, &|pgid| Ok(group_state(pgid))).unwrap_or(None)
}

/// [`orphan_in`] that tells a group that is gone (`Ok(None)`) from a process
/// table that could not be read (`Err`). The caller decides nothing on the
/// latter: taking it for "gone" overwrote the record of a command that
/// still ran whenever `ps` went over its time under load.
pub(super) fn orphan_reading(
    text: &str,
    read_group_state: &dyn Fn(u32) -> Result<Option<GroupState>, Unreadable>,
) -> Result<Option<u32>, Unreadable> {
    let Some(pgid) = super::slots::recorded_pgid(text) else {
        return Ok(None);
    };
    let started = text.lines().find_map(|line| line.strip_prefix(STARTED_KEY));
    let state = read_group_state(pgid)?;
    Ok(is_the_recorded_group(started, state.as_ref()).then_some(pgid))
}
