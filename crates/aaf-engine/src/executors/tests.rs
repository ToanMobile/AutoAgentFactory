use std::process::Command;
use std::time::{Duration, Instant};

use tempfile::TempDir;

use super::capture;

/// Kills the process whose pid is in the file, so no holder outlives a
/// failed test.
struct KillOnDrop(std::path::PathBuf);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Ok(pid) = std::fs::read_to_string(&self.0) {
            let _ = Command::new("/bin/kill")
                .args(["-KILL", pid.trim()])
                .status();
        }
    }
}

/// A helper (`ps`, `kill`) that exited has all its output read at once,
/// even while another process still holds its stdout: what a copy of the
/// stream that leaked into a child another thread spawned does on macOS
/// (`aaf_fp::output`). Read to the end of a pipe, the capture waited for
/// the holder and the reaper's `ps` came back as "timed out".
#[test]
fn another_holder_of_the_output_does_not_hold_a_finished_capture() {
    let dir = TempDir::new().unwrap();
    let pid_file = dir.path().join("holder.pid");
    let _holder = KillOnDrop(pid_file.clone());
    let script = format!(
        "/bin/sleep 30 & echo $! > '{}'; echo out",
        pid_file.display()
    );
    let started = Instant::now();
    let done = capture("/bin/sh", &["-c", &script], Duration::from_secs(10));
    let took = started.elapsed();
    assert_eq!(done.unwrap(), (Some(0), "out\n".to_string()));
    assert!(took < Duration::from_secs(5), "took {took:?}");
}

/// The allowed neighbour: a helper that does not end is still killed and
/// reported at its timeout.
#[test]
fn a_helper_that_does_not_end_is_reported_at_its_timeout() {
    let started = Instant::now();
    let done = capture("/bin/sleep", &["30"], Duration::from_millis(200));
    assert!(done.is_err(), "{done:?}");
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn secondary_volume_temp_dir_selects_per_uid_folder_on_macos_volumes() {
    use super::secondary_volume_temp_dir_in;
    use std::path::Path;

    let worktree = Path::new("/Volumes/Data/projects/sample-game");
    let exact = secondary_volume_temp_dir_in(worktree, Some(501), |p| {
        p == Path::new("/Volumes/Data/.TemporaryItems/folders.501")
    });
    assert_eq!(
        exact.as_deref(),
        Some(Path::new("/Volumes/Data/.TemporaryItems/folders.501"))
    );

    let fallback = secondary_volume_temp_dir_in(worktree, Some(501), |p| {
        p == Path::new("/Volumes/Data/.TemporaryItems")
    });
    assert_eq!(
        fallback.as_deref(),
        Some(Path::new("/Volumes/Data/.TemporaryItems"))
    );

    let boot = secondary_volume_temp_dir_in(Path::new("/Users/alice/repo"), Some(501), |_| true);
    assert_eq!(boot, None);

    let none = secondary_volume_temp_dir_in(worktree, Some(501), |_| false);
    assert_eq!(none, None);
}
