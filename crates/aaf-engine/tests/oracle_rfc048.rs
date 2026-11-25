#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Oracle tests for RFC-048: family fallback stats keys (`@tool:<tool>` and
//! `@cmd:<family>`) aggregate with `max` instead of `p75`, while exact
//! `(repo_id, label)` matches retain `p75` (dropping the single top outlier
//! once 4+ samples exist).

use std::time::Duration;

use aaf_engine::throttle::Sleeper;
use aaf_engine::throttle::heavy::HeavyTool;
use aaf_engine::throttle::stats::{
    HeavySample, cmd_label, record, recorded_peak, recorded_peak_for, recorded_peak_with_tool,
    tool_label,
};
use tempfile::TempDir;

const GIB: u64 = 1024 * 1024 * 1024;

struct NoopSleeper;

impl Sleeper for NoopSleeper {
    fn sleep(&self, _duration: Duration) {}
}

fn sample(peak_rss_bytes: u64) -> HeavySample {
    HeavySample {
        peak_rss_bytes,
        duration_ms: 1_500,
    }
}

#[test]
fn cmd_family_fallback_uses_max_not_p75_when_four_light_runs_join_one_heavy_run() {
    let data = TempDir::new().unwrap();
    let sleeper = NoopSleeper;
    let repo = "repo-rfc048";

    // Simulate 4 light `cargo check` runs (1 GiB) and 1 heavy `cargo test` run (6 GiB),
    // recording both their exact labels and the shared `@cmd:cargo` family key.
    for _ in 0..4 {
        assert!(record(data.path(), repo, "cargo-check", sample(GIB), &sleeper).unwrap());
        assert!(
            record(
                data.path(),
                repo,
                &cmd_label("cargo"),
                sample(GIB),
                &sleeper
            )
            .unwrap()
        );
    }
    assert!(record(data.path(), repo, "cargo-test", sample(6 * GIB), &sleeper).unwrap());
    assert!(
        record(
            data.path(),
            repo,
            &cmd_label("cargo"),
            sample(6 * GIB),
            &sleeper,
        )
        .unwrap()
    );

    // An unmeasured custom label in the same cargo family falls back to `@cmd:cargo`
    // and must receive `max` (6 GiB), NOT `p75` (1 GiB).
    assert_eq!(
        recorded_peak_for(
            data.path(),
            repo,
            "new-unmeasured-cargo-label",
            None,
            Some("cargo"),
        ),
        Some(6 * GIB),
    );
}

#[test]
fn tool_family_fallback_uses_max_not_p75_when_four_light_runs_join_one_heavy_run() {
    let data = TempDir::new().unwrap();
    let sleeper = NoopSleeper;
    let repo = "repo-rfc048-tool";

    for _ in 0..4 {
        assert!(record(data.path(), repo, "gradle-lint", sample(GIB), &sleeper).unwrap());
        assert!(
            record(
                data.path(),
                repo,
                &tool_label(HeavyTool::Gradle),
                sample(GIB),
                &sleeper,
            )
            .unwrap()
        );
    }
    assert!(record(data.path(), repo, "gradle-test", sample(6 * GIB), &sleeper).unwrap());
    assert!(
        record(
            data.path(),
            repo,
            &tool_label(HeavyTool::Gradle),
            sample(6 * GIB),
            &sleeper,
        )
        .unwrap()
    );

    // Both `recorded_peak_for` and `recorded_peak_with_tool` must use `max` (6 GiB)
    // when falling back to `@tool:gradle`.
    assert_eq!(
        recorded_peak_for(
            data.path(),
            repo,
            "new-unmeasured-gradle-label",
            Some(HeavyTool::Gradle),
            Some("gradle"),
        ),
        Some(6 * GIB),
    );
    assert_eq!(
        recorded_peak_with_tool(
            data.path(),
            repo,
            "new-unmeasured-gradle-label",
            Some(HeavyTool::Gradle),
        ),
        Some(6 * GIB),
    );
}

#[test]
fn exact_label_match_retains_p75_dropping_single_high_outlier() {
    let data = TempDir::new().unwrap();
    let sleeper = NoopSleeper;
    let repo = "repo-rfc048-exact";

    for peak in [GIB, GIB, GIB, GIB, 6 * GIB] {
        assert!(record(data.path(), repo, "exact-build", sample(peak), &sleeper).unwrap());
        assert!(
            record(
                data.path(),
                repo,
                &cmd_label("cargo"),
                sample(peak),
                &sleeper
            )
            .unwrap()
        );
    }

    // Exact match still uses p75 (2nd highest = 1 GiB) via both entry points.
    assert_eq!(recorded_peak(data.path(), repo, "exact-build"), Some(GIB));
    assert_eq!(
        recorded_peak_for(data.path(), repo, "exact-build", None, Some("cargo")),
        Some(GIB),
    );

    // Sibling unmeasured label still gets `max` (6 GiB) from `@cmd:cargo`.
    assert_eq!(
        recorded_peak_for(data.path(), repo, "other-cargo-label", None, Some("cargo")),
        Some(6 * GIB),
    );
}

#[test]
fn unmeasured_and_zero_peak_fallback_preserve_none() {
    let data = TempDir::new().unwrap();
    let sleeper = NoopSleeper;
    let repo = "repo-rfc048-none";

    // Nothing recorded -> stays Unmeasured (`None`).
    assert_eq!(
        recorded_peak_for(data.path(), repo, "fresh", None, Some("cargo")),
        None,
    );
    assert_eq!(
        recorded_peak_for(
            data.path(),
            repo,
            "fresh",
            Some(HeavyTool::Gradle),
            Some("gradle"),
        ),
        None,
    );

    // Zero-only samples on fallback keys still count as unmeasured (`None`).
    assert!(record(data.path(), repo, &cmd_label("cargo"), sample(0), &sleeper).unwrap());
    assert_eq!(
        recorded_peak_for(data.path(), repo, "fresh", None, Some("cargo")),
        None,
    );
}
