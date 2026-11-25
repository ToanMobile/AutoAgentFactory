#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::os::unix::fs::symlink;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use aaf_domain::config::MachineConfig;
use aaf_domain::ports::Clock;
use aaf_engine::throttle::orphans::{MAX_RECORDED_MEMBERS, record_path, resume_orphans};
use aaf_engine::throttle::probe::{
    GroupState, LoadProbe, MemberState, Pressure, ProcId, Unreadable, parse_member_states,
    stopped_member_pids,
};
use aaf_engine::throttle::slots::{Admission, MachineView, try_acquire_in};
use aaf_engine::throttle::stats;
use aaf_engine::throttle::watchdog::{
    GroupControl, PriorityTools, SystemGroup, TreeSample, resume_stopped_members_verified,
};
use aaf_engine::throttle::{HeavyJob, HeavySample, Progress, Sleeper, Throttle};
use tempfile::TempDir;

const REPO_ID: &str = "oracle-rfc049";
const LABEL: &str = "test_scoped";
const LEADER_STARTED: &str = "Tue Nov 24 12:00:00 2026";
const HELPER_STARTED: &str = "Tue Nov 24 12:00:01 2026";
const HELPER_PID: u32 = 999_049;

struct StaticClock;
impl Clock for StaticClock {
    fn now(&self) -> i64 {
        1_000
    }
}

struct NoopSleeper;
impl Sleeper for NoopSleeper {
    fn sleep(&self, _duration: Duration) {}
}

struct CalmProbe;
impl LoadProbe for CalmProbe {
    fn load_percent(&self) -> Option<u32> {
        Some(0)
    }
    fn pressure(&self) -> Option<Pressure> {
        Some(Pressure::Normal)
    }
    fn free_memory_bytes(&self) -> Option<u64> {
        Some(u64::MAX)
    }
}

fn proc_id(pid: u32, started: &str) -> ProcId {
    ProcId {
        pid,
        started: started.to_string(),
    }
}

fn dead_pgid() -> u32 {
    let mut child = Command::new("/usr/bin/true")
        .process_group(0)
        .spawn()
        .or_else(|_| Command::new("/bin/true").process_group(0).spawn())
        .unwrap();
    let pgid = child.id();
    let _ = child.wait();
    pgid
}

