#![allow(clippy::unwrap_used, clippy::expect_used)]

use aaf_domain::blocked::BlockedReason;
use aaf_domain::task::{ImmutableSpec, Priority};
use serde_json::json;

use super::*;

fn task(id: &str, state: TaskState, created: i64, updated: i64) -> Task {
    let spec = ImmutableSpec {
        description: "d".into(),
        acceptance: vec![],
    };
    let mut t = Task::new_root(id, "/repo", id, spec, Priority::P2, created);
    t.state = state;
    t.updated_at = updated;
    t
}

fn moved(task_id: &str, to: &str, at: i64) -> FactoryEvent {
    FactoryEvent::new(
        EventKind::TaskStateChanged,
        json!({"to": to, "why": "test"}).to_string(),
        at,
    )
    .for_task(task_id)
}

fn run(task_id: &str, exec: &str, status: RunStatus, start: i64, end: Option<i64>) -> RunRecord {
    RunRecord {
        id: format!("r-{task_id}-{start}"),
        task_id: task_id.into(),
        account_id: None,
        executor: exec.into(),
        worktree_path: "/wt".into(),
        branch: "b".into(),
        status,
        exit_code: None,
        tokens_in: 1_000_000,
        tokens_out: 100_000,
        started_at: start,
        ended_at: end,
    }
}

fn all(now: i64) -> ReportWindow {
    ReportWindow { since: None, now }
}

#[test]
fn percentiles_use_nearest_rank() {
    let stats = DurationStats::from_samples(vec![50, 10, 40, 20, 30]);
    assert_eq!(stats.samples, 5);
    assert_eq!(stats.total_secs, 150);
    assert_eq!(stats.p50_secs, 30);
    assert_eq!(stats.p90_secs, 50);
    assert_eq!(stats.max_secs, 50);
    assert_eq!(DurationStats::from_samples(vec![7]).p90_secs, 7);
    assert_eq!(
        DurationStats::from_samples(vec![]),
        DurationStats::default()
    );
}

#[test]
fn a_finished_task_gives_stage_times_lead_time_and_done_count() {
    let t = task("t1", TaskState::Done, 100, 1000);
    let events = vec![
        moved("t1", "ready", 110),
        moved("t1", "running", 160),
        moved("t1", "gating", 460),
        moved("t1", "review", 520),
        moved("t1", "done", 1000),
    ];
    let report = compute_report(&[t], &[], &events, all(2000), CostRates::default());
    assert_eq!(report.tasks_done, 1);
    assert_eq!(report.lead_time.max_secs, 900);
    let by_stage: HashMap<&str, u64> = report
        .stages
        .iter()
        .map(|s| (s.stage.as_str(), s.durations.total_secs))
        .collect();
    assert_eq!(by_stage[QUEUED_STAGE], 10);
    assert_eq!(by_stage["ready"], 50);
    assert_eq!(by_stage["running"], 300);
    assert_eq!(by_stage["gating"], 60);
    assert_eq!(by_stage["review"], 480);
    assert!(!by_stage.contains_key("done"), "terminal state has no stay");
    let order: Vec<&str> = report.stages.iter().map(|s| s.stage.as_str()).collect();
    assert_eq!(order, ["queued", "ready", "running", "gating", "review"]);
    assert_eq!(report.autonomy.done_tasks, 1);
}

#[test]
fn a_stage_still_in_progress_is_open_not_a_sample() {
    let t = task("t1", TaskState::Running, 100, 200);
    let events = vec![moved("t1", "ready", 150), moved("t1", "running", 200)];
    let report = compute_report(&[t], &[], &events, all(9999), CostRates::default());
    let running = report.stages.iter().find(|s| s.stage == "running").unwrap();
    assert_eq!(running.durations.samples, 0);
    assert_eq!(running.open_now, 1);
    assert_eq!(report.tasks_done, 0);
    assert_eq!(report.lead_time, DurationStats::default());
}

