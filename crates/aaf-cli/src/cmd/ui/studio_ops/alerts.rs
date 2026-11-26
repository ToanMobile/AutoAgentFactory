//! Actionable 4-pillar operational alerts for `/health` (legacy `/studio`) and `/aaf_heath`.

use aaf_engine::telemetry::AutonomyMetrics;
use serde_json::{Value, json};

use super::REWORK_CHURN_MIN_CYCLES;

const RECENT_WINDOW_SECS: i64 = 3600;
const DRAIN_DEADLOCK_AGE_SECS: u64 = 60;
const DRAIN_DEADLOCK_WAIT_SECS: u64 = 30;
const QUEUE_WARN_SECS: u64 = 180;
const QUEUE_HIGH_SECS: u64 = 300;
const GATE_AVG_WARN_SECS: u64 = 180;
const GATE_AVG_HIGH_SECS: u64 = 420;
const TOKENS_PER_DONE_WARN: u64 = 450_000;
const ZERO_DONE_BURN_WARN: u64 = 800_000;

pub(super) struct AlertFacts<'a> {
    pub now: i64,
    pub autonomy: &'a AutonomyMetrics,
    pub clear_zero_count: u64,
    pub clear_zero_tasks: &'a [String],
    pub clear_zero_last_at: Option<i64>,
    pub vacuous_warns: u64,
    pub vacuous_last_at: Option<i64>,
    pub churn_labels: &'a [String],
    pub churn_ids: &'a [String],
    pub dead_orphan_slots: &'a [String],
    pub draining: bool,
    pub drain_age_secs: u64,
    pub active_slots: usize,
    pub waiters: usize,
    pub oldest_wait_secs: u64,
    pub stalled_workers: &'a [Value],
    pub gate_avg_secs: Option<u64>,
    pub done_window: u64,
    pub total_tokens: u64,
    pub tokens_per_done: Option<u64>,
}

fn is_recent(now: i64, last_at: Option<i64>) -> bool {
    last_at.is_some_and(|t| now.saturating_sub(t) <= RECENT_WINDOW_SECS)
}

#[allow(clippy::too_many_arguments)]
fn push_alert(
    alerts: &mut Vec<Value>,
    severity: &str,
    category: &str,
    code: &str,
    title: String,
    detail: String,
    task_ids: Vec<String>,
    active_recent: bool,
    last_occurred_at: Option<i64>,
) {
    alerts.push(json!({
        "severity": severity,
        "category": category,
        "code": code,
        "title": title,
        "detail": detail,
        "task_ids": task_ids,
        "active_recent": active_recent,
        "last_occurred_at": last_occurred_at,
    }));
}

