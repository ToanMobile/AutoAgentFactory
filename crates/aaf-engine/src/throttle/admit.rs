//! Admission: the wait for a slot, then for a calm machine.

use std::io;
use std::path::Path;

use aaf_domain::ports::Clock;

use super::heavy::HeavyTool;
use super::queue::{self, WaitTicket};
use super::slots::{self, Admission, Busy, DrainGuard, MachineView};
use super::{Admitted, Backoff, Pressure, Progress, Throttle, stats};

/// A waiting command reports at most this often while nothing changes.
const PROGRESS_EVERY_SECS: i64 = 10;

/// A command that runs alone starts draining the slots once it has waited
/// this long (see [`slots::try_drain`]).
///
/// ponytail: fixed at one minute; make it a `[machine]` setting when a
/// machine with many slots shows that measured commands wait too long
/// behind a loner, or a loner too long behind them.
pub const DRAIN_AFTER_SECS: u64 = 60;

/// Who asks for a slot.
#[derive(Debug, Clone, Copy)]
pub struct Ask<'a> {
    /// Identifies the repository in `heavy_stats.json`.
    pub repo_id: &'a str,
    pub label: &'a str,
    /// Build tool class (used as a fallback measurement key across labels).
    pub tool: Option<HeavyTool>,
    /// Command family for stats fallback (`@cmd:<family>`) when `tool` is `None`.
    pub cmd_family: Option<&'a str>,
    /// Runs alone whatever was measured of it.
    pub exclusive: bool,
    /// Longest admission wait; `None` waits as long as it takes.
    pub wait_limit_secs: Option<u64>,
}

/// Emits a wait report when its kind changes, else at most every
/// [`PROGRESS_EVERY_SECS`].
struct Reporter {
    last_kind: Option<u8>,
    last_at: i64,
}

fn waited_from(since: i64, now: i64) -> u64 {
    u64::try_from(now - since).unwrap_or(0)
}

