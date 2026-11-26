//! Machine-wide slots for heavy commands: advisory file locks under
//! `<data_dir>/throttle`, shared by every repo and every `aaf` process of
//! this user. The kernel drops a lock when its holder dies.
//!
//! The lock file holds the holder's reservation and process group; its token
//! lives beside it in the closed dir ([`super::token`]). A sandboxed guest
//! writes its record in `guest/` ([`super::slot_file`]).
//!
//! If `aaf` is killed while its command's process group still runs, the next
//! probe sees the leftover lock content ([`Busy::Orphaned`]) until the group
//! ends or the bounded orphan-wait timeout reaps the stale lock file.

mod orphan_reap;
mod orphan_since;

use std::fs::TryLockError;
use std::io;
use std::path::Path;

pub use orphan_reap::{reap_orphaned_slots, reap_stale_orphans};
use orphan_since::SidecarSet;
pub(super) use orphan_since::{OrphanTracker, observe_and_reap_orphans};

use super::drain::draining;
pub use super::drain::{DrainGuard, try_drain};
use super::legacy;
use super::orphans::{STARTED_KEY, orphan_in, orphan_reading};
pub use super::paths::{drain_path, locks_dir, slot_path};
use super::probe::{GroupState, Unreadable};
pub(super) use super::reservation::recorded_pgid;
pub use super::reservation::{Reservation, read_reservation};
use super::slot_file::{SlotFile, holder_token_text, lock_text, owner_token_text};
pub(super) use super::slot_file::{is_locked, unlock_all};
use super::token::SlotToken;

/// Free memory must cover the recorded peak with this margin (1.25 x).
const HEADROOM_NUM: u64 = 5;
const HEADROOM_DEN: u64 = 4;

/// Number of slots: `max_heavy_parallel`, or a quarter of the CPUs when 0.
pub fn ceiling(max_heavy_parallel: u32, cpus: usize) -> usize {
    match usize::try_from(max_heavy_parallel) {
        Ok(0) => (cpus / 4).max(1),
        Ok(explicit) => explicit,
        Err(_) => usize::MAX,
    }
}

/// Memory a command needs free before it may share the machine.
pub fn required_free(peak_rss_bytes: u64) -> u64 {
    peak_rss_bytes.saturating_mul(HEADROOM_NUM) / HEADROOM_DEN
}

/// What is known about the command asking for a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Peak resident memory of its recent runs.
    Measured { peak_rss_bytes: u64 },
    /// Its appetite is unknown (never measured) or cannot be trusted (a
    /// build tool that works through daemons), so it runs alone.
    Unmeasured,
}

/// Why no slot was given on this attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Busy {
    /// Every slot the command may use is taken, or a running command of
    /// unknown size leaves no room to share.
    SlotsTaken,
    /// Another unmeasured or exclusive command holds `drain.lock` and is
    /// draining the queue until every active slot empties.
    Draining,
    /// A command that runs alone waits until no other heavy command runs.
    RunsAlone,
    /// Free memory, less what the running commands are still expected to
    /// take, does not cover the recorded peak (`free` unknown ⇒ `None`).
    NotEnoughMemory { free: Option<u64>, needed: u64 },
    /// A slot is free of its holder but not of its command: the `aaf` that
    /// held it was killed and the command's process group still runs.
    Orphaned { pgid: u32 },
}

/// True when `text` holds exactly `token` in a well-formed token line.
pub(super) fn names_token(text: &str, token: &SlotToken) -> bool {
    text.lines()
        .find_map(|line| line.strip_prefix("token="))
        .and_then(SlotToken::parse)
        .is_some_and(|written| written.matches(token))
}

/// True when slot `index` is held now and its lock file holds exactly
/// `token`: the caller runs below the command of that holder.
pub fn holds_token(data_dir: &Path, index: usize, token: &SlotToken) -> bool {
    holds_token_in(data_dir, index, token, &|_| None)
}

