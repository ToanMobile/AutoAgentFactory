//! DevKit-standard acceptance & handover renderers (HTML with CSP + verified
//! base64 PNG, and Markdown with B10 3-line summary, Root Cause, 5-item
//! acceptance report, Automated QA Suite table, 7 Golden Standards, and Proof).

use std::fmt::Write as _;
use std::path::Path;

use crate::handover::{HandoverData, HandoverGateCommand, SOLO_DEV_BADGE};
use crate::handover_html::{
    CSP_META, escape_html, format_scores_line, load_verified_proof_png, standard_name,
};

fn format_epoch_utc(epoch: i64) -> String {
    if epoch <= 0 {
        return "n/a".to_string();
    }
    let days = epoch.div_euclid(86_400);
    let secs_of_day = epoch.rem_euclid(86_400);
    let hour = secs_of_day / 3_600;
    let minute = (secs_of_day % 3_600) / 60;
    let second = secs_of_day % 60;
    // Howard Hinnant civil-from-days algorithm (UTC, proleptic Gregorian).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC ({epoch})")
}

fn gate_cmd_outcome(cmd: &HandoverGateCommand) -> (&'static str, &'static str) {
    if !cmd.timed_out && cmd.exit_code == Some(0) {
        ("PASS", "ok")
    } else if cmd.timed_out {
        ("TIMEOUT", "warn")
    } else {
        ("FAIL", "fail")
    }
}

fn md_cell(s: &str) -> String {
    s.replace('|', "\\|").replace(['\r', '\n'], " ")
}

/// Renders a standalone, offline-safe DevKit-standard HTML handover report.
pub(crate) fn render_devkit_html(data: &HandoverData, data_dir: &Path) -> String {
    let mut html = String::with_capacity(8192);
    let is_ok = data.status == "XONG";
    let (cls, status_pill) = if is_ok {
        ("ok", format!("✔ {}", data.status))
    } else {
        ("warn", format!("⚠ {}", data.status))
    };
    let verdict = if data.verdict_label.is_empty() {
        if is_ok {
            "PASS — ĐỦ ĐIỀU KIỆN NGHIỆM THU & BÀN GIAO"
        } else {
            "CHƯA ĐẠT — CÒN THIẾU ĐIỀU KIỆN NGHIỆM THU"
        }
    } else {
        &data.verdict_label
    };
    let _ = writeln!(
        html,
        "<!DOCTYPE html>\n<html lang=\"vi\">\n<head>\n  <meta charset=\"utf-8\">\n  \
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n  {CSP_META}\n  \
         <title>Báo Cáo Nghiệm Thu — {} ({})</title>\n{INLINE_STYLE}</head>\n<body>\n<main class=\"wrap\">",
        escape_html(&data.task_id),
        escape_html(&data.status)
    );
    let _ = writeln!(
        html,
        "  <header class=\"hero\">\n    \
         <div class=\"hero-top\">\n      \
         <span class=\"eyebrow\">Báo Cáo Nghiệm Thu &amp; Kiểm Toán Chất Lượng</span>\n      \
         <span class=\"badge {cls}\">{}</span>\n    \
         </div>\n    \
         <h1>{}</h1>\n    \
         <p class=\"verdict {cls}\">{}</p>\n    \
         <div class=\"meta-grid\">\n      \
         <div><span class=\"k\">Task ID</span><code>{}</code></div>\n      \
         <div><span class=\"k\">Dự án</span><code>{}</code></div>\n      \
         <div><span class=\"k\">Trạng thái</span><code>{}</code></div>\n      \
         <div><span class=\"k\">Tree FP</span><code>{}</code></div>\n      \
         <div><span class=\"k\">Thời gian</span><code>{}</code></div>\n    \
         </div>",
        escape_html(&status_pill),
        escape_html(&data.title),
        escape_html(verdict),
        escape_html(&data.task_id),
        escape_html(&data.repo_root),
        escape_html(&data.state),
        escape_html(data.tree_fp.as_deref().unwrap_or("n/a")),
        escape_html(&format_epoch_utc(data.generated_at)),
    );
    if data.same_account_warning {
        let _ = writeln!(
            html,
            "    <p class=\"solo-badge\">{}</p>",
            escape_html(SOLO_DEV_BADGE)
        );
    }
    let _ = writeln!(html, "  </header>");

    render_b10_summary_html(&mut html, data, cls);
    if !data.missing.is_empty() {
        let _ = writeln!(
            html,
            "  <section class=\"card alert\"><h2>Missing / Blocking</h2><ul>"
        );
        for item in &data.missing {
            let _ = writeln!(html, "    <li>{}</li>", escape_html(item));
        }
        let _ = writeln!(html, "  </ul></section>");
    }
    render_acceptance_html(&mut html, data);
    render_qa_suite_html(&mut html, data);
    render_scoring_html(&mut html, data);
    if !data.reviewers.is_empty() {
        html.push_str(&crate::handover_dual::render_dual_reviewers_html(
            &data.reviewers,
            data.adjudicated,
        ));
    }
    render_proof_html(&mut html, data, data_dir);
    let _ = writeln!(html, "</main>\n</body>\n</html>");
    html
}

