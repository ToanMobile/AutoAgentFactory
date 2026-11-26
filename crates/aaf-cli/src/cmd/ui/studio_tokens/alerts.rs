//! Actionable Vietnamese token-burn & anomaly alerts for `/studio`.

use std::collections::{HashMap, HashSet};

use aaf_domain::budget::RootBudget;
use aaf_domain::event::EventKind;
use aaf_domain::ports::EventSink;
use aaf_domain::task::Task;
use aaf_engine::db::Db;
use serde_json::Value;

use super::{DupAuditRow, SINGLE_RUN_SPIKE_THRESHOLD, pct, ratio_ge};

const CROSS_AUDIT_EVENTS_PER_TASK: u32 = 80;
const RECENT_WINDOW_SECS: i64 = 3600;

/// Collects duplicate non-retry, non-reused post-gate `CrossAudit` groups by
/// `(task_id, tree_fp, config_hash)` using indexed per-task event queries.
pub(super) fn collect_duplicate_audits(
    db: &Db,
    task_map: &HashMap<&str, &Task>,
    audit_task_ids: &HashSet<String>,
    since: Option<i64>,
) -> Vec<DupAuditRow> {
    let mut grouped: HashMap<(String, String, String), (Vec<String>, i64)> = HashMap::new();

    for task_id in audit_task_ids {
        let events = db
            .events_for_task_by_kinds(
                task_id,
                &[EventKind::CrossAudit],
                CROSS_AUDIT_EVENTS_PER_TASK,
            )
            .unwrap_or_default();
        for ev in events {
            if since.is_some_and(|c| ev.created_at < c) {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(&ev.payload_json) else {
                continue;
            };
            if v.get("phase").and_then(Value::as_str) != Some("post_gate")
                || v.get("short_circuited").and_then(Value::as_bool) == Some(true)
                || v.get("retry").and_then(Value::as_bool) == Some(true)
                || v.get("reused_from").is_some_and(|x| !x.is_null())
            {
                continue;
            }
            let verdict = v
                .get("verdict")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if verdict.is_empty() || verdict == "auth_fallback" || verdict == "auth_wait" {
                continue;
            }
            let Some(fp) = v
                .get("tree_fp")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let cfg_hash = v
                .get("config_hash")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("")
                .to_string();
            let short_verdict: String = verdict.chars().take(90).collect();
            let entry = grouped
                .entry((task_id.clone(), fp.to_string(), cfg_hash))
                .or_insert_with(|| (Vec::new(), ev.created_at));
            entry.0.push(short_verdict);
            entry.1 = entry.1.max(ev.created_at);
        }
    }

    let mut out: Vec<DupAuditRow> = grouped
        .into_iter()
        .filter(|(_, (verdicts, _))| verdicts.len() > 1)
        .map(|((task_id, tree_fp, config_hash), (verdicts, last_at))| {
            let fail_count = verdicts
                .iter()
                .filter(|v| v.to_ascii_lowercase().starts_with("fail"))
                .count() as u64;
            let inconclusive_count = verdicts
                .iter()
                .filter(|v| v.to_ascii_lowercase().starts_with("inconclusive"))
                .count() as u64;
            let task_title = task_map
                .get(task_id.as_str())
                .map(|t| t.title.clone())
                .unwrap_or_else(|| task_id.clone());
            DupAuditRow {
                task_id,
                task_title,
                tree_fp,
                config_hash,
                count: verdicts.len() as u64,
                fail_count,
                inconclusive_count,
                last_occurred_at: last_at,
                verdicts,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.fail_count
            .cmp(&a.fail_count)
            .then_with(|| b.count.cmp(&a.count))
            .then_with(|| b.last_occurred_at.cmp(&a.last_occurred_at))
            .then_with(|| a.task_id.cmp(&b.task_id))
            .then_with(|| a.tree_fp.cmp(&b.tree_fp))
    });
    out
}

/// One structured token alert surfaced prominently on `/studio`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TokenAlert {
    pub(super) severity_rank: u8,
    pub(super) severity: &'static str,
    pub(super) code: &'static str,
    pub(super) title: String,
    pub(super) detail: String,
    pub(super) recommendation: String,
    pub(super) cli_cmd: Option<String>,
    pub(super) last_occurred_at: Option<i64>,
    pub(super) active_recent: bool,
    pub(super) task_ids: Vec<String>,
}

pub(super) struct AlertInputs<'a> {
    pub(super) now: i64,
    pub(super) supervisor_paused: bool,
    pub(super) total_tokens: u64,
    pub(super) total_runs: u64,
    pub(super) wasted_tok: u64,
    pub(super) wasted_runs: u64,
    pub(super) wasted_pct: f64,
    pub(super) wasted_last_at: Option<i64>,
    pub(super) failed_tok: u64,
    pub(super) timed_out_tok: u64,
    pub(super) timed_out_runs: u64,
    pub(super) timed_out_last_at: Option<i64>,
    pub(super) killed_tok: u64,
    pub(super) limit_hit_tok: u64,
    pub(super) zero_token_failed_runs: u64,
    pub(super) zero_token_last_at: Option<i64>,
    pub(super) unmeasured_long_runs: u64,
    pub(super) unmeasured_long_secs: u64,
    pub(super) unmeasured_long_tasks: &'a [String],
    pub(super) unmeasured_last_at: Option<i64>,
    pub(super) stale_running_runs: u64,
    pub(super) stale_running_tasks: &'a [String],
    pub(super) audit_tok: u64,
    pub(super) audit_runs: u64,
    pub(super) audit_pct: f64,
    pub(super) avg_audit_tokens: u64,
    pub(super) audit_last_at: Option<i64>,
    pub(super) tokens_last_1h: u64,
    pub(super) tokens_last_24h: u64,
    pub(super) daily_cap: u64,
    pub(super) spike_limit_1h: u64,
    pub(super) cost_paused_until: Option<i64>,
    pub(super) max_single_run: u64,
    pub(super) spike_last_at: Option<i64>,
    pub(super) top_wasted_tasks: &'a [String],
    pub(super) timeout_task_ids: &'a [String],
    pub(super) spike_run_tasks: &'a [String],
    pub(super) spike_run_tasks_1h: &'a [String],
    pub(super) dup_audits: &'a [DupAuditRow],
    pub(super) budget_rows: &'a [(&'a RootBudget, String, f64, &'static str, bool)],
}

