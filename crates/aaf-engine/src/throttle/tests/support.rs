//! Fakes shared by the throttle tests and the gate tests.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aaf_domain::config::MachineConfig;
use aaf_domain::ports::Clock;

use super::super::probe::ProcId;
use super::super::watchdog::TreeSample;
use super::super::{
    BuildDaemon, GroupControl, LoadProbe, Pressure, PriorityTools, Progress, Sleeper, SystemGroup,
    Throttle,
};

pub const GIB: u64 = 1024 * 1024 * 1024;

/// A clock the fake sleeper moves forward.
pub struct TestClock(pub AtomicI64);

impl Clock for TestClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Answers in order; the last answer repeats for ever.
pub struct Script<T> {
    items: Mutex<VecDeque<T>>,
    pub calls: AtomicUsize,
}

impl<T: Clone> Script<T> {
    pub fn new(items: Vec<T>) -> Script<T> {
        assert!(!items.is_empty());
        Script {
            items: Mutex::new(items.into()),
            calls: AtomicUsize::new(0),
        }
    }

    pub fn set(&self, items: Vec<T>) {
        *self.items.lock().unwrap() = items.into();
    }

    fn next(&self) -> T {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut items = self.items.lock().unwrap();
        if items.len() > 1 {
            items.pop_front().unwrap()
        } else {
            items.front().unwrap().clone()
        }
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

pub struct FakeProbe {
    pub load: Script<Option<u32>>,
    pub pressure: Script<Option<Pressure>>,
    pub free: Script<Option<u64>>,
}

impl FakeProbe {
    /// An idle machine with plenty of memory.
    pub fn calm() -> FakeProbe {
        FakeProbe {
            load: Script::new(vec![Some(10)]),
            pressure: Script::new(vec![Some(Pressure::Normal)]),
            free: Script::new(vec![Some(64 * GIB)]),
        }
    }
}

impl LoadProbe for FakeProbe {
    fn load_percent(&self) -> Option<u32> {
        self.load.next()
    }

    fn pressure(&self) -> Option<Pressure> {
        self.pressure.next()
    }

    fn free_memory_bytes(&self) -> Option<u64> {
        self.free.next()
    }
}

type SleepHook = Box<dyn FnMut(usize) + Send>;

/// Never sleeps: records the request, moves the clock, runs the hook.
pub struct FakeSleeper {
    pub clock: Arc<TestClock>,
    pub slept: Mutex<Vec<Duration>>,
    hook: Mutex<Option<SleepHook>>,
}

impl FakeSleeper {
    pub fn new(clock: Arc<TestClock>) -> FakeSleeper {
        FakeSleeper {
            clock,
            slept: Mutex::new(Vec::new()),
            hook: Mutex::new(None),
        }
    }

    /// `hook` gets the number of sleeps so far, this one included.
    pub fn on_sleep(&self, hook: impl FnMut(usize) + Send + 'static) {
        *self.hook.lock().unwrap() = Some(Box::new(hook));
    }

    pub fn count(&self) -> usize {
        self.slept.lock().unwrap().len()
    }
}

impl Sleeper for FakeSleeper {
    fn sleep(&self, duration: Duration) {
        let count = {
            let mut slept = self.slept.lock().unwrap();
            slept.push(duration);
            slept.len()
        };
        // At least a second per sleep so that waits are visible to a clock
        // that only counts seconds.
        let secs = i64::try_from(duration.as_secs().max(1)).unwrap();
        self.clock.0.fetch_add(secs, Ordering::SeqCst);
        if let Some(hook) = self.hook.lock().unwrap().as_mut() {
            hook(count);
        }
    }
}

#[derive(Default)]
pub struct FakeGroup {
    pub rss: Mutex<VecDeque<Option<u64>>>,
    pub stops: AtomicUsize,
    pub resumes: AtomicUsize,
    /// When true the signals "fail".
    pub refuse: Mutex<bool>,
    /// Daemon listings, one per look; none left means no daemon runs.
    pub daemons: Mutex<VecDeque<Vec<BuildDaemon>>>,
    /// Working directory per pid; a pid not listed cannot be read.
    pub cwds: Mutex<HashMap<u32, PathBuf>>,
    /// What the process tree looks like on every sample; `None` means it
    /// cannot be read (the group's own figure is used then).
    pub tree: Mutex<Option<TreeSample>>,
    /// Every `signal_members` call: the signal and whom it went to.
    pub signalled: Mutex<Vec<(String, Vec<ProcId>)>>,
}

impl GroupControl for FakeGroup {
    fn tree(&self, _pgid: u32, _known: &[ProcId]) -> Option<TreeSample> {
        self.tree.lock().unwrap().clone()
    }

    fn signal_members(&self, signal: &str, members: &[ProcId]) -> usize {
        let mut signalled = self.signalled.lock().unwrap();
        signalled.push((signal.to_string(), members.to_vec()));
        members.len()
    }

    fn build_daemons(&self) -> Vec<BuildDaemon> {
        self.daemons.lock().unwrap().pop_front().unwrap_or_default()
    }

    fn process_cwd(&self, pid: u32) -> Option<PathBuf> {
        self.cwds.lock().unwrap().get(&pid).cloned()
    }

    fn rss_bytes(&self, _pgid: u32) -> Option<u64> {
        self.rss.lock().unwrap().pop_front().flatten()
    }

    fn stop(&self, _pgid: u32) -> bool {
        self.stops.fetch_add(1, Ordering::SeqCst);
        !*self.refuse.lock().unwrap()
    }

    fn resume(&self, _pgid: u32) -> bool {
        self.resumes.fetch_add(1, Ordering::SeqCst);
        !*self.refuse.lock().unwrap()
    }
}

type ProgressFn = Box<dyn Fn(&Progress<'_>) + Send + Sync>;

/// Everything a [`Throttle`] borrows, owned in one place.
pub struct Rig {
    pub machine: MachineConfig,
    pub clock: Arc<TestClock>,
    pub probe: FakeProbe,
    pub sleeper: FakeSleeper,
    pub group: FakeGroup,
    pub events: Arc<Mutex<Vec<String>>>,
    progress: ProgressFn,
    pub cpus: usize,
    pub tools: PriorityTools,
    /// Real `ps` and `/bin/kill` instead of the fake group.
    pub real_group: bool,
    pub sample_every: Duration,
}

impl Rig {
    /// A calm machine with two slots; nothing ever has to wait.
    pub fn calm() -> Rig {
        let clock = Arc::new(TestClock(AtomicI64::new(1_000)));
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        Rig {
            machine: MachineConfig {
                max_heavy_parallel: 2,
                ..MachineConfig::default()
            },
            sleeper: FakeSleeper::new(Arc::clone(&clock)),
            clock,
            probe: FakeProbe::calm(),
            group: FakeGroup::default(),
            events,
            progress: Box::new(move |event| sink.lock().unwrap().push(format!("{event:?}"))),
            cpus: 8,
            tools: PriorityTools::default(),
            real_group: false,
            sample_every: Duration::from_millis(20),
        }
    }

    pub fn throttle(&self) -> Throttle<'_> {
        Throttle {
            machine: &self.machine,
            probe: &self.probe,
            sleeper: &self.sleeper,
            control: if self.real_group {
                &SystemGroup
            } else {
                &self.group
            },
            progress: &*self.progress,
            cpus: self.cpus,
            tools: self.tools.clone(),
            jitter_seed: 7,
            sample_every: self.sample_every,
        }
    }

    pub fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }

    pub fn events_starting(&self, prefix: &str) -> usize {
        self.events()
            .iter()
            .filter(|event| event.starts_with(prefix))
            .count()
    }
}

/// The real group ([`SystemGroup`]), plus `file` written as soon as a
/// sample reads a resident size above zero. A command that waits for the
/// file ([`after_file`]) cannot end before it has been measured, however
/// slow a loaded machine makes the first sample (a command that had already
/// ended was read as a zombie, 0 bytes).
pub struct MarksSampled {
    pub file: PathBuf,
}

impl MarksSampled {
    fn mark(&self, rss: Option<u64>) {
        if rss.is_some_and(|rss| rss > 0) {
            std::fs::write(&self.file, "").unwrap();
        }
    }
}

impl GroupControl for MarksSampled {
    fn rss_bytes(&self, pgid: u32) -> Option<u64> {
        let rss = SystemGroup.rss_bytes(pgid);
        self.mark(rss);
        rss
    }