fn render_b10_summary_html(html: &mut String, data: &HandoverData, cls: &str) {
    let outcome = if data.user_outcome.is_empty() {
        &data.title
    } else {
        &data.user_outcome
    };
    let gate_line = if data.gate_proof_next_line.is_empty() {
        &data.prevent_old_bug
    } else {
        &data.gate_proof_next_line
    };
    let _ = writeln!(
        html,
        "  <section class=\"card b10\"><h2>Tóm Tắt 3 Dòng (Chuẩn B10 DevKit)</h2>\n    \
         <div class=\"b10-lines\">\n      \
         <p class=\"b10-l1\"><span class=\"badge {cls}\">{}</span></p>\n      \
         <p class=\"b10-l2\">{}</p>\n      \
         <p class=\"b10-l3\"><code>{}</code></p>\n    \
         </div>\n  </section>",
        escape_html(&data.status),
        escape_html(outcome),
        escape_html(gate_line),
    );
}

fn render_acceptance_html(html: &mut String, data: &HandoverData) {
    let _ = writeln!(
        html,
        "  <section class=\"card\"><h2>Báo Cáo Nghiệm Thu 5 Mục (Acceptance Report)</h2><ol class=\"acceptance\">"
    );
    let _ = writeln!(
        html,
        "    <li><strong>1. Đã làm gì / Đã fix gì:</strong> {}<div class=\"root-cause\"><strong>Nguyên nhân:</strong> {}</div></li>",
        escape_html(&data.what_done),
        escape_html(if data.root_cause.is_empty() {
            "Theo yêu cầu task"
        } else {
            &data.root_cause
        }),
    );
    for (label, value) in [
        ("2. Chặn bug cũ", &data.prevent_old_bug),
        ("3. Nguy cơ bug mới", &data.new_bug_risk),
        ("4. An toàn mã nguồn", &data.code_safety),
        ("5. Còn hở / chưa kiểm", &data.unverified),
    ] {
        let _ = writeln!(
            html,
            "    <li><strong>{}:</strong> {}</li>",
            escape_html(label),
            escape_html(value)
        );
    }
    let _ = writeln!(html, "  </ol></section>");
}

fn render_qa_suite_html(html: &mut String, data: &HandoverData) {
    let _ = writeln!(
        html,
        "  <section class=\"card\"><h2>Danh Sách Tiêu Chí Nghiệm Thu — Automated QA Suite <span class=\"tag\">[tự chạy]</span></h2>"
    );
    if data.gate_commands.is_empty() {
        let _ = writeln!(
            html,
            "    <p class=\"meta\">Tóm tắt gate: {}</p>\n  </section>",
            escape_html(&data.prevent_old_bug)
        );
        return;
    }
    let _ = writeln!(
        html,
        "    <table><thead><tr><th>Kiểm toán / Tên</th><th>Lệnh</th><th>Thời lượng</th><th>Kết quả</th><th>Nguồn</th></tr></thead><tbody>"
    );
    for cmd in &data.gate_commands {
        let (label, badge_cls) = gate_cmd_outcome(cmd);
        let _ = writeln!(
            html,
            "      <tr><td><strong>{}</strong></td><td><code>{}</code></td><td>{}ms</td><td><span class=\"badge {badge_cls}\">{label}</span></td><td>[tự chạy]</td></tr>",
            escape_html(&cmd.name),
            escape_html(&cmd.command),
            cmd.duration_ms
        );
    }
    let _ = writeln!(html, "    </tbody></table>\n  </section>");
}