fn ps_state(pid: u32) -> String {
    let out = Command::new("/bin/ps")
        .args(["-o", "state=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn wait_stopped(pid: u32, stopped: bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if ps_state(pid).contains('T') == stopped {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn read_lstart(pid: u32) -> String {
    for _ in 0..10 {
        if let Ok(Some(GroupState {
            leader_started: Some(started),
            ..
        })) = SystemGroup.read_group_state(pid)
        {
            return started;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("ps did not report lstart for pid {pid}");
}

fn run_trivial_with_system_group(data_dir: &Path, work_dir: &Path) {
    let quiet = |_: &Progress<'_>| {};
    let machine = MachineConfig {
        max_heavy_parallel: 2,
        max_load_percent: 0,
        low_priority: false,
        exclusive_build_tools: false,
        ..MachineConfig::default()
    };
    let throttle = Throttle {
        machine: &machine,
        probe: &CalmProbe,
        sleeper: &NoopSleeper,
        control: &SystemGroup,
        progress: &quiet,
        cpus: 4,
        tools: PriorityTools::default(),
        jitter_seed: 1,
        sample_every: Duration::from_millis(50),
    };
    let job = HeavyJob {
        repo_id: REPO_ID,
        label: LABEL,
        command: "true",
        cwd: work_dir,
        timeout: Duration::from_secs(30),
        log_tail_bytes: 256,
    };
    let run = throttle.run_heavy(data_dir, &StaticClock, &job).unwrap();
    assert_eq!(run.exit_code, Some(0));
}

struct PauseCaptureMachine {
    data_dir: PathBuf,
    release_file: PathBuf,
    watchdog_active: AtomicBool,
    stopped: AtomicBool,
    captured_record: Mutex<Option<String>>,
    members: Vec<ProcId>,
}

impl LoadProbe for PauseCaptureMachine {
    fn load_percent(&self) -> Option<u32> {
        Some(0)
    }
    fn pressure(&self) -> Option<Pressure> {
        if !self.watchdog_active.load(Ordering::SeqCst) || self.stopped.load(Ordering::SeqCst) {
            Some(Pressure::Normal)
        } else {
            Some(Pressure::Critical)
        }
    }
    fn free_memory_bytes(&self) -> Option<u64> {
        Some(u64::MAX)
    }
}

impl GroupControl for PauseCaptureMachine {
    fn rss_bytes(&self, _pgid: u32) -> Option<u64> {
        Some(4096)
    }
    fn stop(&self, pgid: u32) -> bool {
        *self.captured_record.lock().unwrap() =
            fs::read_to_string(record_path(&self.data_dir, pgid)).ok();
        self.stopped.store(true, Ordering::SeqCst);
        true
    }
    fn resume(&self, _pgid: u32) -> bool {
        let _ = fs::write(&self.release_file, b"go\n");
        true
    }
    fn group_state(&self, _pgid: u32) -> Option<GroupState> {
        Some(GroupState {
            stopped: self.stopped.load(Ordering::SeqCst),
            leader_started: Some(LEADER_STARTED.to_string()),
        })
    }
    fn tree(&self, pgid: u32, _known: &[ProcId]) -> Option<TreeSample> {
        self.watchdog_active.store(true, Ordering::SeqCst);
        let mut members = vec![proc_id(pgid, LEADER_STARTED)];
        members.extend(self.members.iter().cloned());
        Some(TreeSample {
            rss_bytes: 4096,
            members,
        })
    }
    fn signal_members(&self, _signal: &str, members: &[ProcId]) -> usize {
        members.len()
    }
}

struct RecordingControl {
    group_state_result: Option<GroupState>,
    member_unreadable_left: Mutex<usize>,
    resumed_groups: Mutex<Vec<u32>>,
    resumed_member_batches: Mutex<Vec<Vec<ProcId>>>,
}

impl RecordingControl {
    fn new(group_state_result: Option<GroupState>, member_unreadable_left: usize) -> Self {
        Self {
            group_state_result,
            member_unreadable_left: Mutex::new(member_unreadable_left),
            resumed_groups: Mutex::new(Vec::new()),
            resumed_member_batches: Mutex::new(Vec::new()),
        }
    }
}

impl GroupControl for RecordingControl {
    fn rss_bytes(&self, _pgid: u32) -> Option<u64> {
        None
    }
    fn stop(&self, _pgid: u32) -> bool {
        true
    }
    fn resume(&self, pgid: u32) -> bool {
        self.resumed_groups.lock().unwrap().push(pgid);
        true
    }
    fn read_group_state(&self, _pgid: u32) -> Result<Option<GroupState>, Unreadable> {
        Ok(self.group_state_result.clone())
    }
    fn resume_stopped_members(&self, members: &[ProcId]) -> Result<usize, Unreadable> {
        let mut left = self.member_unreadable_left.lock().unwrap();
        if *left > 0 {
            *left -= 1;
            return Err(Unreadable);
        }
        self.resumed_member_batches
            .lock()
            .unwrap()
            .push(members.to_vec());
        Ok(members.len())
    }
}

// Scenario (1a): Watchdog::stop persists `member=<pid> <lstart>` and blocks invalid entries.
#[test]
fn rfc049_watchdog_stop_persists_tracked_members_in_pause_record() {
    let data = TempDir::new().unwrap();
    let release_file = data.path().join("release");
    let helper = proc_id(HELPER_PID, HELPER_STARTED);
    let fake = PauseCaptureMachine {
        data_dir: data.path().to_path_buf(),
        release_file: release_file.clone(),
        watchdog_active: AtomicBool::new(false),
        stopped: AtomicBool::new(false),
        captured_record: Mutex::new(None),
        members: vec![
            helper.clone(),
            proc_id(1, HELPER_STARTED),
            proc_id(
                HELPER_PID + 1,
                &format!("{HELPER_STARTED}\nmember=777777 injected"),
            ),
            proc_id(
                HELPER_PID + 2,
                &format!("{HELPER_STARTED}\rmember=888888 injected"),
            ),
        ],
    };

    let sample = HeavySample {
        peak_rss_bytes: 1,
        duration_ms: 10,
    };
    stats::record(data.path(), REPO_ID, LABEL, sample, &NoopSleeper).unwrap();
    let view = MachineView {
        free_memory: &|| Some(u64::MAX),
        group_rss: &|_| None,
        group_state: &|_| None,
    };
    let _slot0 = try_acquire_in(
        data.path(),
        2,
        Admission::Measured { peak_rss_bytes: 1 },
        &view,
    )
    .unwrap()
    .unwrap();

    let quiet = |_: &Progress<'_>| {};
    let machine = MachineConfig {
        max_heavy_parallel: 2,
        max_load_percent: 0,
        low_priority: false,
        exclusive_build_tools: true,
        ..MachineConfig::default()
    };
    let throttle = Throttle {
        machine: &machine,
        probe: &fake,
        sleeper: &NoopSleeper,
        control: &fake,
        progress: &quiet,
        cpus: 4,
        tools: PriorityTools::default(),
        jitter_seed: 1,
        sample_every: Duration::from_millis(10),
    };
    let job = HeavyJob {
        repo_id: REPO_ID,
        label: LABEL,
        command: r#"i=0; while [ ! -f "$AAF_RFC049_DONE" ] && [ "$i" -lt 200 ]; do /bin/sleep 0.01; i=$((i+1)); done"#,
        cwd: data.path(),
        timeout: Duration::from_secs(10),
        log_tail_bytes: 512,
    };
    let env = [("AAF_RFC049_DONE", release_file.to_str().unwrap())];
    let run = throttle
        .run_heavy_with_env(data.path(), &StaticClock, &job, &env)
        .unwrap();
    assert_eq!(run.exit_code, Some(0));

    let captured = fake
        .captured_record
        .lock()
        .unwrap()
        .clone()
        .expect("Watchdog::stop must write pause record before stopping");
    let expected = format!("member={} {}", helper.pid, helper.started);
    assert!(captured.lines().any(|l| l == expected), "{captured}");
    assert!(!captured.contains("member=1 "), "{captured}");
    assert!(
        !captured.contains("777777") && !captured.contains("888888"),
        "{captured}"
    );
}

fn sweep_real_helper_record(record_lstart_override: Option<&str>) -> (bool, bool, bool) {
    let data = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let dead = dead_pgid();
    let mut helper = Command::new("/bin/sleep")
        .arg("60")
        .process_group(0)
        .spawn()
        .unwrap();
    let helper_pid = helper.id();
    let lstart = record_lstart_override
        .map(str::to_string)
        .unwrap_or_else(|| read_lstart(helper_pid));
    let stopped = SystemGroup.stop(helper_pid) && wait_stopped(helper_pid, true);
    let path = record_path(data.path(), dead);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        format!("lstart=Thu Jan 1 00:00:00 1970\nmember={helper_pid} {lstart}\n"),
    )
    .unwrap();
    for _ in 0..10 {
        if !path.exists() {
            break;
        }
        run_trivial_with_system_group(data.path(), work.path());
    }
    let resumed = if record_lstart_override.is_none() {
        wait_stopped(helper_pid, false)
    } else {
        !ps_state(helper_pid).contains('T')
    };
    let record_removed = !path.exists();
    let _ = helper.kill();
    let _ = helper.wait();
    (stopped, record_removed, resumed)
}

// Scenario (1b): Real stopped helper is woken when main group is dead (`Ok(None)`) and file is removed.
#[test]
fn rfc049_real_stopped_helper_is_woken_when_main_group_is_dead() {
    let (stopped, record_removed, resumed) = sweep_real_helper_record(None);
    assert!(stopped, "helper was never stopped");
    assert!(record_removed, "pause record must be removed after sweep");
    assert!(
        resumed,
        "stopped helper must be resumed when main group is dead"
    );
}

// Scenario (2a): Real stopped helper with mismatched lstart does NOT receive SIGCONT.
#[test]
fn rfc049_real_stopped_helper_with_mismatched_lstart_is_not_woken() {
    let (stopped, record_removed, resumed) =
        sweep_real_helper_record(Some("Thu Jan 1 00:00:00 1970"));
    assert!(stopped, "helper was never stopped");
    assert!(record_removed, "pause record must be removed after sweep");
    assert!(!resumed, "mismatched lstart must NOT receive SIGCONT");
}

// Scenario (2b): Verified resume skips mismatched lstart and non-stopped (`S`/`R`/`Z`) processes.
#[test]
fn rfc049_verified_resume_skips_mismatched_lstart_and_non_stopped_states() {
    let listing = "\
    1 T    Tue Nov 24 12:00:01 2026
  200 T    Tue Nov 24 12:00:01 2026
  201 T    Tue Nov 24 12:05:00 2026
  202 S    Tue Nov 24 12:00:01 2026
  203 R+   Tue Nov 24 12:00:01 2026
  204 Z    Tue Nov 24 12:00:01 2026
  205 T+   Tue Nov 24 12:00:01 2026
  206 TN   Tue Nov 24 12:00:01 2026
";
    let members: Vec<ProcId> = [1, 200, 201, 202, 203, 204, 205, 206]
        .into_iter()
        .map(|pid| proc_id(pid, HELPER_STARTED))
        .collect();
    let sent: Mutex<Vec<(String, Vec<String>)>> = Mutex::new(Vec::new());
    let send = |sig: &str, pids: &[String]| {
        sent.lock().unwrap().push((sig.to_string(), pids.to_vec()));
    };

    let count =
        resume_stopped_members_verified(&|| Some(listing.to_string()), &send, &members).unwrap();
    assert_eq!(count, 3);
    assert_eq!(
        *sent.lock().unwrap(),
        vec![(
            "-CONT".to_string(),
            vec!["200".to_string(), "205".to_string(), "206".to_string()]
        )]
    );
}

// Scenario (3): Unreadable `ps` -> NO signal sent (no raw-PID fallback) AND pause record preserved.
#[test]
fn rfc049_unreadable_ps_sends_no_signal_and_keeps_pause_record() {
    let members = vec![proc_id(HELPER_PID, HELPER_STARTED)];
    let sent = AtomicUsize::new(0);
    let res = resume_stopped_members_verified(
        &|| None,
        &|_, _| {
            sent.fetch_add(1, Ordering::SeqCst);
        },
        &members,
    );
    assert_eq!(res, Err(Unreadable));
    assert_eq!(sent.load(Ordering::SeqCst), 0);

    let data = TempDir::new().unwrap();
    let path = record_path(data.path(), 55_555);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        format!("lstart={LEADER_STARTED}\nmember={HELPER_PID} {HELPER_STARTED}\n"),
    )
    .unwrap();

    let dead_ctrl = RecordingControl::new(None, 1);
    assert_eq!(resume_orphans(data.path(), &dead_ctrl), 0);
    assert!(path.exists());
    assert!(dead_ctrl.resumed_groups.lock().unwrap().is_empty());
    assert!(dead_ctrl.resumed_member_batches.lock().unwrap().is_empty());

    assert_eq!(resume_orphans(data.path(), &dead_ctrl), 1);
    assert!(!path.exists());
    assert_eq!(
        *dead_ctrl.resumed_member_batches.lock().unwrap(),
        vec![members]
    );

    let path2 = record_path(data.path(), 55_556);
    fs::write(
        &path2,
        format!("lstart={LEADER_STARTED}\nmember={HELPER_PID} {HELPER_STARTED}\n"),
    )
    .unwrap();
    let stopped_ctrl = RecordingControl::new(
        Some(GroupState {
            leader_started: Some(LEADER_STARTED.to_string()),
            stopped: true,
        }),
        1,
    );
    assert_eq!(resume_orphans(data.path(), &stopped_ctrl), 0);
    assert!(path2.exists());
    assert!(stopped_ctrl.resumed_groups.lock().unwrap().is_empty());
    assert!(
        stopped_ctrl
            .resumed_member_batches
            .lock()
            .unwrap()
            .is_empty()
    );
}

// Scenario (4): `member=0`, `member=1`, malformed lines, >256 lines capped, symlinks & hardlinks skipped.
#[test]
fn rfc049_filters_invalid_members_caps_at_256_and_skips_links() {
    let data = TempDir::new().unwrap();
    let pgid = 66_666;
    let path = record_path(data.path(), pgid);
    fs::create_dir_all(path.parent().unwrap()).unwrap();

    let mut content = format!(
        "lstart={LEADER_STARTED}\n\
         member=0 {HELPER_STARTED}\n\
         member=1 {HELPER_STARTED}\n\
         member=abc {HELPER_STARTED}\n\
         member=500\n\
         member=501   \n"
    );
    for i in 0..(MAX_RECORDED_MEMBERS + 44) {
        let pid = 10_000 + u32::try_from(i).unwrap();
        content.push_str(&format!("member={pid} {HELPER_STARTED}\n"));
    }
    fs::write(&path, content).unwrap();

    let target = data.path().join("outside_secret.txt");
    fs::write(&target, format!("member={HELPER_PID} {HELPER_STARTED}\n")).unwrap();
    let sym_path = record_path(data.path(), pgid + 1);
    symlink(&target, &sym_path).unwrap();

    let hard_src = data.path().join("hardlink_src.txt");
    fs::write(&hard_src, format!("member={HELPER_PID} {HELPER_STARTED}\n")).unwrap();
    let hard_path = record_path(data.path(), pgid + 2);
    fs::hard_link(&hard_src, &hard_path).unwrap();

    let ctrl = RecordingControl::new(None, 0);
    assert_eq!(resume_orphans(data.path(), &ctrl), 1);

    let batches = ctrl.resumed_member_batches.lock().unwrap();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].len(), MAX_RECORDED_MEMBERS);
    assert!(batches[0].iter().all(|m| m.pid > 1));
    assert_eq!(batches[0].first().unwrap().pid, 10_000);
    assert_eq!(
        batches[0].last().unwrap().pid,
        10_000 + u32::try_from(MAX_RECORDED_MEMBERS - 1).unwrap()
    );

    assert!(!path.exists());
    assert!(fs::symlink_metadata(&sym_path).is_ok_and(|m| m.file_type().is_symlink()));
    assert!(target.exists());
    assert!(hard_path.exists());
}

