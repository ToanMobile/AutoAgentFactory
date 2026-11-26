//! Actionable 4-pillar operational alerts for `/health` (legacy `/studio`) and `/aaf_heath`.

use aaf_engine::telemetry::AutonomyMetrics;
use serde_json::{Value, json};

use super::probes::AccountPoolSummary;

const RECENT_WINDOW_SECS: i64 = 3600;
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
    pub churn_severe: bool,
    pub dead_orphan_slots: &'a [String],
    pub draining: bool,
    pub active_slots: usize,
    pub waiters: usize,
    pub oldest_wait_secs: u64,
    pub stalled_workers: &'a [Value],
    pub stuck_gating_tasks: &'a [Value],
    pub gating_spin_tasks: &'a [Value],
    pub long_running_workers: &'a [Value],
    pub looping_workers: &'a [Value],
    pub stale_review_tasks: &'a [Value],
    pub dag_bottlenecks: &'a [Value],
    pub account_pool: &'a AccountPoolSummary,
    pub open_ready_or_gating: usize,
    pub open_gating_count: usize,
    pub gate_avg_secs: Option<u64>,
    pub done_window: u64,
    pub total_tokens: u64,
    pub tokens_per_done: Option<u64>,
}

fn is_recent(now: i64, last_at: Option<i64>) -> bool {
    last_at.is_some_and(|t| now.saturating_sub(t) <= RECENT_WINDOW_SECS)
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("?")
}