fn render_scoring_html(html: &mut String, data: &HandoverData) {
    let (mech, llm, eff) = format_scores_line(data);
    let _ = writeln!(
        html,
        "  <section class=\"card\"><h2>Quality &amp; 7 Golden Standards</h2>\
         <p class=\"meta\">MechanicalScore: <strong>{}</strong> &middot; LLM Avg: <strong>{}</strong> &middot; Effective: <strong>{}</strong></p>",
        escape_html(&mech),
        escape_html(&llm),
        escape_html(&eff)
    );
    if !data.scores.is_empty() {
        let _ = writeln!(
            html,
            "    <table><thead><tr><th>ID</th><th>Standard</th><th>Score</th><th>Source</th><th>Deduction</th></tr></thead><tbody>"
        );
        for s in &data.scores {
            let reason = if s.deduction_reason.trim().is_empty() {
                "—"
            } else {
                &s.deduction_reason
            };
            let _ = writeln!(
                html,
                "      <tr><td><code>{}</code></td><td>{}</td><td>{}/100</td><td>[agent báo]</td><td>{}</td></tr>",
                escape_html(&s.id),
                escape_html(standard_name(&s.id)),
                s.score,
                escape_html(reason)
            );
        }
        let _ = writeln!(html, "    </tbody></table>");
    }
    let _ = writeln!(html, "  </section>");
}

fn render_proof_html(html: &mut String, data: &HandoverData, data_dir: &Path) {
    let _ = writeln!(
        html,
        "  <section class=\"card\"><h2>Minh Chứng Hình Ảnh Nghiệm Thu (Verification Proof)</h2>"
    );
    let Some(proof) = &data.proof else {
        let msg = if data.proof_required {
            "Missing verified device proof (proof.required = true)."
        } else {
            "No device proof attached."
        };
        let _ = writeln!(
            html,
            "    <p class=\"meta\">{}</p>\n  </section>",
            escape_html(msg)
        );
        return;
    };
    let _ = writeln!(
        html,
        "    <p class=\"meta\">Device: <code>{}</code> &middot; Summary: {} &middot; SHA-256: <code>{}</code> &middot; Captured: <code>{}</code></p>",
        escape_html(&proof.device_serial),
        escape_html(&proof.summary),
        escape_html(&proof.image_sha256),
        proof.captured_at
    );
    match load_verified_proof_png(proof, data, data_dir) {
        Ok(b64) => {
            let _ = writeln!(
                html,
                "    <figure class=\"proof\"><img src=\"data:image/png;base64,{b64}\" alt=\"Verified proof screenshot\"></figure>"
            );
        }
        Err(reason) => {
            let _ = writeln!(
                html,
                "    <p class=\"notice\">Proof image omitted: {}</p>",
                escape_html(&reason)
            );
        }
    }
    let _ = writeln!(html, "  </section>");
}

