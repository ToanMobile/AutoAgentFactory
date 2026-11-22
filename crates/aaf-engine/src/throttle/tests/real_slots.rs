//! Real run: the kernel frees a slot when the process holding it dies.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};

use tempfile::TempDir;

use super::super::slots::{Admission, Busy, try_acquire};

const HOLD_ENV: &str = "AAF_TEST_HOLD_SLOT_DIR";
const HELD: &str = "slot-held";
const MEASURED: Admission = Admission::Measured { peak_rss_bytes: 1 };

/// Not a test of its own, hence ignored in a normal run: the test below
/// runs this test binary again with `--ignored --exact` and `HOLD_ENV` set
/// on the child, which then holds slot 0 until it is killed.
#[test]
#[ignore = "helper process of slot_is_released_by_the_kernel_when_its_holder_is_killed"]
fn child_holds_slot_zero_until_killed() {
    let Some(dir) = std::env::var_os(HOLD_ENV) else {
        return;
    };
    let _guard = try_acquire(Path::new(&dir), 1, MEASURED, &|| None)
        .unwrap()
        .unwrap();
    println!("{HELD}");
    // The parent keeps the pipe open and never writes: this blocks until the
    // child is killed (or ends the child if the parent itself dies).
    let mut sink = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut sink);
}

#[test]
fn slot_is_released_by_the_kernel_when_its_holder_is_killed() {
    let data = TempDir::new().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "throttle::tests::real_slots::child_holds_slot_zero_until_killed",
            "--nocapture",
        ])
        .env(HOLD_ENV, data.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let held = lines.any(|line| line.unwrap().contains(HELD));
    assert!(held, "the child never took the slot");

    // Another process holds it: this one is made to wait.
    let busy = try_acquire(data.path(), 1, MEASURED, &|| None).unwrap();
    assert_eq!(busy.unwrap_err(), Busy::SlotsTaken);

    // SIGKILL: the child gets no chance to unlock anything itself.
    child.kill().unwrap();
    child.wait().unwrap();
    let free = try_acquire(data.path(), 1, MEASURED, &|| None).unwrap();
    assert_eq!(free.unwrap().index(), 0);
}