/// [`holds_token`] that also knows the processes of the machine: a process
/// below the command of a killed holder is still inside that slot, for as
/// long as that command's group runs. Without this it would queue behind
/// its own ancestor, which never ends while it waits.
pub fn holds_token_in(
    data_dir: &Path,
    index: usize,
    token: &SlotToken,
    group_state: &dyn Fn(u32) -> Option<GroupState>,
) -> bool {
    if legacy::holds_token(data_dir, index, token, group_state) {
        return true;
    }
    if !is_locked(&slot_path(data_dir, index)) {
        // Trust only owner-written files; read twice around the group check so
        // a concurrent takeover invalidates the earlier token.
        let written = || names_token(&owner_token_text(data_dir, index), token);
        let text = lock_text(data_dir, index);
        return written() && orphan_in(&text, group_state).is_some() && written();
    }
    // Held first, read second: emptied on release and rewritten on acquire.
    names_token(&holder_token_text(data_dir, index), token)
}

/// What a holder writes beside its token has this length, so that rewriting
/// it (the process group arrives after the start) is one write over the
/// same bytes.
const CONTENT_LEN: usize = 128;

/// Reservation, process group and its start time, as a holder records them.
fn content(reservation: Reservation, started: Option<&str>) -> String {
    let mut text = reservation.render();
    // `ps` prints a date of 24 characters; anything longer is not one.
    if let Some(started) = started.filter(|started| started.len() <= 40) {
        text.push_str(&format!("{STARTED_KEY}{started}\n"));
    }
    while text.len() < CONTENT_LEN {
        text.push('\n');
    }
    text
}

/// Writes the holder's token and reservation as the record of a held slot.
/// The token line is written first and is the same on every rewrite: a
/// process below the holder that reads during one finds its token whichever
/// version it sees. A token that came and went would send it into the queue
/// behind its own ancestor.
fn write_holder(
    file: &SlotFile,
    token: &SlotToken,
    reservation: Reservation,
    started: Option<&str>,
) -> io::Result<()> {
    file.write_record(
        &format!("token={}\n", token.as_str()),
        &content(reservation, started),
    )
}

/// A held slot. Dropping it (or the death of the process) releases it.
#[derive(Debug)]
pub struct SlotGuard {
    index: usize,
    /// Locked files; a command that runs alone holds every slot so that
    /// nothing can start beside it.
    files: Vec<SlotFile>,
    sidecars: SidecarSet,
    reservation: Reservation,
    /// Start time of the command's group leader as `ps` prints it, once the
    /// command runs: what tells this group from a later one with its id.
    started: Option<String>,
    /// Drawn for this guard alone; gone from the lock files when it drops.
    token: SlotToken,
}

impl SlotGuard {
    /// Takes over locked files. On an error they are unlocked again: a slot
    /// without a token would make every command below it wait for itself.
    fn holding(
        index: usize,
        files: Vec<SlotFile>,
        sidecars: SidecarSet,
        reservation: Reservation,
    ) -> io::Result<SlotGuard> {
        let written = SlotToken::generate().and_then(|token| {
            files
                .iter()
                .try_for_each(|file| write_holder(file, &token, reservation, None))
                .map(|()| token)
        });
        match written {
            Ok(token) => Ok(SlotGuard {
                index,
                files,
                sidecars,
                reservation,
                started: None,
                token,
            }),
            Err(err) => {
                sidecars.clear();
                release_all(&files);
                Err(err)
            }
        }
    }

    pub fn index(&self) -> usize {
        self.index
    }

    /// What a process below this holder shows to prove where it runs.
    pub fn token(&self) -> &SlotToken {
        &self.token
    }

    /// Says which process group runs in this slot, so that an admission to
    /// another slot can see how much of the reservation is already in use.
    pub fn record_pgid(&mut self, pgid: u32) {
        self.record_group(pgid, None);
    }

    /// [`SlotGuard::record_pgid`] with the start time of the group's leader
    /// as `ps` prints it. If this process is killed, the record is how the
    /// next command finds the one it left running.
    pub fn record_group(&mut self, pgid: u32, leader_started: Option<String>) {
        self.reservation.pgid = Some(pgid);
        self.started = leader_started;
        for file in &self.files {
            // A group that could not be written leaves the whole reservation
            // counted against the others: the safe side.
            let _ = write_holder(file, &self.token, self.reservation, self.started.as_deref());
        }
    }