impl Throttle<'_> {
    fn report(&self, reporter: &mut Reporter, now: i64, event: &Progress<'_>) {
        let kind = match event {
            Progress::WaitingForSlot { .. } => 0u8,
            Progress::WaitingForMemory { .. } => 1,
            _ => 2,
        };
        if reporter.last_kind != Some(kind) || now - reporter.last_at >= PROGRESS_EVERY_SECS {
            reporter.last_kind = Some(kind);
            reporter.last_at = now;
            (self.progress)(event);
        }
    }

    /// Waits for a slot, then for the machine to calm down. Waiting for a
    /// slot or for memory has no ceiling (every holder is bounded by its own
    /// command timeout); waiting for CPU load ends after `heavy_wait_secs`
    /// with `degraded` set.
    ///
    /// ponytail: waiters poll `try_lock` with backoff and jitter, so order is
    /// not first come, first served; upgrade to a ticket queue when more
    /// than 8 processes wait at once.
    pub fn admit(
        &self,
        data_dir: &Path,
        clock: &dyn Clock,
        repo_id: &str,
        label: &str,
    ) -> io::Result<Admitted> {
        let ask = Ask {
            repo_id,
            label,
            tool: None,
            cmd_family: None,
            exclusive: false,
            wait_limit_secs: None,
        };
        // Without a limit the wait only ends with a slot.
        self.admit_ask(data_dir, clock, &ask)?
            .ok_or_else(|| io::Error::other("admission ended without a slot"))
    }

    /// [`Throttle::admit`] for any kind of command. `None` when
    /// `wait_limit_secs` passed first; nothing is held then.
    pub fn admit_ask(
        &self,
        data_dir: &Path,
        clock: &dyn Clock,
        ask: &Ask<'_>,
    ) -> io::Result<Option<Admitted>> {
        let label = ask.label;
        let started = clock.now();
        let waited = |now: i64| waited_from(started, now);
        let expired = |now: i64| {
            ask.wait_limit_secs
                .is_some_and(|limit| waited(now) >= limit)
        };
        let mut reporter = Reporter {
            last_kind: None,
            last_at: started,
        };
        let mut free_unreadable = false;
        let mut load_wait_started: Option<i64> = None;
        let mut warned = (false, false);

        // Wait for a calm machine BEFORE locking any heavy slot to eliminate
        // hold-and-wait starvation when memory pressure is Warn/Critical or
        // CPU load is elevated, then acquire the slot and re-verify once.
        loop {
            // Phase A: wait for calm memory pressure and CPU load without holding a slot.
            let mut calm_backoff = Backoff::new(self.jitter_seed);
            let mut degraded = false;
            loop {
                let now = clock.now();
                let pressure = self.probe.pressure();
                if pressure.is_none() && !warned.0 {
                    warned.0 = true;
                    (self.progress)(&Progress::ProbeUnreadable {
                        what: "memory pressure",
                    });
                }
                let event = match pressure {
                    Some(pressure @ (Pressure::Warn | Pressure::Critical)) => {
                        Progress::WaitingForMemory {
                            label,
                            pressure,
                            waited_secs: waited(now),
                        }
                    }
                    Some(Pressure::Normal) | None => {
                        let max = self.machine.max_load_percent;
                        if max == 0 {
                            break;
                        }
                        let load = self.probe.load_percent();
                        if load.is_none() && !warned.1 {
                            warned.1 = true;
                            (self.progress)(&Progress::ProbeUnreadable { what: "CPU load" });
                        }
                        let Some(load) = load.filter(|load| *load > max) else {
                            break;
                        };
                        let since = *load_wait_started.get_or_insert(now);
                        if waited_from(since, now) >= self.machine.heavy_wait_secs {
                            degraded = true;
                            break;
                        }
                        Progress::WaitingForLoad {
                            label,
                            load_percent: load,
                            max_load_percent: max,
                            waited_secs: waited(now),
                            gives_up_after_secs: self.machine.heavy_wait_secs,
                        }
                    }
                };
                if expired(now) {
                    return Ok(None);
                }
                self.report(&mut reporter, now, &event);
                self.sleeper.sleep(calm_backoff.next_delay());
            }

            // Phase B: acquire a heavy slot.
            let mut slot_backoff = Backoff::new(self.jitter_seed);
            let mut drain: Option<DrainGuard> = None;
            let mut ticket: Option<WaitTicket> = None;
            let slot = loop {
                // Read again on every attempt: a sibling task of the same repo
                // may record the first measurement while this one waits.
                let peak = stats::recorded_peak_for(
                    data_dir,
                    ask.repo_id,
                    label,
                    ask.tool,
                    ask.cmd_family,
                );
                let admission = match peak {
                    Some(peak_rss_bytes) if !ask.exclusive => {
                        Admission::Measured { peak_rss_bytes }
                    }
                    _ => Admission::Unmeasured,
                };
                if admission != Admission::Unmeasured {
                    // Measured in the meantime: it no longer needs the machine
                    // to itself, and would shut itself out.
                    drain = None;
                } else {
                    // Unmeasured / exclusive commands use `drain.lock` instead of
                    // the measured FIFO ticket queue.
                    ticket = None;
                }
                let ceiling = self.ceiling();
                // FIFO fairness for measured commands: if older live waiters are
                // queued ahead and there are not enough free slots for them plus
                // us, take our own ticket and yield so newcomers polling at 250 ms
                // never starve long-waiting processes.
                let attempt = if admission != Admission::Unmeasured {
                    let ahead = queue::older_waiters(data_dir, ticket.as_ref());
                    if ahead > 0 && slots::free_slot_count(data_dir, ceiling) <= ahead {
                        Ok(Err(Busy::SlotsTaken))
                    } else {
                        let view = MachineView {
                            free_memory: &|| self.probe.free_memory_bytes(),
                            group_rss: &|pgid| {
                                self.control
                                    .tree(pgid, &[])
                                    .map(|sample| sample.rss_bytes)
                                    .or_else(|| self.control.rss_bytes(pgid))
                            },
                            group_state: &|pgid| self.control.group_state(pgid),
                        };
                        slots::try_acquire_in(data_dir, ceiling, admission, &view)
                    }
                } else {
                    let view = MachineView {
                        free_memory: &|| self.probe.free_memory_bytes(),
                        group_rss: &|pgid| {
                            self.control
                                .tree(pgid, &[])
                                .map(|sample| sample.rss_bytes)
                                .or_else(|| self.control.rss_bytes(pgid))
                        },
                        group_state: &|pgid| self.control.group_state(pgid),
                    };
                    slots::try_acquire_in(data_dir, ceiling, admission, &view)
                };
                match attempt? {
                    Ok(slot) => break slot,
                    Err(reason) => {
                        if matches!(reason, Busy::NotEnoughMemory { free: None, .. })
                            && !free_unreadable
                        {
                            free_unreadable = true;
                            (self.progress)(&Progress::ProbeUnreadable {
                                what: "free memory",
                            });
                        }
                        let now = clock.now();
                        if expired(now) {
                            return Ok(None);
                        }
                        if admission != Admission::Unmeasured {
                            match reason {
                                Busy::SlotsTaken | Busy::Draining => {
                                    if ticket.is_none() {
                                        ticket = WaitTicket::join(data_dir, started);
                                    }
                                }
                                // Release ticket while waiting on RAM so a large
                                // job never head-of-line blocks smaller jobs.
                                Busy::NotEnoughMemory { .. } => {
                                    ticket = None;
                                }
                                _ => {}
                            }
                        }
                        if admission == Admission::Unmeasured
                            && drain.is_none()
                            && waited(now) >= DRAIN_AFTER_SECS
                        {
                            // Failing to drain only costs fairness: the wait
                            // goes on as before.
                            drain = slots::try_drain(data_dir).ok().flatten();
                        }
                        let event = Progress::WaitingForSlot {
                            label,
                            reason,
                            waited_secs: waited(now),
                        };
                        self.report(&mut reporter, now, &event);
                        self.sleeper.sleep(slot_backoff.next_delay());
                    }
                }
            };
            // It holds the slot(s) now; the queue behind it moves again.
            drop(ticket);
            drop(drain);

            // Phase C: re-check memory pressure right after acquiring the slot.
            // If pressure spiked while waiting for a slot, drop the slot so we
            // never hold-and-wait on a pressured machine.
            let pressure_now = self.probe.pressure();
            if matches!(pressure_now, Some(Pressure::Warn | Pressure::Critical)) {
                drop(slot);
                continue;
            }
            return Ok(Some(Admitted {
                slot,
                waited_secs: waited(clock.now()),
                degraded,
            }));
        }
    }
}
