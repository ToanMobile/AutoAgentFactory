#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Mutex;

use aaf_domain::ports::{ClosedFingerprint, FingerprintStore, PortError, PortResult};
use aaf_engine::detector::dedupe::{DedupeDecision, SUPPRESS_WINDOW_SECS, decide};

const REPO: &str = "/repos/shop";
const FP: &str = "fp-1";
const NOW: i64 = 1_800_000_000;

#[derive(Default)]
struct FakeStore {
    closed: Mutex<Vec<ClosedFingerprint>>,
    /// (fingerprint, repo_root, task_id)
    open: Mutex<Vec<(String, String, String)>>,
    fail: bool,
    /// Simulates a store that ignores `since`.
    ignore_since: bool,
}

impl FakeStore {
    fn with_closed(task_id: &str, closed_at: i64) -> FakeStore {
        let store = FakeStore::default();
        store
            .record_closed(&ClosedFingerprint {
                fingerprint: FP.to_string(),
                repo_root: REPO.to_string(),
                task_id: task_id.to_string(),
                closed_at,
            })
            .unwrap();
        store
    }

    fn add_open(&self, fingerprint: &str, repo_root: &str, task_id: &str) {
        self.open.lock().unwrap().push((
            fingerprint.to_string(),
            repo_root.to_string(),
            task_id.to_string(),
        ));
    }
}

impl FingerprintStore for FakeStore {
    fn record_closed(&self, closed: &ClosedFingerprint) -> PortResult<()> {
        self.closed.lock().unwrap().push(closed.clone());
        Ok(())
    }

    fn find_closed_since(
        &self,
        fingerprint: &str,
        repo_root: &str,
        since: i64,
    ) -> PortResult<Option<ClosedFingerprint>> {
        if self.fail {
            return Err(PortError::Io("db down".to_string()));
        }
        Ok(self
            .closed
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.fingerprint == fingerprint && c.repo_root == repo_root)
            .filter(|c| self.ignore_since || c.closed_at >= since)
            .max_by_key(|c| c.closed_at)
            .cloned())
    }

    fn find_open_task(&self, fingerprint: &str, repo_root: &str) -> PortResult<Option<String>> {
        if self.fail {
            return Err(PortError::Io("db down".to_string()));
        }
        Ok(self
            .open
            .lock()
            .unwrap()
            .iter()
            .find(|(fp, repo, _)| fp == fingerprint && repo == repo_root)
            .map(|(_, _, id)| id.clone()))
    }
}

fn suppressed(task_id: &str, closed_at: i64) -> DedupeDecision {
    DedupeDecision::SuppressRecentlyClosed {
        task_id: task_id.to_string(),
        closed_at,
    }
}

#[test]
fn unknown_fingerprint_creates_a_task() {
    let store = FakeStore::default();
    assert_eq!(
        decide(FP, REPO, &store, NOW).unwrap(),
        DedupeDecision::CreateTask
    );
}

#[test]
fn open_task_with_the_fingerprint_gets_the_occurrence() {
    let store = FakeStore::default();
    store.add_open(FP, REPO, "task-open");
    assert_eq!(
        decide(FP, REPO, &store, NOW).unwrap(),
        DedupeDecision::AttachToOpen {
            task_id: "task-open".to_string()
        }
    );
}

#[test]
fn open_task_wins_over_a_recent_closure() {
    let store = FakeStore::with_closed("task-old", NOW - 60);
    store.add_open(FP, REPO, "task-open");
    assert_eq!(
        decide(FP, REPO, &store, NOW).unwrap(),
        DedupeDecision::AttachToOpen {
            task_id: "task-open".to_string()
        }
    );
}

#[test]
fn closed_inside_the_window_is_suppressed() {
    let closed_at = NOW - 3600;
    let store = FakeStore::with_closed("task-old", closed_at);
    assert_eq!(
        decide(FP, REPO, &store, NOW).unwrap(),
        suppressed("task-old", closed_at)
    );
}

#[test]
fn window_is_72_hours() {
    assert_eq!(SUPPRESS_WINDOW_SECS, 259_200);
}

#[test]
fn closed_exactly_72_hours_ago_is_still_suppressed() {
    let closed_at = NOW - SUPPRESS_WINDOW_SECS;
    let store = FakeStore::with_closed("task-old", closed_at);
    assert_eq!(
        decide(FP, REPO, &store, NOW).unwrap(),
        suppressed("task-old", closed_at)
    );
}

#[test]
fn closed_one_second_past_72_hours_creates_a_task() {
    let store = FakeStore::with_closed("task-old", NOW - SUPPRESS_WINDOW_SECS - 1);
    assert_eq!(
        decide(FP, REPO, &store, NOW).unwrap(),
        DedupeDecision::CreateTask
    );
}

#[test]
fn closed_one_second_inside_72_hours_is_suppressed() {
    let closed_at = NOW - SUPPRESS_WINDOW_SECS + 1;
    let store = FakeStore::with_closed("task-old", closed_at);
    assert_eq!(
        decide(FP, REPO, &store, NOW).unwrap(),
        suppressed("task-old", closed_at)
    );
}

#[test]
fn window_does_not_rely_on_the_store_honouring_since() {
    let mut store = FakeStore::with_closed("task-old", NOW - SUPPRESS_WINDOW_SECS - 1);
    store.ignore_since = true;
    assert_eq!(
        decide(FP, REPO, &store, NOW).unwrap(),
        DedupeDecision::CreateTask
    );
}

#[test]
fn most_recent_closure_is_reported() {
    let store = FakeStore::with_closed("task-older", NOW - 7200);
    store
        .record_closed(&ClosedFingerprint {
            fingerprint: FP.to_string(),
            repo_root: REPO.to_string(),
            task_id: "task-newer".to_string(),
            closed_at: NOW - 600,
        })
        .unwrap();
    assert_eq!(
        decide(FP, REPO, &store, NOW).unwrap(),
        suppressed("task-newer", NOW - 600)
    );
}

#[test]
fn other_repo_or_other_fingerprint_does_not_match() {
    let store = FakeStore::with_closed("task-old", NOW - 60);
    store.add_open(FP, REPO, "task-open");
    assert_eq!(
        decide(FP, "/repos/other", &store, NOW).unwrap(),
        DedupeDecision::CreateTask
    );
    assert_eq!(
        decide("fp-2", REPO, &store, NOW).unwrap(),
        DedupeDecision::CreateTask
    );
}

#[test]
fn store_error_is_returned_not_treated_as_new() {
    let store = FakeStore {
        fail: true,
        ..FakeStore::default()
    };
    assert_eq!(
        decide(FP, REPO, &store, NOW),
        Err(PortError::Io("db down".to_string()))
    );
}