    /// Moves to slot 0 when it is free. A command paused in a later slot
    /// uses this so that one command always makes progress.
    pub fn promote(
        &mut self,
        data_dir: &Path,
        group_state: &dyn Fn(u32) -> Option<GroupState>,
    ) -> io::Result<bool> {
        if self.index == 0 {
            return Ok(true);
        }
        // Slot 0 with the command of a killed holder still in it is taken.
        let Probe::Taken(file) = try_slot(data_dir, 0, &|pgid| Ok(group_state(pgid)))? else {
            return Ok(false);
        };
        // The token moves along: the processes below this command still
        // carry it, with the number of the slot they were started in.
        if let Err(err) = write_holder(
            &file,
            &self.token,
            self.reservation,
            self.started.as_deref(),
        ) {
            release_all(&[file]);
            return Err(err);
        }
        // The later slot is released for the next command.
        std::mem::replace(&mut self.sidecars, SidecarSet::one(data_dir, 0)).clear();
        release_all(&std::mem::replace(&mut self.files, vec![file]));
        self.index = 0;
        Ok(true)
    }
}

/// Writes the initial unmeasured placeholder while locked; gives the slot back on error.
fn claim(file: &SlotFile) -> io::Result<()> {
    let nothing_yet = Reservation {
        reserved_bytes: None,
        pgid: None,
    };
    file.claim(&content(nothing_yet, None))
        .inspect_err(|_| release_all(std::slice::from_ref(file)))
}

/// Gives back slots that carry this process's token and reservation.
fn release_all(files: &[SlotFile]) {
    for file in files {
        // Empty while still locked so the next holder never inherits our token/reservation.
        let _ = file.clear_record();
        file.unlock();
    }
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.sidecars.clear();
        release_all(&self.files);
    }
}

/// What a look at one slot found.
enum Probe {
    /// Free; it is locked by the caller now.
    Taken(SlotFile),
    /// No lock file, and this process cannot create one: a guest, for which
    /// the slot does not exist.
    Missing,
    /// A live process holds it.
    Held,
    /// Nobody holds it, but the command of a killed holder still runs.
    Orphaned(u32),
}

fn try_slot(
    data_dir: &Path,
    index: usize,
    read_group: &dyn Fn(u32) -> Result<Option<GroupState>, Unreadable>,
) -> io::Result<Probe> {
    locks_dir(data_dir)?;
    let Some(file) = SlotFile::open(data_dir, index)? else {
        return Ok(Probe::Missing);
    };
    match file.try_lock() {
        Ok(()) => {
            // Content without a holder: left by one that was killed.
            let left_by_killed = file.lock_has_content().inspect_err(|_| file.unlock())?;
            // An empty lock file of a held slot means a guest holds it, and
            // its record in `guest/` is read. Claimed before the slower looks
            // below (each a `ps` run, and the other slots after this one), a
            // record an agent plants there meanwhile is not taken for this
            // holder's.
            if !left_by_killed {
                claim(&file)?;
            }
            let give_back = |file: SlotFile| {
                if left_by_killed {
                    // Not written to: left as it was found.
                    file.unlock();
                } else {
                    release_all(&[file]);
                }
            };
            if let Some(busy) = legacy::holder(data_dir, index, read_group) {
                give_back(file);
                return Ok(match busy {
                    legacy::Holder::Live => Probe::Held,
                    legacy::Holder::Orphan(pgid) => Probe::Orphaned(pgid),
                });
            }
            if left_by_killed {
                // Unreadable content names no group to wait for.
                let left = lock_text(data_dir, index);
                let busy = match orphan_reading(&left, read_group) {
                    Ok(None) => None,
                    // Its command still runs. The file stays as it is, for
                    // the next look and for the processes below that
                    // command, which still prove themselves with its token.
                    Ok(Some(pgid)) => Some(Probe::Orphaned(pgid)),
                    // No answer from the process table: its command may
                    // still run, so the slot is busy and the record stays.
                    Err(Unreadable) => Some(Probe::Held),
                };
                if let Some(busy) = busy {
                    give_back(file);
                    return Ok(busy);
                }
                // From this moment the slot reads as held, and that
                // holder's token must not be found in it: a process that
                // still carries it would run beside the next command. A
                // guest cannot empty the file: until a holder outside the
                // sandbox does ([`prepare_for_guests`] before every agent
                // run), readers take the dead holder's record for this
                // one's, which admits less beside it and never more.
                if !file.is_guest() {
                    claim(&file)?;
                }
            }
            file.drop_guest_leftover(data_dir, index);
            orphan_since::clear(data_dir, index);
            Ok(Probe::Taken(file))
        }
        Err(TryLockError::WouldBlock) => Ok(Probe::Held),
        Err(TryLockError::Error(err)) => Err(err),
    }
}

