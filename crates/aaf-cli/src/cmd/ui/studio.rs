//! Data behind the `/studio` page of `aaf ui`: the period report and the
//! doctor checks, so every number is one page away instead of one command.

use aaf_domain::ports::Clock;
use aaf_engine::db::Db;
use aaf_engine::telemetry::{CostPolicy, ReportWindow, collect_report};
use serde_json::{Value, json};

use crate::cmd::doctor::{DoctorArgs, run as run_doctor};
use crate::cmd::report::since_cutoff;
use crate::ctx::{Ctx, SystemClock};

/// The `/studio` link next to a pairing URL (`http://h:p/#token=…`).
pub(crate) fn studio_url(pairing_url: &str) -> String {
    pairing_url.replacen("/#token=", "/studio#token=", 1)
}

/// `cost` prices API-billed accounts only; subscription runs report tokens.
pub(crate) fn report_json(
    db: Option<&Db>,
    repo: Option<&str>,
    since_hours: Option<u64>,
    cost: &CostPolicy,
) -> Result<Value, String> {
    let now = SystemClock.now();
    let window = ReportWindow {
        since: since_hours.map(|h| since_cutoff(now, h)),
        now,
    };
    let report = match db {
        Some(db) => collect_report(db, repo, window, cost).map_err(|e| format!("report: {e}"))?,
        None => crate::cmd::report::empty_report(window),
    };
    let value = serde_json::to_value(&report).map_err(|e| format!("report: {e}"))?;
    Ok(
        json!({ "ok": true, "scope": repo.unwrap_or("all"), "since_hours": since_hours, "report": value }),
    )
}

pub(crate) fn doctor_json(ctx: &Ctx) -> Result<Value, String> {
    let out = run_doctor(&DoctorArgs {}, ctx).map_err(|e| e.message)?;
    let mut object = out.fields;
    object.insert("ok".into(), json!(true));
    object.insert("healthy".into(), json!(out.status == "ok"));
    object.insert("status".into(), json!(out.status));
    Ok(Value::Object(object))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn studio_url_keeps_the_token_fragment() {
        assert_eq!(
            studio_url("http://127.0.0.1:4317/#token=abc"),
            "http://127.0.0.1:4317/studio#token=abc"
        );
    }

    #[test]
    fn report_without_a_database_is_empty() {
        let v = report_json(None, Some("/repo"), Some(24), &CostPolicy::default()).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["scope"], "/repo");
        assert_eq!(v["since_hours"], 24);
        assert_eq!(v["report"]["counts"]["total"], 0);
        assert!(v["report"]["window"]["since"].is_i64());
    }
}
