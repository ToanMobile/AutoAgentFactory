//! Running an admitted heavy command under the watchdog.

use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

use aaf_domain::ports::Clock;

use super::daemons::{self, BuildDaemon};
use super::heavy::{self, HeavyTool};
use super::pools;
use super::{
    Ask, HeavyJob, HeavyRun, HeavySample, Progress, Throttle, orphans, stats, token, watchdog,
};
use crate::gate::proc;

pub use super::token::SLOT_ENV;

/// Time the helpers of a timed-out command get between SIGTERM and SIGKILL.
const TREE_TERM_GRACE: Duration = Duration::from_secs(2);

/// What a heavy command inherits from the caller's environment.
#[derive(Debug, Clone, Default)]
pub(crate) struct Inherited {
    pub gradle_opts: Option<OsString>,
}

impl Inherited {
    fn from_process_env() -> Inherited {
        Inherited {
            gradle_opts: std::env::var_os("GRADLE_OPTS"),
        }
    }
}

/// A command for [`Throttle::run_slot`]: an argument vector whose output
/// goes straight to the caller's own streams.
#[derive(Debug)]
pub struct SlotJob<'a> {
    /// Identifies the repository (or directory) in `heavy_stats.json`.
    pub repo_id: &'a str,
    pub label: &'a str,
    pub argv: &'a [OsString],
    pub cwd: &'a Path,
    /// Longest wait for admission; `None` waits as long as it takes.
    pub wait_limit_secs: Option<u64>,
    /// Longest runtime once admitted (`None` = unlimited).
    pub exec_timeout: Option<Duration>,
    /// False gives the command an empty stdin: in a process group of its
    /// own it would be stopped for reading the terminal.
    pub inherit_stdin: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotRun {
    pub status: ExitStatus,
    pub timed_out: bool,
    /// Slot the command ended in.
    pub slot: usize,
    pub waited_secs: u64,
    pub degraded: bool,
    pub paused: Duration,
    pub peak_rss_bytes: Option<u64>,
    /// Gradle or Kotlin daemons the command left running.
    pub leftover_daemons: Vec<BuildDaemon>,
}

/// How the command's output is handled.
enum Output {
    /// Both streams captured, this many trailing bytes kept.
    Tail(usize),
    /// The streams the command was given.
    Passed,
}

/// One command to admit and run.
struct Launch<'a> {
    ask: Ask<'a>,
    /// The build tool it starts, if any; such a command runs alone.
    tool: Option<HeavyTool>,
    command: Command,
    timeout: Duration,
    output: Output,
}

struct Supervised {
    status: ExitStatus,
    timed_out: bool,
    duration: Duration,
    paused: Duration,
    output: Vec<u8>,
    slot: usize,
    waited_secs: u64,
    degraded: bool,
    peak_rss_bytes: Option<u64>,
    leftover_daemons: Vec<BuildDaemon>,
}