/// Creates what a guest ([`super::paths`]) needs and cannot create, and
/// empties what a killed holder left in a free slot, which a guest cannot
/// do either. Called by whoever starts a confined agent, before it starts;
/// `slots` is the number of slots the agent will find.
pub fn prepare_for_guests(
    data_dir: &Path,
    slots: usize,
    group_state: &dyn Fn(u32) -> Option<GroupState>,
) -> io::Result<()> {
    super::paths::prepare(data_dir)?;
    for index in 0..slots.max(1) {
        if let Probe::Taken(file) = try_slot(data_dir, index, &|pgid| Ok(group_state(pgid)))? {
            release_all(&[file]);
        }
    }
    Ok(())
}

fn no_slot_file(data_dir: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "no slot lock file in {} and no right to create one",
            super::paths::throttle_dir(data_dir).display()
        ),
    )
}

/// True while a live process holds slot `index`.
pub fn is_held(data_dir: &Path, index: usize) -> bool {
    is_locked(&slot_path(data_dir, index)) || legacy::is_held(data_dir, index)
}

/// What an admission attempt reads from the machine. Each is asked only
/// when it decides.
pub struct MachineView<'a> {
    /// Memory a new process can take without swapping.
    pub free_memory: &'a dyn Fn() -> Option<u64>,
    /// Resident memory of a process group now; `None` when it has no
    /// process or cannot be read.
    pub group_rss: &'a dyn Fn(u32) -> Option<u64>,
    /// Whether a process group exists and when its leader started; `None`
    /// when it has no process. Decides whether the command of a killed
    /// holder still occupies its slot.
    pub group_state: &'a dyn Fn(u32) -> Option<GroupState>,
}

/// Counts how many of the `0..ceiling` heavy slots are currently unlocked
/// (ignoring legacy files; used as a non-blocking hint for FIFO yielding).
pub fn free_slot_count(data_dir: &Path, ceiling: usize) -> usize {
    (0..ceiling.max(1))
        .filter(|&index| !is_locked(&slot_path(data_dir, index)))
        .count()
}

/// [`try_acquire_in`] for a caller that cannot read the memory of other
/// commands: every reservation then counts in full.
pub fn try_acquire(
    data_dir: &Path,
    ceiling: usize,
    admission: Admission,
    free_memory: &dyn Fn() -> Option<u64>,
) -> io::Result<Result<SlotGuard, Busy>> {
    let view = MachineView {
        free_memory,
        group_rss: &|_| None,
        // No look at the process table: a slot nobody holds is free.
        group_state: &|_| None,
    };
    try_acquire_in(data_dir, ceiling, admission, &view)
}

/// One non-blocking attempt.
///
/// A measured command is refused while a command that runs alone is
/// draining the slots ([`try_drain`]). Otherwise it takes the first free
/// slot. While no other slot is busy it needs nothing more. Beside running
/// commands it needs free memory
/// for its recorded peak with headroom, after setting aside what each of
/// them is still expected to grow by (its reservation less what its process
/// group uses now). A command that runs alone needs every slot idle, and
/// keeps them all.
pub fn try_acquire_in(
    data_dir: &Path,
    ceiling: usize,
    admission: Admission,
    view: &MachineView<'_>,
) -> io::Result<Result<SlotGuard, Busy>> {
    let read_group = |pgid| Ok((view.group_state)(pgid));
    try_acquire_reading(data_dir, ceiling, admission, view, &read_group)
}

/// [`try_acquire_in`] that reads process groups with `read_group`
/// instead of `view.group_state`: a process table that cannot be read
/// (`Err`) leaves a slot with a killed holder's record busy and untouched.
pub(crate) fn try_acquire_reading(
    data_dir: &Path,
    ceiling: usize,
    admission: Admission,
    view: &MachineView<'_>,
    read_group: &dyn Fn(u32) -> Result<Option<GroupState>, Unreadable>,
) -> io::Result<Result<SlotGuard, Busy>> {
    let ceiling = ceiling.max(1);
    let reservation = Reservation::of(admission);
    match admission {
        Admission::Unmeasured => acquire_alone(data_dir, ceiling, reservation, read_group),
        Admission::Measured { peak_rss_bytes } => {
            let needed = required_free(peak_rss_bytes);
            acquire_shared(data_dir, ceiling, reservation, needed, view, read_group)
        }
    }
}