/// Renders [`HandoverData`] as DevKit-standard GitHub-flavored Markdown.
pub(crate) fn render_devkit_markdown(data: &HandoverData) -> String {
    let mut out = String::with_capacity(2048);
    let outcome = if data.user_outcome.is_empty() {
        &data.title
    } else {
        &data.user_outcome
    };
    let gate_line = if data.gate_proof_next_line.is_empty() {
        &data.prevent_old_bug
    } else {
        &data.gate_proof_next_line
    };
    // B10 3-line summary block at the top.
    let _ = writeln!(out, "{}\n{}\n{}\n", data.status, outcome, gate_line);
    let _ = writeln!(
        out,
        "# {} — {} (`{}`)\n",
        data.status, data.title, data.task_id
    );
    let verdict = if data.verdict_label.is_empty() {
        if data.status == "XONG" {
            "PASS — ĐỦ ĐIỀU KIỆN NGHIỆM THU & BÀN GIAO"
        } else {
            "CHƯA ĐẠT — CÒN THIẾU ĐIỀU KIỆN NGHIỆM THU"
        }
    } else {
        &data.verdict_label
    };
    let _ = writeln!(
        out,
        "- State: `{}` | Tree: `{}` | Verdict: **{}**",
        data.state,
        data.tree_fp.as_deref().unwrap_or("n/a"),
        verdict
    );
    if data.same_account_warning {
        let _ = writeln!(out, "- Warning: **{SOLO_DEV_BADGE}**");
    }
    if !data.missing.is_empty() {
        let _ = writeln!(out, "\n## Missing / Blocking\n");
        for item in &data.missing {
            let _ = writeln!(out, "- {item}");
        }
    }
    let root_cause = if data.root_cause.is_empty() {
        "Theo yêu cầu task"
    } else {
        &data.root_cause
    };
    let _ = writeln!(
        out,
        "\n## Acceptance Report\n\n\
         1. **Đã làm gì / Đã fix gì:** {}\n\
         \x20  - **Nguyên nhân:** {}\n\
         2. **Chặn bug cũ:** {}\n\
         3. **Nguy cơ bug mới:** {}\n\
         4. **An toàn mã nguồn:** {}\n\
         5. **Còn hở / chưa kiểm:** {}",
        data.what_done,
        root_cause,
        data.prevent_old_bug,
        data.new_bug_risk,
        data.code_safety,
        data.unverified
    );
    if !data.gate_commands.is_empty() {
        let _ = writeln!(
            out,
            "\n## Automated QA Suite [tự chạy]\n\n\
             | Kiểm toán / Tên | Lệnh | Thời lượng | Kết quả | Nguồn |\n\
             |---|---|---|---|---|"
        );
        for cmd in &data.gate_commands {
            let (label, _) = gate_cmd_outcome(cmd);
            let _ = writeln!(
                out,
                "| {} | `{}` | {}ms | **{}** | [tự chạy] |",
                md_cell(&cmd.name),
                md_cell(&cmd.command).replace('`', "'"),
                cmd.duration_ms,
                label
            );
        }
    }
    let (mech, llm, eff) = format_scores_line(data);
    let _ = writeln!(
        out,
        "\n## Scores\n\n- MechanicalScore: {mech} | LLM Average: {llm} | Effective: {eff}"
    );
    if !data.scores.is_empty() {
        let _ = writeln!(
            out,
            "\n| ID | Standard | Score | Source | Deduction |\n|---|---|---|---|---|"
        );
        for s in &data.scores {
            let reason = if s.deduction_reason.trim().is_empty() {
                "—"
            } else {
                &s.deduction_reason
            };
            let _ = writeln!(
                out,
                "| `{}` | {} | {}/100 | [agent báo] | {} |",
                s.id,
                standard_name(&s.id),
                s.score,
                md_cell(reason)
            );
        }
    }
    if !data.reviewers.is_empty() {
        let adj = if data.adjudicated {
            " (adjudicated)"
        } else {
            ""
        };
        let _ = writeln!(out, "\n### Dual-Review Reviewers{adj}\n");
        for r in &data.reviewers {
            let model = r.model.trim();
            let em = if model.is_empty() {
                r.executor.clone()
            } else {
                format!("{}/{model}", r.executor)
            };
            let llm_str = r
                .llm_average
                .map_or_else(|| "n/a".to_string(), |v| format!("{v}/100"));
            let _ = writeln!(
                out,
                "- `{}` (`{}`): **{}** (P0/P1/P2: {}/{}/{}, LLM Avg: {})",
                r.id, em, r.verdict, r.p0, r.p1, r.p2, llm_str
            );
        }
    }
    if let Some(p) = &data.proof {
        let _ = writeln!(
            out,
            "\n## Verification Proof\n\n- Tool: `{}`\n- Device: `{}`\n- Summary: {}\n- Image SHA-256: `{}`\n- Tree: `{}`",
            p.source_tool, p.device_serial, p.summary, p.image_sha256, p.tree_fp
        );
    }
    out
}