fn u64_of(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn ids_of(items: &[Value], key: &str) -> Vec<String> {
    items
        .iter()
        .filter_map(|v| v.get(key).and_then(Value::as_str).map(str::to_string))
        .collect()
}

fn has_high(items: &[Value]) -> bool {
    items
        .iter()
        .any(|v| v.get("severity").and_then(Value::as_str) == Some("high"))
}

fn push_live(
    alerts: &mut Vec<Value>,
    sev: &str,
    cat: &str,
    code: &str,
    title: String,
    detail: String,
    task_ids: Vec<String>,
) {
    alerts.push(json!({
        "severity": sev, "category": cat, "code": code,
        "title": title, "detail": detail, "task_ids": task_ids,
        "active_recent": true, "last_occurred_at": null,
    }));
}

#[allow(clippy::too_many_arguments)]
fn push_timed(
    alerts: &mut Vec<Value>,
    sev: &str,
    cat: &str,
    code: &str,
    title: String,
    detail: String,
    task_ids: Vec<String>,
    now: i64,
    last_at: Option<i64>,
) {
    alerts.push(json!({
        "severity": sev, "category": cat, "code": code,
        "title": title, "detail": detail, "task_ids": task_ids,
        "active_recent": is_recent(now, last_at), "last_occurred_at": last_at,
    }));
}

fn worker_snippet(w: &Value) -> String {
    let tid = str_of(w, "task_id");
    let tool = w
        .get("last_tool")
        .and_then(Value::as_str)
        .or_else(|| w.get("last_type").and_then(Value::as_str))
        .unwrap_or("no-stream");
    let target = w.get("last_target").and_then(Value::as_str).unwrap_or("");
    if target.is_empty() {
        format!("{tid} [{tool}]")
    } else {
        format!("{tid} [{tool}: {target}]")
    }
}

fn collect_quality_alerts(f: &AlertFacts<'_>, alerts: &mut Vec<Value>) {
    let aut = f.autonomy;
    if f.clear_zero_count > 0 {
        push_timed(
            alerts,
            "high",
            "quality",
            "clear_gate_with_mechanical_score_zero",
            format!(
                "{} gate CLEAR nhưng mechanical_score = 0",
                f.clear_zero_count
            ),
            "Kiểm tra proof_verified / trigger_globs để tránh vòng lặp audit_rework.".into(),
            f.clear_zero_tasks.to_vec(),
            f.now,
            f.clear_zero_last_at,
        );
    }
    if aut.gate_tasks_evaluated >= 3
        && let Some(rate) = aut.gate_first_pass_rate_pct
        && rate < 70.0
    {
        let (sev, msg, hint) = if rate < 40.0 {
            (
                "high",
                format!(
                    "Tỷ lệ đạt Gate ngay lượt đầu rất thấp: {rate:.1}% ({}/{})",
                    aut.gate_first_pass_clear, aut.gate_tasks_evaluated
                ),
                "Xem gate_failure_taxonomy và tối ưu prompt/pre-check của worker.",
            )
        } else {
            (
                "medium",
                format!(
                    "Tỷ lệ đạt Gate lượt đầu dưới 70%: {rate:.1}% ({}/{})",
                    aut.gate_first_pass_clear, aut.gate_tasks_evaluated
                ),
                "Cần xem nhóm lỗi Gate lặp lại để nâng tỷ lệ pass lượt đầu.",
            )
        };
        push_live(
            alerts,
            sev,
            "quality",
            "low_first_pass_gate",
            msg,
            hint.into(),
            Vec::new(),
        );
    }
    if aut.cross_audit_total >= 3
        && let Some(fail_rate) = aut.cross_audit_fail_rate_pct
        && fail_rate > 45.0
    {
        push_live(
            alerts,
            "medium",
            "quality",
            "high_cross_audit_fail_rate",
            format!(
                "Tỷ lệ Cross-Audit fail cao: {fail_rate:.1}% ({}/{})",
                aut.cross_audit_failed, aut.cross_audit_total
            ),
            "Kiểm tra rubric audit hoặc tiêu chí spec để tránh rework thừa.".into(),
            Vec::new(),
        );
    }
    if f.vacuous_warns >= 3 {
        push_timed(
            alerts,
            "medium",
            "quality",
            "vacuous_test_warnings",
            format!("{} lượt gate có cảnh báo vacuous test", f.vacuous_warns),
            "Test mới vẫn pass khi revert code prod — cần rà chất lượng oracle test.".into(),
            Vec::new(),
            f.now,
            f.vacuous_last_at,
        );
    }
    if !f.churn_labels.is_empty() {
        push_live(
            alerts,
            if f.churn_severe { "high" } else { "medium" },
            "quality",
            "high_rework_churn",
            format!(
                "{} task đang mở lặp rework/audit quá nhiều vòng",
                f.churn_labels.len()
            ),
            f.churn_labels.join(", "),
            f.churn_ids.to_vec(),
        );
    }
}

fn collect_speed_and_slot_alerts(f: &AlertFacts<'_>, alerts: &mut Vec<Value>) {
    let ap = f.account_pool;
    let prov_exhausted = ap
        .by_provider
        .iter()
        .any(|p| u64_of(p, "total") > 0 && u64_of(p, "available") == 0);
    if (prov_exhausted && f.open_ready_or_gating > 0) || ap.waiting_for_account > 0 {
        let sev = if f.open_gating_count > 0 || ap.waiting_for_account >= 3 {
            "high"
        } else {
            "medium"
        };
        let mut parts: Vec<String> = ap
            .by_provider
            .iter()
            .map(|p| {
                format!(
                    "{}: {}/{} rảnh (bận={}, cd={})",
                    str_of(p, "provider"),
                    u64_of(p, "available"),
                    u64_of(p, "total"),
                    u64_of(p, "leased"),
                    u64_of(p, "cooling")
                )
            })
            .collect();
        if !ap.waiting_repos.is_empty() {
            parts.push(format!(
                "đang chờ tài khoản: {}",
                ap.waiting_repos.join(", ")
            ));
        }
        push_live(
            alerts,
            sev,
            "speed",
            "account_pool_exhausted",
            format!(
                "Cạn tài khoản chạy ({} rảnh/{}, {} task chờ tài khoản)",
                ap.total_available, ap.total_accounts, ap.waiting_for_account
            ),
            parts.join(" · "),
            Vec::new(),
        );
    }
    if !f.dead_orphan_slots.is_empty() {
        push_live(
            alerts,
            "high",
            "speed",
            "dead_orphan_heavy_slots",
            format!(
                "Phát hiện {} heavy slot mồ côi đã chết",
                f.dead_orphan_slots.len()
            ),
            f.dead_orphan_slots.join(", "),
            Vec::new(),
        );
    }
    // Require live waiters >= 30s with 0 active slots (avoid false positives on persistent drain.lock mtime).
    if f.draining
        && f.active_slots == 0
        && f.waiters > 0
        && f.oldest_wait_secs >= DRAIN_DEADLOCK_WAIT_SECS
    {
        push_live(
            alerts,
            "high",
            "speed",
            "heavy_slot_drain_deadlock",
            format!(
                "drain.lock đang giữ với active_slots = 0 trong khi {} tiến trình chờ {}s",
                f.waiters, f.oldest_wait_secs
            ),
            "Nguy cơ deadlock hàng đợi build nặng giữa guest sandbox và host.".into(),
            Vec::new(),
        );
    }
    if f.waiters > 0 && f.oldest_wait_secs >= QUEUE_WARN_SECS {
        let is_hi = f.oldest_wait_secs >= QUEUE_HIGH_SECS;
        let (sev, code, title, detail) = if is_hi {
            (
                "high",
                "heavy_slot_queue_stalled",
                format!(
                    "Hàng đợi heavy slot tắc: {} tiến trình chờ (lâu nhất {}s)",
                    f.waiters, f.oldest_wait_secs
                ),
                "Kiểm tra lệnh build nặng đang giữ slot hoặc tăng độ hẹp test scoped.",
            )
        } else {
            (
                "medium",
                "heavy_slot_queue_slow",
                format!(
                    "Hàng đợi heavy slot chậm: {} tiến trình chờ (lâu nhất {}s)",
                    f.waiters, f.oldest_wait_secs
                ),
                "Nhiều build nặng xếp hàng song song.",
            )
        };
        push_live(alerts, sev, "speed", code, title, detail.into(), Vec::new());
    }
    if !f.looping_workers.is_empty() {
        let detail: Vec<String> = f
            .looping_workers
            .iter()
            .take(3)
            .map(|w| {
                format!(
                    "{} (×{} {})",
                    str_of(w, "task_id"),
                    u64_of(w, "repeat_count"),
                    w.get("signature").and_then(Value::as_str).unwrap_or("")
                )
            })
            .collect();
        push_live(
            alerts,
            "high",
            "speed",
            "worker_command_loop",
            format!(
                "{} worker đang lặp cùng một lệnh/lỗi liên tục",
                f.looping_workers.len()
            ),
            detail.join("; "),
            ids_of(f.looping_workers, "task_id"),
        );
    }
    if !f.stalled_workers.is_empty() {
        let max_idle = f
            .stalled_workers
            .first()
            .map_or(0, |w| u64_of(w, "idle_secs"));
        let detail: Vec<String> = f
            .stalled_workers
            .iter()
            .take(3)
            .map(worker_snippet)
            .collect();
        push_live(
            alerts,
            if has_high(f.stalled_workers) {
                "high"
            } else {
                "medium"
            },
            "speed",
            "stalled_worker",
            format!(
                "{} worker đang chạy im lặng không xuất log (lâu nhất {}p)",
                f.stalled_workers.len(),
                max_idle / 60
            ),
            detail.join(" · "),
            ids_of(f.stalled_workers, "task_id"),
        );
    }
    if !f.stuck_gating_tasks.is_empty() {
        let max_m = f
            .stuck_gating_tasks
            .first()
            .map_or(0, |g| u64_of(g, "age_secs"))
            / 60;
        let causes: Vec<String> = f
            .stuck_gating_tasks
            .iter()
            .take(4)
            .map(|g| {
                format!(
                    "{} ({}p, nguyên nhân: {})",
                    str_of(g, "task_id"),
                    u64_of(g, "age_secs") / 60,
                    str_of(g, "cause")
                )
            })
            .collect();
        push_live(
            alerts,
            if has_high(f.stuck_gating_tasks) {
                "high"
            } else {
                "medium"
            },
            "speed",
            "stuck_gating_tasks",
            format!(
                "{} task bị kẹt ở trạng thái gating (lâu nhất {}p)",
                f.stuck_gating_tasks.len(),
                max_m
            ),
            causes.join(" · "),
            ids_of(f.stuck_gating_tasks, "task_id"),
        );
    }
    if !f.gating_spin_tasks.is_empty() {
        let max_cnt = f
            .gating_spin_tasks
            .first()
            .map_or(0, |g| u64_of(g, "gate_count_30m"));
        push_live(
            alerts,
            "high",
            "speed",
            "gating_spin_loop",
            format!(
                "{} task đang lặp gate liên tục trong 30p (tối đa {} lượt) mà không thoát gating",
                f.gating_spin_tasks.len(),
                max_cnt
            ),
            "Task pass gate lặp lại nhưng không chạy được Cross-Audit hoặc không land được.".into(),
            ids_of(f.gating_spin_tasks, "task_id"),
        );
    }
    if !f.long_running_workers.is_empty() {
        let max_m = f
            .long_running_workers
            .first()
            .map_or(0, |w| u64_of(w, "age_secs"))
            / 60;
        let detail: Vec<String> = f
            .long_running_workers
            .iter()
            .take(3)
            .map(worker_snippet)
            .collect();
        push_live(
            alerts,
            if has_high(f.long_running_workers) {
                "high"
            } else {
                "medium"
            },
            "speed",
            "long_running_worker",
            format!(
                "{} worker chạy kéo dài bất thường (lâu nhất {}p)",
                f.long_running_workers.len(),
                max_m
            ),
            detail.join(" · "),
            ids_of(f.long_running_workers, "task_id"),
        );
    }
    if !f.dag_bottlenecks.is_empty() {
        let has_non_human_high = f.dag_bottlenecks.iter().any(|b| {
            let is_human = b.get("blocker_reason").and_then(Value::as_str) == Some("needs_human");
            !is_human && u64_of(b, "blocked_count") >= 4
        });
        let desc: Vec<String> = f
            .dag_bottlenecks
            .iter()
            .take(3)
            .map(|b| {
                let tag = match b.get("blocker_reason").and_then(Value::as_str) {
                    Some(r) => format!("{}:{r}", str_of(b, "blocker_state")),
                    None => str_of(b, "blocker_state").to_string(),
                };
                format!(
                    "{} [{tag}] đang chặn {} task",
                    str_of(b, "blocker_id"),
                    u64_of(b, "blocked_count")
                )
            })
            .collect();
        push_live(
            alerts,
            if has_non_human_high { "high" } else { "medium" },
            "speed",
            "dag_upstream_bottleneck",
            format!(
                "{} nút cổ chai DAG đang chặn các task Ready phía sau",
                f.dag_bottlenecks.len()
            ),
            desc.join(" · "),
            ids_of(f.dag_bottlenecks, "blocker_id"),
        );
    }
    if !f.stale_review_tasks.is_empty() {
        let max_h = f
            .stale_review_tasks
            .first()
            .map_or(0, |r| u64_of(r, "age_secs"))
            / 3600;
        push_live(
            alerts,
            "medium",
            "speed",
            "stale_review_tasks",
            format!(
                "{} task ở trạng thái review quá 4 giờ (lâu nhất {}h)",
                f.stale_review_tasks.len(),
                max_h
            ),
            "Cần duyệt / merge PR hoặc `aaf board approve` để giải phóng hàng đợi.".into(),
            ids_of(f.stale_review_tasks, "task_id"),
        );
    }
    if let Some(avg_s) = f.gate_avg_secs
        && f.autonomy.gate_tasks_evaluated >= 2
        && avg_s > GATE_AVG_WARN_SECS
    {
        let (sev, title, detail) = if avg_s > GATE_AVG_HIGH_SECS {
            (
                "high",
                format!("Thời gian Gate trung bình rất chậm: {avg_s}s (> 7p)"),
                "Tối ưu scripts/aaf_scoped_test.sh hoặc bật cache build.",
            )
        } else {
            (
                "medium",
                format!("Thời gian Gate trung bình chậm: {avg_s}s (> 3p)"),
                "Kiểm tra độ hẹp scoped test và tranh chấp heavy slot.",
            )
        };
        push_live(
            alerts,
            sev,
            "speed",
            "slow_gate_avg",
            title,
            detail.into(),
            Vec::new(),
        );
    }
}

pub(super) fn collect_ops_alerts(f: &AlertFacts<'_>) -> Vec<Value> {
    let mut alerts = Vec::new();
    collect_quality_alerts(f, &mut alerts);
    collect_speed_and_slot_alerts(f, &mut alerts);
    if let Some(tpd) = f.tokens_per_done
        && tpd > TOKENS_PER_DONE_WARN
    {
        push_live(
            &mut alerts,
            "medium",
            "efficiency",
            "high_tokens_per_done",
            format!("Chi phí token/task done cao: {tpd} token/task (> 450k)"),
            "Kiểm tra các task rework nhiều vòng hoặc diff quá lớn cần tách nhỏ.".into(),
            Vec::new(),
        );
    } else if f.done_window == 0 && f.total_tokens > ZERO_DONE_BURN_WARN {
        push_live(
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
        );
    }
    alerts
}