fn acquire_alone(
    data_dir: &Path,
    ceiling: usize,
    reservation: Reservation,
    read_group: &dyn Fn(u32) -> Result<Option<GroupState>, Unreadable>,
) -> io::Result<Result<SlotGuard, Busy>> {
    let mut files = Vec::new();
    let mut sidecars = SidecarSet::empty();
    for index in 0..ceiling {
        let busy = match try_slot(data_dir, index, read_group)? {
            Probe::Taken(file) => {
                files.push(file);
                sidecars.push(data_dir, index);
                continue;
            }
            // Nobody can hold a slot that has no lock file.
            Probe::Missing => continue,
            Probe::Held => Busy::RunsAlone,
            Probe::Orphaned(pgid) => Busy::Orphaned { pgid },
        };
        // The slots taken so far are given back: holding them while idle
        // would leave the machine with no command running.
        release_all(&files);
        return Ok(Err(busy));
    }
    if files.is_empty() {
        return Err(no_slot_file(data_dir));
    }
    Ok(Ok(SlotGuard::holding(0, files, sidecars, reservation)?))
}

fn acquire_shared(
    data_dir: &Path,
    ceiling: usize,
    reservation: Reservation,
    needed: u64,
    view: &MachineView<'_>,
    read_group: &dyn Fn(u32) -> Result<Option<GroupState>, Unreadable>,
) -> io::Result<Result<SlotGuard, Busy>> {
    if draining(data_dir) {
        orphan_reap::reap_dead_slots(data_dir, ceiling, read_group);
        return Ok(Err(Busy::Draining));
    }
    let mut mine = None;
    let mut busy = Vec::new();
    let mut orphan = None;
    let mut missing = 0;
    for index in 0..ceiling {
        match try_slot(data_dir, index, read_group)? {
            Probe::Taken(file) if mine.is_none() => mine = Some((index, file)),
            // Free: it was only looked at.
            Probe::Taken(file) => release_all(&[file]),
            Probe::Missing => missing += 1,
            Probe::Held => busy.push(index),
            // Its command runs like any other; its reservation is still
            // in the file and is counted below.
            Probe::Orphaned(pgid) => {
                orphan = Some(pgid);
                busy.push(index);
            }
        }
    }
    let Some((index, file)) = mine else {
        if missing == ceiling {
            return Err(no_slot_file(data_dir));
        }
        return Ok(Err(match orphan {
            Some(pgid) => Busy::Orphaned { pgid },
            None => Busy::SlotsTaken,
        }));
    };
    // The slot is taken and the reservation written before memory is
    // judged: two commands that arrive together then see each other, and
    // at worst both step back and try again. Judging first would let both
    // through on the same free memory. Every refusal below drops the guard,
    // which gives the slot back.
    let guard = SlotGuard::holding(
        index,
        vec![file],
        SidecarSet::one(data_dir, index),
        reservation,
    )?;
    if busy.is_empty() {
        return Ok(Ok(guard));
    }
    let mut outstanding: u64 = 0;
    for other in busy {
        let Some(Reservation {
            reserved_bytes: Some(reserved),
            pgid,
        }) = read_reservation(data_dir, other)
        else {
            // Unreadable, or a command of unknown size: no sharing.
            return Ok(Err(Busy::SlotsTaken));
        };
        // Not started yet, or its memory cannot be read: all of the
        // reservation is still to come.
        let used = pgid.and_then(|pgid| (view.group_rss)(pgid)).unwrap_or(0);
        outstanding = outstanding.saturating_add(reserved.saturating_sub(used));
    }
    // Unknown free memory never admits beside another command.
    let free = (view.free_memory)().map(|free| free.saturating_sub(outstanding));
    if free.is_none_or(|free| free < needed) {
        return Ok(Err(Busy::NotEnoughMemory { free, needed }));
    }
    Ok(Ok(guard))
}