const INLINE_STYLE: &str = "  <style>\n\
    :root{color-scheme:dark light;--bg:#0d1117;--card:#161b22;--border:#30363d;--fg:#e6edf3;--muted:#8b949e;--code:#21262d;\
    --ok-bg:#122d1e;--ok-fg:#3fb950;--warn-bg:#2e2208;--warn-fg:#d29922;--fail-bg:#331418;--fail-fg:#f85149;--accent:#58a6ff}\n\
    @media(prefers-color-scheme:light){:root{--bg:#f6f8fa;--card:#ffffff;--border:#d0d7de;--fg:#1f2328;--muted:#59636e;--code:#eff1f3;\
    --ok-bg:#dafbe1;--ok-fg:#1a7f37;--warn-bg:#fff8c5;--warn-fg:#9a6700;--fail-bg:#ffebe9;--fail-fg:#cf222e;--accent:#0969da}}\n\
    *{box-sizing:border-box}body{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Helvetica,Arial,sans-serif;margin:0;padding:24px;\
    background:var(--bg);color:var(--fg);line-height:1.55}\n\
    .wrap{max-width:980px;margin:0 auto}.hero{background:var(--card);border:1px solid var(--border);border-radius:12px;padding:20px 24px;margin-bottom:16px}\n\
    .hero-top{display:flex;justify-content:space-between;align-items:center;gap:12px;flex-wrap:wrap}\n\
    .eyebrow{font-size:.82rem;text-transform:uppercase;letter-spacing:.06em;color:var(--accent);font-weight:700}\n\
    h1{margin:10px 0 8px;font-size:1.45rem;line-height:1.3}h2{margin:0 0 12px;font-size:1.05rem}\n\
    .verdict{margin:0 0 14px;padding:8px 12px;border-radius:8px;font-weight:600;font-size:.92rem}\n\
    .verdict.ok{background:var(--ok-bg);color:var(--ok-fg)}.verdict.warn{background:var(--warn-bg);color:var(--warn-fg)}\n\
    .meta-grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(190px,1fr));gap:10px;padding-top:10px;border-top:1px solid var(--border)}\n\
    .meta-grid .k{display:block;font-size:.75rem;color:var(--muted);margin-bottom:2px}\n\
    .badge{display:inline-block;padding:3px 10px;border-radius:999px;font-weight:700;font-size:.8rem}\n\
    .badge.ok{background:var(--ok-bg);color:var(--ok-fg)}.badge.warn{background:var(--warn-bg);color:var(--warn-fg)}\n\
    .badge.fail{background:var(--fail-bg);color:var(--fail-fg)}\n\
    .solo-badge,.notice{background:var(--warn-bg);color:var(--warn-fg);padding:8px 12px;border-radius:8px;font-size:.88rem}\n\
    .solo-badge{display:inline-block;margin:12px 0 0}\n\
    .card{background:var(--card);border:1px solid var(--border);border-radius:12px;padding:18px 22px;margin-bottom:16px}\n\
    .card.alert{border-color:var(--warn-fg)}.card.b10{border-left:4px solid var(--accent)}\n\
    .b10-lines p{margin:6px 0}.b10-l2{font-weight:600}.b10-l3 code{display:block;padding:8px 10px;white-space:pre-wrap;word-break:break-word}\n\
    .acceptance{padding-left:0;list-style:none;margin:0}.acceptance li{padding:8px 0;border-bottom:1px solid var(--border)}\n\
    .acceptance li:last-child{border-bottom:none}.root-cause{margin-top:6px;padding:6px 10px;border-left:3px solid var(--accent);background:var(--code);border-radius:4px;font-size:.92rem}\n\
    .meta{color:var(--muted);font-size:.9rem;margin:4px 0 10px}.tag{font-size:.78rem;color:var(--muted);font-weight:500}\n\
    table{width:100%;border-collapse:collapse;margin-top:10px;font-size:.88rem}\n\
    th,td{text-align:left;padding:8px 10px;border-bottom:1px solid var(--border);vertical-align:top}th{color:var(--muted);font-weight:600}\n\
    code{background:var(--code);padding:2px 6px;border-radius:4px;font-size:.85em;word-break:break-word}\n\
    .proof{margin:12px 0 0}.proof img{max-width:100%;height:auto;border:1px solid var(--border);border-radius:8px}\n\
  </style>\n";