#[test]
fn the_window_drops_old_tasks_old_runs_and_old_stays() {
    let old = task("old", TaskState::Done, 10, 20);
    let mut fresh = task("new", TaskState::Blocked, 50, 500);
    fresh.blocked_reason = Some(BlockedReason::Infra { detail: "x".into() });
    let events = vec![
        moved("old", "done", 20),
        moved("new", "running", 60),
        moved("new", "blocked", 500),
    ];
    let runs = vec![
        run("old", "claude", RunStatus::Succeeded, 12, Some(18)),
        run("new", "codex", RunStatus::Failed, 60, Some(90)),
        run("new", "codex", RunStatus::Failed, 300, Some(400)),
    ];
    let window = ReportWindow {
        since: Some(100),
        now: 1000,
    };
    let report = compute_report(&[old, fresh], &runs, &events, window, CostRates::default());
    assert_eq!(report.counts.total, 1);
    assert_eq!(report.counts.blocked, 1);
    assert_eq!(report.tasks_done, 0);
    assert_eq!(report.top_blocked, vec![("infra".to_string(), 1)]);
    // Only the codex run that started inside the window counts.
    assert_eq!(report.executors.len(), 1);
    let codex = &report.executors[0];
    assert_eq!((codex.runs, codex.failed, codex.wall_secs), (1, 1, 100));
    // The `running` stay started at 60, before the window: not a sample.
    assert!(report.stages.iter().all(|s| s.stage != "running"));
}

#[test]
fn executors_sum_tokens_cost_and_outcomes() {
    let t = task("t1", TaskState::Review, 0, 50);
    let runs = vec![
        run("t1", "claude", RunStatus::Succeeded, 0, Some(30)),
        run("t1", "claude", RunStatus::TimedOut, 40, Some(100)),
        run("t1", "agy", RunStatus::Running, 90, None),
    ];
    let report = compute_report(&[t], &runs, &[], all(120), CostRates::default());
    let names: Vec<&str> = report
        .executors
        .iter()
        .map(|e| e.executor.as_str())
        .collect();
    assert_eq!(names, ["agy", "claude"]);
    let claude = &report.executors[1];
    assert_eq!((claude.runs, claude.succeeded, claude.failed), (2, 1, 1));
    assert_eq!(claude.wall_secs, 90);
    // 2M in at $3/M + 0.2M out at $15/M.
    assert!((claude.estimated_cost_usd - 9.0).abs() < 1e-9);
    let agy = &report.executors[0];
    assert_eq!((agy.running, agy.wall_secs), (1, 30));
    assert_eq!(report.total_tokens_in, 3_000_000);
    assert!((report.estimated_cost_usd - 13.5).abs() < 1e-9);
}

#[test]
fn malformed_or_foreign_events_are_ignored() {
    let t = task("t1", TaskState::Ready, 0, 10);
    let mut bad = moved("t1", "nonsense", 5);
    bad.payload_json = "not json".into();
    let events = vec![bad, moved("other", "running", 6), moved("t1", "ready", 10)];
    let report = compute_report(&[t], &[], &events, all(20), CostRates::default());
    let queued = report
        .stages
        .iter()
        .find(|s| s.stage == QUEUED_STAGE)
        .unwrap();
    assert_eq!(queued.durations.total_secs, 10);
    let ready = report.stages.iter().find(|s| s.stage == "ready").unwrap();
    assert_eq!(ready.open_now, 1);
}

#[test]
fn top_blocked_is_most_frequent_first_and_capped() {
    let mut map = BTreeMap::new();
    for (k, n) in [("a", 1), ("b", 3), ("c", 2), ("d", 1), ("e", 1), ("f", 5)] {
        map.insert(k.to_string(), n);
    }
    let top = top_blocked(map);
    let keys: Vec<&str> = top.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["f", "b", "c", "a", "d"]);
}

#[test]
fn collect_report_reads_the_database() {
    use aaf_domain::ports::EventSink;
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("aaf.db")).unwrap();
    let mut t = task("t1", TaskState::Backlog, 100, 100);
    t.repo_root = "/repo".into();
    db.insert_task(&t).unwrap();
    db.set_state("t1", TaskState::Ready, None, 130).unwrap();
    db.emit(&moved("t1", "ready", 130)).unwrap();
    let report = collect_report(&db, Some("/repo"), all(200), CostRates::default()).unwrap();
    assert_eq!(report.counts.ready, 1);
    let queued = report
        .stages
        .iter()
        .find(|s| s.stage == QUEUED_STAGE)
        .unwrap();
    assert_eq!(queued.durations.total_secs, 30);
    let other = collect_report(&db, Some("/elsewhere"), all(200), CostRates::default()).unwrap();
    assert_eq!(other.counts.total, 0);
}
