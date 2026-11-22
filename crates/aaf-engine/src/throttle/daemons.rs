//! Build daemons. A Gradle build normally hands its work to a `GradleDaemon`
//! (and the Kotlin plugin to a `KotlinCompileDaemon`) in a session of its
//! own: outside the command's process group, so its memory is not measured
//! and a stop signal never reaches it. Heavy commands are therefore started
//! with the daemons switched off, and a daemon that shows up anyway is
//! reported.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use super::probe;

/// Gradle user guide, "Disable Daemon": documented for `GRADLE_OPTS`.
pub const GRADLE_DAEMON_OFF: &str = "-Dorg.gradle.daemon=false";

/// The Kotlin Gradle property `kotlin.compiler.execution.strategy`, given as
/// the system property Gradle maps to a project property
/// (`org.gradle.project.<name>`). The plain system property
/// `kotlin.compiler.execution.strategy` is deprecated since Kotlin 1.7.
pub const KOTLIN_IN_PROCESS: &str =
    "-Dorg.gradle.project.kotlin.compiler.execution.strategy=in-process";

/// `GRADLE_OPTS` for a heavy command: what the caller had, then the two
/// switches (a later `-D` wins in the JVM). Nothing is added twice.
pub fn gradle_opts(existing: Option<&OsStr>) -> OsString {
    let mut opts = existing.map(OsStr::to_os_string).unwrap_or_default();
    for flag in [GRADLE_DAEMON_OFF, KOTLIN_IN_PROCESS] {
        let present = opts
            .to_string_lossy()
            .split_whitespace()
            .any(|word| word == flag);
        if !present {
            if !opts.is_empty() {
                opts.push(" ");
            }
            opts.push(flag);
        }
    }
    opts
}

/// A running Gradle or Kotlin compile daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildDaemon {
    pub pid: u32,
    /// `gradle` or `kotlin`.
    pub kind: &'static str,
    /// Start time as `ps` prints it; with the pid it names one process.
    pub started: String,
    pub rss_bytes: u64,
}

/// Columns of the listing [`parse_daemons`] reads.
pub const PS_DAEMON_COLUMNS: &str = "pid=,rss=,lstart=,command=";

/// The daemons in `ps -A -o pid=,rss=,lstart=,command=` (rss in KiB, start
/// time in five words). Lines that do not fit are left out.
pub fn parse_daemons(ps_output: &str) -> Vec<BuildDaemon> {
    ps_output
        .lines()
        .filter_map(|line| {
            let kind = if line.contains("GradleDaemon") {
                "gradle"
            } else if line.contains("KotlinCompileDaemon") {
                "kotlin"
            } else {
                return None;
            };
            let words: Vec<&str> = line.split_whitespace().collect();
            if words.len() < 8 {
                return None;
            }
            Some(BuildDaemon {
                pid: words[0].parse().ok()?,
                kind,
                started: words[2..7].join(" "),
                rss_bytes: words[1].parse::<u64>().ok()?.saturating_mul(1024),
            })
        })
        .collect()
}

/// The daemons running now; empty when `ps` cannot be read.
pub fn list() -> Vec<BuildDaemon> {
    read().unwrap_or_default()
}

/// The daemons running now; `None` when `ps` gave no answer.
pub(crate) fn read() -> Option<Vec<BuildDaemon>> {
    // `-ww`: the class name comes after a long class path, and BSD `ps`
    // may cut the line at 80 columns otherwise.
    probe::tool_output(probe::PS, &["-A", "-ww", "-o", PS_DAEMON_COLUMNS])
        .map(|listing| parse_daemons(&listing))
}

/// The daemons of `after` that were not there in `before`.
pub fn started_since(before: &[BuildDaemon], after: Vec<BuildDaemon>) -> Vec<BuildDaemon> {
    after
        .into_iter()
        .filter(|now| {
            !before
                .iter()
                .any(|old| old.pid == now.pid && old.started == now.started)
        })
        .collect()
}

/// True for `<gradle user home>/daemon/<version>`, where a Gradle daemon
/// sits when it is not in the directory of a build.
fn is_gradle_daemon_home(cwd: &Path) -> bool {
    cwd.parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == "daemon")
}

/// The daemons of `started` that a build in `work_dir` can have started.
/// Left out is only what certainly is somebody else's (another checkout of
/// the same app, an IDE): a Gradle daemon working in a project directory
/// that is not below `work_dir`. A Gradle daemon started by `gradlew` works
/// in the directory `gradlew` was called from.
///
/// Everything else stays in, because unknown is still worth a line: a
/// daemon whose directory cannot be read; a Gradle daemon in Gradle's own
/// daemon directory; and every Kotlin compile daemon, which works in
/// `~/Library/Application Support/kotlin/daemon` (macOS) whatever project
/// it compiles, so its directory never says whose it is (measured with
/// `lsof -a -p <pid> -d cwd`).
pub fn working_below(
    work_dir: &Path,
    started: Vec<BuildDaemon>,
    cwd_of: &dyn Fn(u32) -> Option<PathBuf>,
) -> Vec<BuildDaemon> {
    // Compared resolved: on macOS a temp directory sits behind a symlink.
    let resolved = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let work_dir = resolved(work_dir);
    started
        .into_iter()
        .filter(|daemon| {
            daemon.kind != "gradle"
                || cwd_of(daemon.pid).is_none_or(|cwd| {
                    let cwd = resolved(&cwd);
                    cwd.starts_with(&work_dir) || is_gradle_daemon_home(&cwd)
                })
        })
        .collect()
}