fn fmt_tok(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

#[allow(clippy::too_many_arguments)]
fn push_alert(
    out: &mut Vec<TokenAlert>,
    severity: &'static str,
    code: &'static str,
    title: impl Into<String>,
    detail: impl Into<String>,
    recommendation: impl Into<String>,
    cli_cmd: Option<String>,
    last_occurred_at: Option<i64>,
    active_recent: bool,
    task_ids: Vec<String>,
) {
    let rank = match severity {
        "critical" => 3,
        "high" => 2,
        _ => 1,
    };
    out.push(TokenAlert {
        severity_rank: rank,
        severity,
        code,
        title: title.into(),
        detail: detail.into(),
        recommendation: recommendation.into(),
        cli_cmd,
        last_occurred_at,
        active_recent,
        task_ids,
    });
}

fn is_recent(last_at: Option<i64>, now: i64) -> bool {
    last_at.is_some_and(|ts| ts >= now.saturating_sub(RECENT_WINDOW_SECS))
}

pub(super) fn build_alerts(i: AlertInputs<'_>) -> Vec<TokenAlert> {
    let mut alerts = Vec::new();

    if i.cost_paused_until.is_some() && i.daily_cap > 0 {
        push_alert(
            &mut alerts,
            "critical",
            "cost_paused",
            "Xưởng đang tạm dừng vì chạm trần token 24h toàn máy (Cost Guard)",
            format!(
                "Toàn máy đã tiêu thụ {} token trong 24h gần nhất (trần {}).",
                fmt_tok(i.tokens_last_24h),
                fmt_tok(i.daily_cap)
            ),
            "Kiểm tra các task đốt token nhiều nhất bên dưới hoặc chờ cửa sổ 24h trượt qua.",
            Some("aaf status --all".into()),
            Some(i.now),
            true,
            Vec::new(),
        );
    } else if i.daily_cap > 0 && ratio_ge(i.tokens_last_24h, i.daily_cap, 80) {
        let p = pct(i.tokens_last_24h, i.daily_cap);
        let sev = if p >= 95.0 { "critical" } else { "high" };
        let heading = if i.tokens_last_24h >= i.daily_cap {
            format!("Đã vượt trần token 24h toàn máy ({p:.1}%)")
        } else {
            format!("Sắp chạm trần token 24h toàn máy ({p:.1}%)")
        };
        push_alert(
            &mut alerts,
            sev,
            "cost_guard_24h_near",
            heading,
            format!(
                "24h gần nhất toàn máy đã dùng {} / {} token.",
                fmt_tok(i.tokens_last_24h),
                fmt_tok(i.daily_cap)
            ),
            "Giảm số luồng song song hoặc tạm dừng các task không ưu tiên.",
            None,
            Some(i.now),
            true,
            Vec::new(),
        );
    } else if i.daily_cap == 0 && i.tokens_last_24h >= 2_000_000 {
        push_alert(
            &mut alerts,
            "medium",
            "cost_guard_disabled",
            format!(
                "Cost Guard chưa bật trần 24h (đã đốt {} toàn máy)",
                fmt_tok(i.tokens_last_24h)
            ),
            "`[unblocker.cost_guard].daily_token_cap = 0` nên xưởng sẽ không tự dừng nếu xảy ra vòng lặp đốt token.",
            "Cấu hình `daily_token_cap` và `spike_factor` trong `~/.config/autofactory/config.toml` để bảo vệ quota.",
            None,
            Some(i.now),
            true,
            Vec::new(),
        );
    }

    if i.spike_limit_1h > 0 && ratio_ge(i.tokens_last_1h, i.spike_limit_1h, 75) {
        let p = pct(i.tokens_last_1h, i.spike_limit_1h);
        let sev = if p >= 95.0 { "critical" } else { "high" };
        push_alert(
            &mut alerts,
            sev,
            "cost_guard_1h_spike",
            format!("Cảnh báo đột biến token 1h toàn máy ({p:.1}% ngưỡng freeze)"),
            format!(
                "1 giờ qua toàn máy đã đốt {} / {} token (vượt 100% sẽ tự đóng băng xưởng).",
                fmt_tok(i.tokens_last_1h),
                fmt_tok(i.spike_limit_1h)
            ),
            "Kiểm tra ngay các lượt chạy đang lặp hoặc timeout trong danh sách Top đốt token.",
            Some("aaf status --all".into()),
            Some(i.now),
            true,
            i.spike_run_tasks_1h.iter().take(4).cloned().collect(),
        );
    }

    if i.stale_running_runs > 0 {
        let (sev, detail) = if i.supervisor_paused {
            (
                "medium",
                "Supervisor đang tắt (`enabled=false`); các lượt `running` cũ này sẽ tự được reap khi bật lại Auto-Runner hoặc chạy `aaf run`.",
            )
        } else {
            (
                "high",
                "Lượt chạy kẹt ở `running` chưa ghi token vào DB (hiển thị 0 token) nhưng có thể vẫn đang tốn slot/tài nguyên.",
            )
        };
        push_alert(
            &mut alerts,
            sev,
            "stale_running_runs",
            format!(
                "{} lượt chạy bị treo ở trạng thái running quá lâu{}",
                i.stale_running_runs,
                if i.supervisor_paused {
                    " (Supervisor đang tắt)"
                } else {
                    ""
                }
            ),
            detail,
            "Mở stream log của task liên quan hoặc kiểm tra `aaf status --all` để dừng/reap lượt treo.",
            Some("aaf status --all".into()),
            Some(i.now),
            !i.supervisor_paused,
            i.stale_running_tasks.iter().take(4).cloned().collect(),
        );
    }

    let fired_high_waste = i.wasted_tok >= 50_000 && i.wasted_pct >= 15.0;
    if fired_high_waste {
        let sev = if i.wasted_pct >= 35.0 || i.wasted_tok >= 500_000 {
            "critical"
        } else if i.wasted_pct >= 20.0 || i.wasted_tok >= 200_000 {
            "high"
        } else {
            "medium"
        };
        let lh_part = if i.limit_hit_tok > 0 {
            format!(", limit_hit: {}", fmt_tok(i.limit_hit_tok))
        } else {
            String::new()
        };
        push_alert(
            &mut alerts,
            sev,
            "high_wasted_tokens",
            format!(
                "Tỷ lệ đốt token lãng phí cao: {} ({:.1}%)",
                fmt_tok(i.wasted_tok),
                i.wasted_pct
            ),
            format!(
                "{} lượt lỗi/timeout/killed đã đốt {} / {} token (failed: {}, timed_out: {}, killed: {}{lh_part}; không tính cross-audit trả FAIL hợp lệ).",
                i.wasted_runs,
                fmt_tok(i.wasted_tok),
                fmt_tok(i.total_tokens),
                fmt_tok(i.failed_tok),
                fmt_tok(i.timed_out_tok),
                fmt_tok(i.killed_tok),
            ),
            "Mở stream log của các lượt lỗi/timeout nhiều token nhất để xem agent bị lặp lệnh hay test treo.",
            None,
            i.wasted_last_at,
            is_recent(i.wasted_last_at, i.now),
            i.top_wasted_tasks.iter().take(4).cloned().collect(),
        );
    }

    if i.unmeasured_long_runs >= 2 || i.unmeasured_long_secs >= 300 {
        let recent = is_recent(i.unmeasured_last_at, i.now);
        let sev = if recent && (i.unmeasured_long_runs >= 3 || i.unmeasured_long_secs >= 900) {
            "high"
        } else {
            "medium"
        };
        push_alert(
            &mut alerts,
            sev,
            "unmeasured_token_leak",
            format!(
                "Rò rỉ token chưa hạch toán: {} lượt dài (>=30s, {}p) ghi 0/0 token",
                i.unmeasured_long_runs,
                i.unmeasured_long_secs / 60
            ),
            "Các lượt chạy lâu bị kill/timeout trước khi CLI in usage JSON vẫn tiêu thụ token thật trên API nhưng không được cộng vào ngân sách.",
            "Kiểm tra stream log và tránh restart service khi worker đang chạy; kiểm tra parser usage khi bị timeout/killed.",
            None,
            i.unmeasured_last_at,
            recent,
            i.unmeasured_long_tasks.iter().take(4).cloned().collect(),
        );
    }

    let timeout_dominates_waste = fired_high_waste && ratio_ge(i.timed_out_tok, i.wasted_tok, 70);
    if !timeout_dominates_waste
        && (i.timed_out_tok >= 50_000 || (i.timed_out_runs >= 2 && i.timed_out_tok >= 20_000))
    {
        let sev = if i.timed_out_tok >= 120_000 {
            "high"
        } else {
            "medium"
        };
        push_alert(
            &mut alerts,
            sev,
            "timeout_token_burn",
            format!(
                "{} lượt timed_out đã đốt {} token",
                i.timed_out_runs,
                fmt_tok(i.timed_out_tok)
            ),
            "Agent chạy chạm ngưỡng timeout rồi bị huỷ giữa chừng, mất toàn bộ token lượt đó.",
            "Kiểm tra scoped test (có bị fallback chạy nhầm suite nặng?) hoặc tách task lớn thành subtask nhỏ hơn.",
            None,
            i.timed_out_last_at,
            is_recent(i.timed_out_last_at, i.now),
            i.timeout_task_ids.iter().take(4).cloned().collect(),
        );
    }

    if i.audit_tok >= 80_000 && (i.audit_pct >= 25.0 || i.avg_audit_tokens >= 40_000) {
        let sev = if i.audit_pct >= 35.0 || i.avg_audit_tokens >= 50_000 {
            "high"
        } else {
            "medium"
        };
        push_alert(
            &mut alerts,
            sev,
            "high_audit_overhead",
            format!(
                "Cross-Audit chiếm {:.1}% tổng token (TB {}/lượt)",
                i.audit_pct,
                fmt_tok(i.avg_audit_tokens)
            ),
            format!(
                "{} lượt audit đã tiêu thụ {} / {} token toàn kỳ.",
                i.audit_runs,
                fmt_tok(i.audit_tok),
                fmt_tok(i.total_tokens)
            ),
            "Kiểm tra MCP thừa nạp vào phiên audit (--strict-mcp-config), diff quá lớn, hoặc audit fail lặp nhiều vòng.",
            None,
            i.audit_last_at,
            is_recent(i.audit_last_at, i.now),
            Vec::new(),
        );
    }

    if !i.dup_audits.is_empty() {
        let extra: u64 = i.dup_audits.iter().map(|d| d.count.saturating_sub(1)).sum();
        let has_dup_fail = i.dup_audits.iter().any(|d| d.fail_count >= 2);
        let sev = if has_dup_fail { "high" } else { "medium" };
        let last_dup_at = i.dup_audits.iter().map(|d| d.last_occurred_at).max();
        let tids: Vec<String> = i
            .dup_audits
            .iter()
            .take(4)
            .map(|d| d.task_id.clone())
            .collect();
        push_alert(
            &mut alerts,
            sev,
            "duplicate_post_gate_audit",
            format!(
                "Phát hiện {} cặp (task, tree_fp, config) bị Cross-Audit lặp (+{} lượt thừa)",
                i.dup_audits.len(),
                extra
            ),
            format!(
                "{} cặp có verdict fail lặp lại trên cùng mã nguồn & cấu hình (đã loại trừ auto-retry và crash-replay reused_from).",
                i.dup_audits.iter().filter(|d| d.fail_count >= 2).count()
            ),
            "Tránh restart service giữa lúc gating hoặc kiểm tra guard tái sử dụng kết quả post-gate FAIL cùng tree_fp.",
            None,
            last_dup_at,
            is_recent(last_dup_at, i.now),
            tids,
        );
    }

    let hot_budgets: Vec<_> = i
        .budget_rows
        .iter()
        .filter(|(b, _, p, _, has_open)| *has_open && (b.frozen || *p >= 70.0))
        .collect();
    if !hot_budgets.is_empty() {
        let any_crit = hot_budgets
            .iter()
            .any(|(b, _, p, _, _)| b.frozen || *p >= 90.0);
        let sev = if any_crit { "critical" } else { "high" };
        let summary = hot_budgets
            .iter()
            .take(3)
            .map(|(b, _, p, kind, _)| match *kind {
                "wall" => format!(
                    "{} ({:.0}% wall · {}s/{}s)",
                    b.root_id, p, b.wall_secs_used, b.wall_secs_limit
                ),
                "cycles" => format!(
                    "{} ({:.0}% cycles · {}/{})",
                    b.root_id, p, b.cycles_used, b.cycles_limit
                ),
                _ => format!(
                    "{} ({:.0}% · {}/{})",
                    b.root_id,
                    p,
                    fmt_tok(b.tokens_used),
                    fmt_tok(b.token_limit)
                ),
            })
            .collect::<Vec<_>>()
            .join(", ");
        push_alert(
            &mut alerts,
            sev,
            "root_budget_near_limit",
            format!(
                "{} root task đang mở sắp/đã chạm trần budget",
                hot_budgets.len()
            ),
            summary,
            "Xem lịch sử rework của các task này; nếu task quá rộng, hãy chia nhỏ hoặc điều chỉnh scope.",
            None,
            Some(i.now),
            true,
            hot_budgets
                .iter()
                .take(4)
                .map(|(b, _, _, _, _)| b.root_id.clone())
                .collect(),
        );
    }

    if i.max_single_run >= SINGLE_RUN_SPIKE_THRESHOLD {
        let sev = if i.max_single_run >= 150_000 {
            "high"
        } else {
            "medium"
        };
        push_alert(
            &mut alerts,
            sev,
            "single_run_spike",
            format!(
                "Có lượt chạy đơn lẻ đốt tới {} token",
                fmt_tok(i.max_single_run)
            ),
            format!(
                "{} task có lượt chạy vượt {} token trong kỳ.",
                i.spike_run_tasks.len(),
                fmt_tok(SINGLE_RUN_SPIKE_THRESHOLD)
            ),
            "Bấm 'Xem log' ở bảng Top lượt chạy bên dưới để kiểm tra agent có đọc thừa file lớn hay lặp tool call.",
            None,
            i.spike_last_at,
            is_recent(i.spike_last_at, i.now),
            i.spike_run_tasks.iter().take(4).cloned().collect(),
        );
    }

    if i.total_runs >= 10
        && i.zero_token_failed_runs >= 5
        && ratio_ge(i.zero_token_failed_runs, i.total_runs, 25)
    {
        push_alert(
            &mut alerts,
            "medium",
            "zero_token_failures",
            format!(
                "{} / {} lượt lỗi/killed ghi nhận 0 token",
                i.zero_token_failed_runs, i.total_runs
            ),
            "Các lượt chết sớm (<30s) thường do auth/mạng/sandbox; lượt chạy lâu (>=30s) ghi 0 token được tách riêng ở cảnh báo rò rỉ token.",
            "Kiểm tra aaf-ui.log / stream log để phát hiện lỗi auth, sandbox, hoặc bị kill khi restart service.",
            None,
            i.zero_token_last_at,
            is_recent(i.zero_token_last_at, i.now),
            Vec::new(),
        );
    }

    alerts
}