impl Throttle<'_> {
    /// Admits the command, runs it under the watchdog and records its
    /// memory use when it passed. `None` when the admission wait ran out:
    /// the command was never started.
    fn supervise(
        &self,
        data_dir: &Path,
        clock: &dyn Clock,
        launch: Launch<'_>,
        inherited: &Inherited,
    ) -> io::Result<Option<Supervised>> {
        let Launch {
            ask,
            tool,
            mut command,
            timeout,
            output,
        } = launch;
        // Before the wait for a slot: a stopped orphan may keep memory that
        // the admission below is waiting for.
        orphans::resume_orphans(data_dir, self.control);
        // Layer 1: when `exclusive_build_tools = false` allows concurrent heavy
        // builds to share Layer-2 capacity slots by RAM, acquire correctness locks
        // (Unity pool -> Worktree lock) strictly BEFORE Layer 2 capacity slots.
        // When `exclusive_build_tools = true`, Layer-2 already serializes every
        // build tool exclusively across the machine.
        let _correctness = if !self.machine.exclusive_build_tools {
            let cwd_for_lock = command
                .get_current_dir()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| {
                    std::env::current_dir().unwrap_or_else(|_| data_dir.to_path_buf())
                });
            let correctness_ask = pools::CorrectnessAsk {
                tool,
                cwd: &cwd_for_lock,
                max_unity_parallel: self.machine.max_unity_parallel,
                wait_limit_secs: ask.wait_limit_secs,
                jitter_seed: self.jitter_seed,
            };
            let Some(guard) =
                pools::acquire_correctness(data_dir, &correctness_ask, clock, self.sleeper)?
            else {
                return Ok(None);
            };
            Some(guard)
        } else {
            None
        };
        let Some(admitted) = self.admit_ask(data_dir, clock, &ask)? else {
            return Ok(None);
        };
        command
            .env(
                "GRADLE_OPTS",
                daemons::gradle_opts(inherited.gradle_opts.as_deref()),
            )
            .env(
                SLOT_ENV,
                token::slot_env_value(admitted.slot.index(), admitted.slot.token()),
            );
        // Only a Gradle command is watched for daemons: the listing costs
        // two `ps` runs, and any other command would only be blamed for the
        // daemons of builds that run outside this tool.
        let watch_daemons = tool == Some(HeavyTool::Gradle);
        let work_dir = command.get_current_dir().map(Path::to_path_buf);
        let daemons_before = if watch_daemons {
            self.control.build_daemons()
        } else {
            Vec::new()
        };
        let mut dog = watchdog::Watchdog::new(
            self.probe,
            self.control,
            self.progress,
            data_dir,
            ask.label,
            admitted.slot,
            self.sample_every,
        );
        let done = match output {
            Output::Tail(bytes) => {
                proc::capture_supervised(command, timeout, Some(bytes), true, &mut dog).map(
                    |done| {
                        (
                            proc::Waited {
                                status: done.status,
                                timed_out: done.timed_out,
                                duration: done.duration,
                                paused: done.paused,
                            },
                            done.output,
                        )
                    },
                )
            }
            Output::Passed => {
                proc::run_supervised(command, timeout, &mut dog).map(|(done, unkilled)| {
                    // Nothing is captured, so the caller's stream is where this
                    // is said (a captured run gets the line in its log).
                    if let Some(pgid) = unkilled {
                        (self.progress)(&Progress::Unkilled {
                            label: ask.label,
                            pgid,
                        });
                    }
                    (done, Vec::new())
                })
            }
        };
        dog.finish();
        let (done, output) = done?;
        if done.timed_out {
            // The timeout killed the command's process group. Its helpers
            // in other groups (and those reparented to pid 1) are still
            // running and would keep the memory the next command is
            // admitted on: end the processes that were seen in the tree,
            // each by its pid and start time, never by name.
            if self.control.signal_members("-TERM", &dog.tracked) > 0 {
                self.sleeper.sleep(TREE_TERM_GRACE);
                self.control.signal_members("-KILL", &dog.tracked);
            }
        }
        // Still inside the slot: the next command must not start on top of
        // a daemon nobody has reported.
        let leftover_daemons = if watch_daemons {
            let started = daemons::started_since(&daemons_before, self.control.build_daemons());
            match &work_dir {
                // Only those of this build: on a machine with other
                // sessions, somebody's daemon starts during most commands.
                Some(dir) => {
                    daemons::working_below(dir, started, &|pid| self.control.process_cwd(pid))
                }
                None => started,
            }
        } else {
            Vec::new()
        };
        // Record peak RSS when the command succeeded OR ran long enough (>= 5s)
        // to produce a representative memory sample even if tests failed.
        let recordable =
            !done.timed_out && (done.status.success() || done.duration >= Duration::from_secs(5));
        if let (true, Some(peak_rss_bytes)) = (recordable, dog.peak_rss_bytes) {
            let sample = HeavySample {
                peak_rss_bytes,
                duration_ms: u64::try_from(done.duration.as_millis()).unwrap_or(u64::MAX),
            };
            // The stats only tune admission. If they cannot be written the
            // command stays "never measured" and keeps running alone, which
            // is the safe side; the result is unaffected.
            let _ = stats::record(data_dir, ask.repo_id, ask.label, sample, self.sleeper);
            if let Some(t) = tool {
                let tool_key = stats::tool_label(t);
                let _ = stats::record(data_dir, ask.repo_id, &tool_key, sample, self.sleeper);
            } else if let Some(family) = ask.cmd_family {
                let cmd_key = stats::cmd_label(family);
                let _ = stats::record(data_dir, ask.repo_id, &cmd_key, sample, self.sleeper);
            }
        }
        Ok(Some(Supervised {
            status: done.status,
            timed_out: done.timed_out,
            duration: done.duration,
            paused: done.paused,
            output,
            slot: dog.slot.index(),
            waited_secs: admitted.waited_secs,
            degraded: admitted.degraded,
            peak_rss_bytes: dog.peak_rss_bytes,
            leftover_daemons,
        }))
    }

    /// Admits `job`, runs it under the watchdog and records its memory use
    /// when it passed. A command that starts Gradle, Unity or xcodebuild
    /// runs alone whatever was measured of it ([`heavy::heavy_tool`]).
    pub fn run_heavy(
        &self,
        data_dir: &Path,
        clock: &dyn Clock,
        job: &HeavyJob<'_>,
    ) -> io::Result<HeavyRun> {
        self.run_heavy_with_env(data_dir, clock, job, &[])
    }

    /// [`Throttle::run_heavy`] with verified extra environment variables (after
    /// stripping inherited `AAF_GATE_MODE`, `AAF_BASE_SHA`,
    /// `AAF_CHANGED_FILES_FILE`, and `AAF_DELETED_FILES_FILE`).
    pub fn run_heavy_with_env(
        &self,
        data_dir: &Path,
        clock: &dyn Clock,
        job: &HeavyJob<'_>,
        extra_env: &[(&str, &str)],
    ) -> io::Result<HeavyRun> {
        self.run_heavy_with_all(
            data_dir,
            clock,
            job,
            &Inherited::from_process_env(),
            extra_env,
        )
    }

    #[cfg(test)]
    pub(crate) fn run_heavy_with(
        &self,
        data_dir: &Path,
        clock: &dyn Clock,
        job: &HeavyJob<'_>,
        inherited: &Inherited,
    ) -> io::Result<HeavyRun> {
        self.run_heavy_with_all(data_dir, clock, job, inherited, &[])
    }

    fn run_heavy_with_all(
        &self,
        data_dir: &Path,
        clock: &dyn Clock,
        job: &HeavyJob<'_>,
        inherited: &Inherited,
        extra_env: &[(&str, &str)],
    ) -> io::Result<HeavyRun> {
        let tool = heavy::heavy_tool(job.command);
        let cmd_family = heavy::stats_command_family(job.command);
        let ask = Ask {
            repo_id: job.repo_id,
            label: job.label,
            tool,
            cmd_family,
            exclusive: self.machine.exclusive_build_tools && tool.is_some(),
            wait_limit_secs: None,
        };
        let argv = watchdog::heavy_argv(job.command, self.machine.low_priority, &self.tools);
        let mut command = Command::new(&argv[0]);
        command.args(&argv[1..]).current_dir(job.cwd);
        // When the gate itself runs inside a git hook these would point the
        // command's own git calls at the wrong repository. Also strip ambient
        // diff-context env vars so callers cannot spoof gate diff inputs.
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "AAF_GATE_MODE",
            "AAF_BASE_SHA",
            "AAF_CHANGED_FILES_FILE",
            "AAF_DELETED_FILES_FILE",
            "AAF_TEST_FILES_FILE",
        ] {
            command.env_remove(var);
        }
        for (k, v) in extra_env {
            command.env(k, v);
        }
        let launch = Launch {
            ask,
            tool,
            command,
            timeout: job.timeout,
            output: Output::Tail(job.log_tail_bytes),
        };
        let run = self
            .supervise(data_dir, clock, launch, inherited)?
            // Without a limit the wait only ends with a slot.
            .ok_or_else(|| io::Error::other("admission ended without a slot"))?;
        Ok(HeavyRun {
            exit_code: run.status.code(),
            timed_out: run.timed_out,
            duration: run.duration,
            output: run.output,
            slot: run.slot,
            waited_secs: run.waited_secs,
            degraded: run.degraded,
            paused: run.paused,
            peak_rss_bytes: run.peak_rss_bytes,
            leftover_daemons: run.leftover_daemons,
        })
    }

    /// Runs any command through the same queue as the gate's heavy
    /// commands, with the caller's own output streams and no time limit of
    /// its own. `None` when `wait_limit_secs` passed before it was admitted:
    /// it was never started.
    pub fn run_slot(
        &self,
        data_dir: &Path,
        clock: &dyn Clock,
        job: &SlotJob<'_>,
    ) -> io::Result<Option<SlotRun>> {
        self.run_slot_with(data_dir, clock, job, &Inherited::from_process_env())
    }

    pub(crate) fn run_slot_with(
        &self,
        data_dir: &Path,
        clock: &dyn Clock,
        job: &SlotJob<'_>,
        inherited: &Inherited,
    ) -> io::Result<Option<SlotRun>> {
        if job.argv.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no command given",
            ));
        }
        let tool = heavy::heavy_tool_argv(job.argv);
        let cmd_family = heavy::stats_command_family_argv(job.argv);
        let ask = Ask {
            repo_id: job.repo_id,
            label: job.label,
            tool,
            cmd_family,
            exclusive: self.machine.exclusive_build_tools && tool.is_some(),
            wait_limit_secs: job.wait_limit_secs,
        };
        let argv = watchdog::priority_argv(job.argv, self.machine.low_priority, &self.tools);
        let mut command = Command::new(&argv[0]);
        command.args(&argv[1..]).current_dir(job.cwd);
        if !job.inherit_stdin {
            command.stdin(Stdio::null());
        }
        // Bound execution time when the caller set `--exec-timeout`; otherwise
        // leave the runtime up to the caller.
        let launch = Launch {
            ask,
            tool,
            command,
            timeout: job.exec_timeout.unwrap_or(Duration::MAX),
            output: Output::Passed,
        };
        let run = self.supervise(data_dir, clock, launch, inherited)?;
        Ok(run.map(|run| SlotRun {
            status: run.status,
            timed_out: run.timed_out,
            slot: run.slot,
            waited_secs: run.waited_secs,
            degraded: run.degraded,
            paused: run.paused,
            peak_rss_bytes: run.peak_rss_bytes,
            leftover_daemons: run.leftover_daemons,
        }))
    }
}
