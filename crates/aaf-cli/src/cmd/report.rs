//! `aaf report [--since HOURS] [--all]`: what the factory finished in a
//! period, what it cost, and how long each lifecycle stage took. Read-only.

use std::fmt::Write as _;

use aaf_domain::ports::Clock;
use aaf_engine::factory::adapters::SystemClock;
use aaf_engine::telemetry::{
    CostRates, DurationStats, FactoryReport, ReportWindow, TaskCounts, collect_report,
};
use serde_json::json;

use super::board::store::{err, open_db_read_only, repo_root};
use crate::ctx::Ctx;
use crate::output::{CmdOutput, CmdResult};

#[derive(Debug, clap::Args)]
pub struct ReportArgs {
    /// Only tasks, runs and stage stays from the last N hours (default: all history).
    #[arg(long, value_name = "HOURS")]
    pub since: Option<u64>,
    /// Report across every repository instead of the current one.
    #[arg(long)]
    pub all: bool,
}

pub fn run(args: &ReportArgs, ctx: &Ctx) -> CmdResult {
    let repo = if args.all {
        None
    } else {
        repo_root(ctx)
            .ok()
            .map(|path| path.to_string_lossy().into_owned())
    };
    let now = SystemClock.now();
    let window = ReportWindow {
        since: args.since.map(|h| since_cutoff(now, h)),
        now,
    };
    let report = match open_db_read_only(ctx)? {
        Some(db) => collect_report(&db, repo.as_deref(), window, CostRates::default())
            .map_err(|e| err("report", e))?,
        None => empty_report(window),
    };
    let scope = repo.clone().unwrap_or_else(|| "all".to_string());
    let text = render_text(&report, &scope, args.since);
    let value = serde_json::to_value(&report).map_err(|e| err("report", e))?;
    Ok(CmdOutput::ok("ok", text)
        .field("scope", json!(scope))
        .field("report", value))
}

pub(crate) fn since_cutoff(now: i64, hours: u64) -> i64 {
    let secs = i64::try_from(hours.saturating_mul(3600)).unwrap_or(i64::MAX);
    now.saturating_sub(secs)
}

pub(crate) fn empty_report(window: ReportWindow) -> FactoryReport {
    FactoryReport {
        window,
        counts: TaskCounts::default(),
        tasks_done: 0,
        lead_time: DurationStats::default(),
        stages: Vec::new(),
        executors: Vec::new(),
        total_tokens_in: 0,
        total_tokens_out: 0,
        estimated_cost_usd: 0.0,
        top_blocked: Vec::new(),
        autonomy: Default::default(),
    }
}

/// `90s`, `12m`, `3h05m`, `2d04h`.
pub(crate) fn human_secs(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60),
        _ => format!("{}d{:02}h", secs / 86_400, (secs % 86_400) / 3600),
    }
}

fn pct(value: Option<f64>) -> String {
    value.map_or_else(|| "n/a".to_string(), |v| format!("{v:.1}%"))
}

