use std::fs::{self, OpenOptions};
use std::sync::Arc;
use std::sync::atomic::AtomicI64;

use tempfile::TempDir;

use super::super::stats::{HeavySample, KEEP_RUNS, record, recorded_peak, samples, stats_path};
use super::support::{FakeSleeper, TestClock};

fn sleeper() -> FakeSleeper {
    FakeSleeper::new(Arc::new(TestClock(AtomicI64::new(0))))
}

fn sample(peak_rss_bytes: u64) -> HeavySample {
    HeavySample {
        peak_rss_bytes,
        duration_ms: 1_500,
    }
}

#[test]
fn missing_file_means_never_measured() {
    let data = TempDir::new().unwrap();
    assert_eq!(recorded_peak(data.path(), "repo", "build"), None);
    assert!(samples(data.path(), "repo", "build").is_empty());
}

#[test]
fn record_then_read_back_per_repo_and_label() {
    let data = TempDir::new().unwrap();
    let sleeper = sleeper();
    assert!(record(data.path(), "repo-a", "build", sample(300), &sleeper).unwrap());
    assert!(record(data.path(), "repo-a", "test_full", sample(900), &sleeper).unwrap());
    assert!(record(data.path(), "repo-b", "build", sample(50), &sleeper).unwrap());

    assert_eq!(recorded_peak(data.path(), "repo-a", "build"), Some(300));
    assert_eq!(recorded_peak(data.path(), "repo-a", "test_full"), Some(900));
    assert_eq!(recorded_peak(data.path(), "repo-b", "build"), Some(50));
    assert_eq!(recorded_peak(data.path(), "repo-b", "test_full"), None);
    assert_eq!(samples(data.path(), "repo-a", "build"), vec![sample(300)]);
    assert_eq!(sleeper.count(), 0);
}

#[test]
fn peak_is_the_maximum_of_the_last_five_runs() {
    let data = TempDir::new().unwrap();
    let sleeper = sleeper();
    // The huge first run falls out of the window after five more.
    for peak in [9_000, 100, 400, 200, 300, 150] {
        record(data.path(), "repo", "build", sample(peak), &sleeper).unwrap();
    }
    let kept = samples(data.path(), "repo", "build");
    assert_eq!(kept.len(), KEEP_RUNS);
    assert_eq!(kept[0], sample(100));
    assert_eq!(recorded_peak(data.path(), "repo", "build"), Some(400));
}

#[test]
fn corrupt_file_is_unmeasured_and_gets_replaced() {
    let data = TempDir::new().unwrap();
    for garbage in ["{ not json", "[]", "{\"entries\": 7}", ""] {
        fs::write(stats_path(data.path()), garbage).unwrap();
        assert_eq!(
            recorded_peak(data.path(), "repo", "build"),
            None,
            "{garbage:?}"
        );
    }
    assert!(record(data.path(), "repo", "build", sample(77), &sleeper()).unwrap());
    assert_eq!(recorded_peak(data.path(), "repo", "build"), Some(77));
}

#[test]
fn a_zero_peak_is_not_a_measurement() {
    let data = TempDir::new().unwrap();
    record(data.path(), "repo", "build", sample(0), &sleeper()).unwrap();
    assert_eq!(recorded_peak(data.path(), "repo", "build"), None);
}

#[test]
fn write_is_atomic_and_leaves_no_temp_file() {
    let data = TempDir::new().unwrap();
    record(data.path(), "repo", "build", sample(5), &sleeper()).unwrap();
    let names: Vec<String> = fs::read_dir(data.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.contains(&"heavy_stats.json".to_string()));
    assert!(names.iter().all(|name| !name.contains("tmp")), "{names:?}");
    // The stored document is valid JSON on its own.
    let body = fs::read(stats_path(data.path())).unwrap();
    serde_json::from_slice::<serde_json::Value>(&body).unwrap();
}

#[test]
fn a_held_stats_lock_skips_the_record_instead_of_blocking() {
    let data = TempDir::new().unwrap();
    let sleeper = sleeper();
    record(data.path(), "repo", "build", sample(5), &sleeper).unwrap();

    let lock = OpenOptions::new()
        .write(true)
        .open(data.path().join("locks/heavy_stats.lock"))
        .unwrap();
    lock.lock().unwrap();
    // Not written while another writer holds the lock...
    assert!(!record(data.path(), "repo", "build", sample(999), &sleeper).unwrap());
    assert_eq!(recorded_peak(data.path(), "repo", "build"), Some(5));
    assert!(sleeper.count() > 0);
    // ...and written once it is free again.
    lock.unlock().unwrap();
    assert!(record(data.path(), "repo", "build", sample(999), &sleeper).unwrap());
    assert_eq!(recorded_peak(data.path(), "repo", "build"), Some(999));
}