// Scenario (5): Probe parser separates `T`/`T+`/`TN` vs `S`/`R`/`Z` and enforces exact `lstart`.
#[test]
fn rfc049_probe_member_state_parser_distinguishes_stopped_and_exact_lstart() {
    let listing = "\
    1 T    Sun Nov  1 09:12:08 2026
  100 T    Sun Nov  1 09:12:08 2026
  101 T+   Sun Nov 22 09:12:08 2026
  102 TN   Sun Nov 22 09:12:08 2026
  103 S    Sun Nov 22 09:12:08 2026
  104 Ss   Sun Nov 22 09:12:08 2026
  105 R    Sun Nov 22 09:12:08 2026
  106 R+   Sun Nov 22 09:12:08 2026
  107 Z    Sun Nov 22 09:12:08 2026
  108 T    Sun Nov 22 09:59:59 2026
  200 T
garbage
";
    let parsed = parse_member_states(listing);
    assert_eq!(parsed.len(), 10);
    assert_eq!(
        parsed[1],
        MemberState {
            id: proc_id(100, "Sun Nov 1 09:12:08 2026"),
            stopped: true,
        }
    );
    for (pid, want_stopped) in [
        (101, true),
        (102, true),
        (103, false),
        (104, false),
        (105, false),
        (106, false),
        (107, false),
    ] {
        assert_eq!(
            parsed.iter().find(|r| r.id.pid == pid).unwrap().stopped,
            want_stopped,
            "pid {pid}"
        );
    }

    let wanted = vec![
        proc_id(1, "Sun Nov 1 09:12:08 2026"),
        proc_id(100, "Sun Nov 1 09:12:08 2026"),
        proc_id(101, "Sun Nov 22 09:12:08 2026"),
        proc_id(102, "Sun Nov 22 09:12:08 2026"),
        proc_id(103, "Sun Nov 22 09:12:08 2026"),
        proc_id(105, "Sun Nov 22 09:12:08 2026"),
        proc_id(107, "Sun Nov 22 09:12:08 2026"),
        proc_id(108, "Sun Nov 22 09:12:08 2026"),
    ];
    assert_eq!(stopped_member_pids(listing, &wanted), vec![100, 101, 102]);
}