pub(crate) fn render_text(report: &FactoryReport, scope: &str, since: Option<u64>) -> String {
    let mut out = String::new();
    let period = since.map_or_else(|| "all history".to_string(), |h| format!("last {h}h"));
    let c = &report.counts;
    let _ = writeln!(out, "Factory report — {scope} ({period})");
    let _ = writeln!(
        out,
        "Tasks: {} touched, {} done, {} blocked, {} in progress",
        c.total,
        report.tasks_done,
        c.blocked,
        c.ready + c.running + c.gating + c.review
    );
    if report.lead_time.samples > 0 {
        let l = &report.lead_time;
        let _ = writeln!(
            out,
            "Lead time (created → done): p50 {}  p90 {}  max {}",
            human_secs(l.p50_secs),
            human_secs(l.p90_secs),
            human_secs(l.max_secs)
        );
    }
    let a = &report.autonomy;
    let _ = writeln!(
        out,
        "Autonomy: {} without human / {} done ({})  gate first pass {}",
        a.done_without_human,
        a.done_tasks,
        pct(a.autonomy_rate_pct),
        pct(a.gate_first_pass_rate_pct)
    );
    let _ = writeln!(
        out,
        "Cost: ~${:.2} ({} tokens in / {} out)",
        report.estimated_cost_usd, report.total_tokens_in, report.total_tokens_out
    );

    if !report.stages.is_empty() {
        let _ = writeln!(out, "\nStage time:");
        let _ = writeln!(
            out,
            "  {:<10} {:>6} {:>8} {:>8} {:>8} {:>5}",
            "stage", "stays", "p50", "p90", "max", "now"
        );
        for s in &report.stages {
            let d = &s.durations;
            let show = |secs: u64| {
                if d.samples == 0 {
                    "—".to_string()
                } else {
                    human_secs(secs)
                }
            };
            let _ = writeln!(
                out,
                "  {:<10} {:>6} {:>8} {:>8} {:>8} {:>5}",
                s.stage,
                d.samples,
                show(d.p50_secs),
                show(d.p90_secs),
                show(d.max_secs),
                s.open_now
            );
        }
    }
    if !report.executors.is_empty() {
        let _ = writeln!(out, "\nExecutors:");
        for e in &report.executors {
            let _ = writeln!(
                out,
                "  {:<10} {} runs ({} ok, {} failed, {} running)  {}  ~${:.2}",
                e.executor,
                e.runs,
                e.succeeded,
                e.failed,
                e.running,
                human_secs(e.wall_secs),
                e.estimated_cost_usd
            );
        }
    }
    if !report.top_blocked.is_empty() {
        let list: Vec<String> = report
            .top_blocked
            .iter()
            .map(|(kind, n)| format!("{kind} ({n})"))
            .collect();
        let _ = writeln!(out, "\nBlocked by: {}", list.join(", "));
    }
    if c.total == 0 {
        let _ = writeln!(out, "\nNo task in this period yet: run `aaf run` first.");
    }
    out.trim_end().to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use aaf_engine::telemetry::{ExecutorStat, StageStat};

    #[test]
    fn human_secs_picks_a_readable_unit() {
        assert_eq!(human_secs(0), "0s");
        assert_eq!(human_secs(59), "59s");
        assert_eq!(human_secs(125), "2m");
        assert_eq!(human_secs(3600 + 5 * 60), "1h05m");
        assert_eq!(human_secs(2 * 86_400 + 4 * 3600), "2d04h");
    }

    #[test]
    fn since_cutoff_saturates() {
        assert_eq!(since_cutoff(10_000, 1), 6_400);
        assert_eq!(since_cutoff(10, u64::MAX), 10 - i64::MAX);
    }

    #[test]
    fn empty_report_says_how_to_get_data() {
        let report = empty_report(ReportWindow {
            since: None,
            now: 0,
        });
        let text = render_text(&report, "/repo", None);
        assert!(text.contains("Factory report — /repo (all history)"));
        assert!(text.contains("No task in this period yet"));
        assert!(!text.contains("Stage time"));
    }

    #[test]
    fn full_report_lists_stages_executors_and_blocks() {
        let mut report = empty_report(ReportWindow {
            since: Some(0),
            now: 0,
        });
        report.counts.total = 3;
        report.counts.blocked = 1;
        report.tasks_done = 2;
        report.lead_time = DurationStats::from_samples(vec![600, 7200]);
        report.stages.push(StageStat {
            stage: "running".into(),
            durations: DurationStats::from_samples(vec![300]),
            open_now: 1,
        });
        report.executors.push(ExecutorStat {
            executor: "claude".into(),
            runs: 2,
            succeeded: 1,
            failed: 1,
            running: 0,
            tokens_in: 10,
            tokens_out: 5,
            wall_secs: 400,
            estimated_cost_usd: 1.25,
        });
        report.top_blocked.push(("infra".into(), 1));
        let text = render_text(&report, "/repo", Some(24));
        assert!(text.contains("(last 24h)"));
        assert!(text.contains("3 touched, 2 done, 1 blocked"));
        assert!(text.contains("p50 10m"));
        assert!(text.contains("running"));
        assert!(text.contains("claude     2 runs (1 ok, 1 failed, 0 running)  6m  ~$1.25"));
        assert!(text.contains("Blocked by: infra (1)"));
        assert!(!text.contains("No task in this period"));
    }
}