    fn tree(&self, pgid: u32, known: &[ProcId]) -> Option<TreeSample> {
        let sample = SystemGroup.tree(pgid, known);
        self.mark(sample.as_ref().map(|sample| sample.rss_bytes));
        sample
    }

    fn stop(&self, pgid: u32) -> bool {
        SystemGroup.stop(pgid)
    }

    fn resume(&self, pgid: u32) -> bool {
        SystemGroup.resume(pgid)
    }

    fn group_state(&self, pgid: u32) -> Option<super::super::GroupState> {
        SystemGroup.group_state(pgid)
    }

    fn signal_members(&self, signal: &str, members: &[ProcId]) -> usize {
        SystemGroup.signal_members(signal, members)
    }
}

/// The first answer of `read`, a look at the real process table that is
/// `None` when `ps` gave no answer. The product has two seconds per `ps`
/// ([`super::super::probe::PROBE_TIMEOUT`]); a loaded machine running this
/// suite's other `ps` calls at the same time went over that (load 50-75).
/// A test about what the listing says asks again instead of failing on the
/// missing answer; the answer it gets is asserted in full.
pub fn answered<T>(what: &str, mut read: impl FnMut() -> Option<T>) -> T {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(answer) = read() {
            return answer;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{what}: the process table gave no answer for 60 s"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Shell text that waits until `file` exists, then runs `then`. Exit 99
/// after about a minute without it: only a broken run gets that far, and
/// it then fails on the exit code instead of hanging.
pub fn after_file(file: &std::path::Path, then: &str) -> String {
    format!(
        "n=0; while [ ! -e '{}' ]; do [ \"$n\" -lt 3000 ] || exit 99; \
         /bin/sleep 0.02; n=$((n+1)); done; {then}",
        file.display()
    )
}