pub(super) fn collect_ops_alerts(f: &AlertFacts<'_>) -> Vec<Value> {
    let mut alerts = Vec::new();
    let aut = f.autonomy;

    if f.clear_zero_count > 0 {
        push_alert(
            &mut alerts,
            "high",
            "quality",
            "clear_gate_with_mechanical_score_zero",
            format!(
                "{} gate CLEAR nhưng mechanical_score = 0",
                f.clear_zero_count
            ),
            "Kiểm tra proof_verified / trigger_globs để tránh vòng lặp audit_rework.".into(),
            f.clear_zero_tasks.to_vec(),
            is_recent(f.now, f.clear_zero_last_at),
            f.clear_zero_last_at,
        );
    }
    if aut.gate_tasks_evaluated >= 3
        && let Some(rate) = aut.gate_first_pass_rate_pct
    {
        if rate < 40.0 {
            push_alert(
                &mut alerts,
                "high",
                "quality",
                "low_first_pass_gate",
                format!(
                    "Tỷ lệ đạt Gate ngay lượt đầu rất thấp: {rate:.1}% ({}/{})",
                    aut.gate_first_pass_clear, aut.gate_tasks_evaluated
                ),
                "Xem gate_failure_taxonomy và tối ưu prompt/pre-check của worker.".into(),
                Vec::new(),
                true,
                None,
            );
        } else if rate < 70.0 {
            push_alert(
                &mut alerts,
                "medium",
                "quality",
                "low_first_pass_gate",
                format!(
                    "Tỷ lệ đạt Gate lượt đầu dưới 70%: {rate:.1}% ({}/{})",
                    aut.gate_first_pass_clear, aut.gate_tasks_evaluated
                ),
                "Cần xem nhóm lỗi Gate lặp lại để nâng tỷ lệ pass lượt đầu.".into(),
                Vec::new(),
                true,
                None,
            );
        }
    }
    if aut.cross_audit_total >= 3
        && let Some(fail_rate) = aut.cross_audit_fail_rate_pct
        && fail_rate > 45.0
    {
        push_alert(
            &mut alerts,
            "medium",
            "quality",
            "high_cross_audit_fail_rate",
            format!(
                "Tỷ lệ Cross-Audit fail cao: {fail_rate:.1}% ({}/{})",
                aut.cross_audit_failed, aut.cross_audit_total
            ),
            "Kiểm tra rubric audit hoặc tiêu chí spec để tránh rework thừa.".into(),
            Vec::new(),
            true,
            None,
        );
    }
    if f.vacuous_warns >= 3 {
        push_alert(
            &mut alerts,
            "medium",
            "quality",
            "vacuous_test_warnings",
            format!("{} lượt gate có cảnh báo vacuous test", f.vacuous_warns),
            "Test mới vẫn pass khi revert code prod — cần rà chất lượng oracle test.".into(),
            Vec::new(),
            is_recent(f.now, f.vacuous_last_at),
            f.vacuous_last_at,
        );
    }
    if !f.churn_labels.is_empty() {
        push_alert(
            &mut alerts,
            "medium",
            "quality",
            "high_rework_churn",
            format!(
                "{} task đang mở lặp >= {REWORK_CHURN_MIN_CYCLES} chu kỳ rework",
                f.churn_labels.len()
            ),
            f.churn_labels.join(", "),
            f.churn_ids.to_vec(),
            true,
            None,
        );
    }
    if !f.dead_orphan_slots.is_empty() {
        push_alert(
            &mut alerts,
            "high",
            "speed",
            "dead_orphan_heavy_slots",
            format!(
                "Phát hiện {} heavy slot mồ côi đã chết",
                f.dead_orphan_slots.len()
            ),
            f.dead_orphan_slots.join(", "),
            Vec::new(),
            true,
            None,
        );
    }
    if f.draining
        && f.active_slots == 0
        && (f.drain_age_secs >= DRAIN_DEADLOCK_AGE_SECS
            || (f.waiters > 0 && f.oldest_wait_secs >= DRAIN_DEADLOCK_WAIT_SECS))
    {
        push_alert(
            &mut alerts,
            "high",
            "speed",
            "heavy_slot_drain_deadlock",
            format!(
                "drain.lock đang giữ {}s trong khi active_slots = 0",
                f.drain_age_secs
            ),
            "Nguy cơ deadlock hàng đợi build nặng giữa guest sandbox và host.".into(),
            Vec::new(),
            true,
            None,
        );
    }
    if f.waiters > 0 && f.oldest_wait_secs >= QUEUE_HIGH_SECS {
        push_alert(
            &mut alerts,
            "high",
            "speed",
            "heavy_slot_queue_stalled",
            format!(
                "Hàng đợi heavy slot tắc: {} tiến trình chờ (lâu nhất {}s)",
                f.waiters, f.oldest_wait_secs
            ),
            "Kiểm tra lệnh build nặng đang giữ slot hoặc tăng độ hẹp test scoped.".into(),
            Vec::new(),
            true,
            None,
        );
    } else if f.waiters > 0 && f.oldest_wait_secs >= QUEUE_WARN_SECS {
        push_alert(
            &mut alerts,
            "medium",
            "speed",
            "heavy_slot_queue_slow",
            format!(
                "Hàng đợi heavy slot chậm: {} tiến trình chờ (lâu nhất {}s)",
                f.waiters, f.oldest_wait_secs
            ),
            "Nhiều build nặng xếp hàng song song.".into(),
            Vec::new(),
            true,
            None,
        );
    }
    if !f.stalled_workers.is_empty() {
        let has_high = f
            .stalled_workers
            .iter()
            .any(|w| w.get("severity").and_then(Value::as_str) == Some("high"));
        let tids: Vec<String> = f
            .stalled_workers
            .iter()
            .filter_map(|w| w.get("task_id").and_then(Value::as_str).map(str::to_string))
            .collect();
        let max_idle = f
            .stalled_workers
            .first()
            .and_then(|w| w.get("idle_secs"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        push_alert(
            &mut alerts,
            if has_high { "high" } else { "medium" },
            "speed",
            "stalled_worker",
            format!(
                "{} worker đang chạy im lặng không xuất log (lâu nhất {}p)",
                f.stalled_workers.len(),
                max_idle / 60
            ),
            "Kiểm tra stream log hoặc tiến trình con đang chờ heavy slot / mạng.".into(),
            tids,
            true,
            None,
        );
    }
    if let Some(avg_s) = f.gate_avg_secs
        && aut.gate_tasks_evaluated >= 2
    {
        if avg_s > GATE_AVG_HIGH_SECS {
            push_alert(
                &mut alerts,
                "high",
                "speed",
                "slow_gate_avg",
                format!("Thời gian Gate trung bình rất chậm: {avg_s}s (> 7p)"),
                "Tối ưu scripts/aaf_scoped_test.sh hoặc bật cache build.".into(),
                Vec::new(),
                true,
                None,
            );
        } else if avg_s > GATE_AVG_WARN_SECS {
            push_alert(
                &mut alerts,
                "medium",
                "speed",
                "slow_gate_avg",
                format!("Thời gian Gate trung bình chậm: {avg_s}s (> 3p)"),
                "Kiểm tra độ hẹp scoped test và tranh chấp heavy slot.".into(),
                Vec::new(),
                true,
                None,
            );
        }
    }
    if let Some(tpd) = f.tokens_per_done
        && tpd > TOKENS_PER_DONE_WARN
    {
        push_alert(
            &mut alerts,
            "medium",
            "efficiency",
            "high_tokens_per_done",
            format!("Chi phí token/task done cao: {tpd} token/task (> 450k)"),
            "Kiểm tra các task rework nhiều vòng hoặc diff quá lớn cần tách nhỏ.".into(),
            Vec::new(),
            true,
            None,
        );
    } else if f.done_window == 0 && f.total_tokens > ZERO_DONE_BURN_WARN {
        push_alert(
            &mut alerts,
            "medium",
            "efficiency",
            "high_burn_zero_done",
            format!(
                "Đã đốt {} token trong kỳ nhưng chưa hoàn thành task nào",
                f.total_tokens
            ),
            "Kiểm tra xem các task có đang bị kẹt gate/audit hoặc chờ tài khoản.".into(),
            Vec::new(),
            true,
            None,
        );
    }

    alerts
}
