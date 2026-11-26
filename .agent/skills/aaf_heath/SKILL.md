---
name: aaf_heath
description: >-
  Bật, chạy 1 lần, hoặc tắt bộ giám sát sức khỏe (health monitor) định kỳ mỗi 5 phút
  cho AutoAgentFactory (AAF) trên GeelyEx2, OfficeReader và các repo đang auto-run:
  kiểm tra `aaf status --all`, DB tasks/runs/events, supervisor logs, Cockpit UI,
  phát hiện rò rỉ bộ nhớ (RSS/FD trend, orphan worker, WAL/worktree/disk bloat) và
  rò rỉ token (token spike, unmeasured 0/0 token leak, duplicate cross-audit, audit ratio,
  churn loop, root budget), tự chẩn đoán & sửa lỗi AAF nếu phát hiện factory bug,
  và báo cáo gọn trạng thái board. Kích hoạt khi gõ `/aaf_heath`, `/aaf_health`,
  `/aaf-health`, "monitor aaf", "bật monitor 5 phút aaf", "theo dõi board aaf",
  "check sức khỏe aaf", hoặc "tắt monitor aaf".
---

# AAF Board Health, Memory-Leak & Token-Leak Monitor (`/aaf_heath`)

Skill giám sát toàn diện sức khỏe board AutoAgentFactory (`aaf`), phát hiện sớm **rò rỉ bộ nhớ (RAM/FD/worktree/WAL)** và **rò rỉ đốt token (unmeasured burn, spike, audit trùng, vòng lặp churn)** định kỳ mỗi 5 phút (hoặc chạy 1 lần theo yêu cầu).

> [!CAUTION]
> - Tuyệt đối **KHÔNG** đụng vào `/Volumes/Data/Toan/agent-workbench` (repo đã blacklisted).
> - Tuyệt đối **KHÔNG** tự ý `git commit`, `git push`, hoặc `gh pr create` nếu người dùng chưa yêu cầu tường minh.
> - Tuyệt đối **KHÔNG** dùng `sleep` trong shell để vòng lặp chờ — chỉ dùng tool `schedule`.
> - Tuyệt đối **KHÔNG** tự ý bật `ui-supervisor` nếu `ui-supervisor.json` đang đặt `"enabled": false`.

---

## 1. Xác định chế độ chạy

| Ý định người dùng | Hành động |
|---|---|
| Mặc định (`/aaf_heath`, "bật monitor 5 phút") | Chạy kiểm tra ngay lập tức 1 lần → kiểm tra `manage_task(Action="list")` để tránh trùng → tạo cron `*/5 * * * *` (`IsDaemon: true`). |
| Kiểm tra nhanh 1 lần (`/aaf_heath once`, "check 1 lần") | Chỉ chạy script kiểm tra và báo cáo, **không** tạo cron. |
| Tắt lịch monitor (`/aaf_heath stop`, "tắt monitor") | Gọi `manage_task(Action="list")` tìm task cron có `[AAF Board Auto-Monitor Tick]` → gọi `manage_task(Action="kill", TaskId=...)`. |

---

## 2. Chạy snapshot sức khỏe & rò rỉ trong 1 lệnh duy nhất

```bash
bash ~/.gemini/config/skills/aaf_heath/scripts/aaf_health_check.sh
```

*(Đường dẫn trực tiếp trong repo: `bash /Volumes/Data/Toan/AutoAgentFactory/.agent/skills/aaf_heath/scripts/aaf_health_check.sh`.)*

Script **đồng bộ trực tiếp với cùng backend `/api/report?since=24` của `http://127.0.0.1:4317/studio`** (tự đọc token từ `~/.local/share/autofactory/ui-service.token`, không in lộ token; tự kiểm tra `since_hours == 24`; tự fallback sang SQLite nếu UI chết) **và** lưu lịch sử vòng tròn atomic (`fcntl.flock` + `os.replace`) tối đa 288 mẫu (24h @ 5 phút) tại `~/.cache/aaf-health/metrics.jsonl` để phát hiện **xu hướng tăng theo thời gian (trend/delta)** ở tầng OS:

### A. Đồng bộ 100% chẩn đoán Token & Runtime từ `/studio` (`/api/report?since=24`)
- Nhận trọn `token_diagnostics` & `alerts` kèm cờ **`active_recent`** (chỉ đẩy `ANOMALY:` cho cảnh báo đang xảy ra gần đây / điều kiện live; cảnh báo lịch sử 24h đã sửa chỉ ghi `INFO:` chống alert fatigue):
  - `studio:high_wasted_tokens`, `studio:unmeasured_token_leak` (lượt `>= 30s` ghi 0/0 token tách riêng khỏi lỗi auth 1s), `studio:duplicate_post_gate_audit` (đã loại trừ `reused_from` Gói B2), `studio:single_run_spike`, `studio:stale_running_runs` (nhận biết supervisor tắt), `studio:cost_guard_*`, `studio:high_audit_overhead`, `studio:root_budget_near_limit` (cả token, wall-clock và cycles), `studio:zero_token_failures`.
- Nhận trọn `runtime_health` từ Studio (`runtime:sqlite_wal_bloat`, `runtime:ui_memory_high`, `runtime:ui_fds_high`, `runtime:streams_disk_bloat`, `runtime:receipts_disk_bloat`, `runtime:orphan_worktrees`).
- Cộng thêm kiểm tra **realtime giữa 2 tick (5 phút)** và cửa sổ 1h: `token_spike_since_prev_tick` (`> +180K` / <= 15p), `token_burn_spike_1h` (`> 350K` / 1h), `accounts_in_cooldown`.

### B. Phát hiện Rò rỉ Bộ nhớ & Tài nguyên OS (phần ngoài tiến trình UI)
- **`ui_memory_leak_trend` / `ui_fd_leak_trend`**: RSS hoặc open FDs của `aaf ui --service` tăng liên tiếp 5 tick (`>= +20 MB` / `>= +32 FDs`) trên cùng PID.
- **`orphan_worker_process`**: Tiến trình con (`claude`, `codex`, `GradleDaemon`, `java`, `Unity`) còn sống trong `AAF_WORKTREE_DIR` khi `active_aaf_run_procs == 0`.
- **`system_memory_pressure` / `high_swap_usage`**: macOS `memory_free_pct < 15%` hoặc swap `> 6 GB`.

### C. Phát hiện Lỗi Board / Factory
- `cockpit_ui_unreachable`, `factory_frozen`, `factory_blocked_tasks` (chỉ báo lỗi factory thật, bỏ qua `needs_human`/`dependency`).

---

## 3. Đăng ký cron 5 phút (chế độ mặc định)

Trước khi tạo cron, gọi `manage_task(Action="list")`. Nếu đã có cron `[AAF Board Auto-Monitor Tick]` đang chạy thì **không** tạo trùng (chỉ báo lại TaskId đang chạy).

Nếu chưa có, gọi tool `schedule`:
- `CronExpression`: `"*/5 * * * *"`
- `IsDaemon`: `true`
- `ExactTiming`: `false`
- `Prompt`:

```text
[AAF Board Auto-Monitor Tick] Kiểm tra toàn diện tình trạng board AAF, rò rỉ bộ nhớ và rò rỉ token trên GeelyEx2, OfficeReader và các repo đang auto-run:
1. Chạy `bash ~/.gemini/config/skills/aaf_heath/scripts/aaf_health_check.sh` để kiểm tra `aaf status --all`, DB `tasks`/`runs`/`events`, xu hướng RAM/FD/WAL/worktree, tốc độ đốt token, unmeasured 0/0 token leak, duplicate cross-audit, churn loop, root budgets, supervisor logs và Cockpit UI `/api/health`.
2. Nếu phát hiện BẤT KỲ lỗi AAF, rò rỉ bộ nhớ, hoặc rò rỉ token nào (`ANOMALY:` hoặc `WARN:` nghiêm trọng do factory bug — ví dụ `ui_memory_leak_trend`, `ui_fd_leak_trend`, `sqlite_wal_bloat`, `unmeasured_token_leak`, `duplicate_cross_audit`, `token_burn_spike`, task bị `blocked` do lỗi AAF, `frozen=true`, UI down):
   - Điều tra root cause thật sự trong `/Volumes/Data/Toan/AutoAgentFactory`.
   - Sửa triệt để root cause + viết test hồi quy + chạy `cargo test`, `cargo test --test repo_rules`, `cargo clippy --all-targets -- -D warnings`.
   - Rebuild & cài lại bằng `bash /Volumes/Data/Toan/AutoAgentFactory/.agent/skills/aaf-update/scripts/update_aaf.sh --local`.
   - Unblock/sync lại các task bị ảnh hưởng bởi lỗi AAF vừa fix để board chạy tiếp tự động.
   - Báo cáo ngắn gọn cho user về lỗi/rò rỉ vừa phát hiện + bản fix vừa cài.
3. Nếu không có lỗi AAF nào (hoặc chỉ dừng hợp lệ vì cần user duyệt head-lock `aaf approve`, cắm máy thật ADB, PR đã mở, hoặc `ui-supervisor` đang tắt chủ động): báo cáo gọn tình trạng board + chỉ số RAM/FD/token mới nhất; nêu rõ nếu có task mới sang `review`/`done` hoặc cảnh báo `WARN:` cần chú ý.
4. Khi toàn bộ các task tự động hoá được trên board chạy mượt 100% không còn lỗi/rò rỉ AAF, báo cáo tổng kết cho user và đề xuất tắt lịch monitor.
```

---

## 4. Playbook điều tra nhanh theo từng cảnh báo rò rỉ

| Cảnh báo | Nơi điều tra trong `/Volumes/Data/Toan/AutoAgentFactory` |
|---|---|
| `ui_memory_leak_trend` / `runtime:ui_memory_high` / `ui_fd_leak_trend` | `crates/aaf-engine/src/web_ui/` (`sse.rs`, `server.rs`), `crates/aaf-cli/src/cmd/ui/report_cache.rs` |
| `studio:unmeasured_token_leak` / `studio:zero_token_failures` | `crates/aaf-engine/src/executors/claude.rs`, `codex.rs`, `stream_json` parser khi bị `killed`/`timed_out` |
| `studio:duplicate_post_gate_audit` / `studio:high_audit_overhead` | `crates/aaf-engine/src/factory/post_audit.rs`, `post_audit/helpers.rs`, `executors/claude_flags.rs` |
| `studio:high_wasted_tokens` / `studio:single_run_spike` / `token_burn_spike_1h` | `crates/aaf-engine/src/factory/unblock.rs`, `scripts/aaf_scoped_test.sh` của repo đích |
| `runtime:sqlite_wal_bloat` / `sqlite_wal_bloat` | `crates/aaf-engine/src/db/mod.rs` (`journal_size_limit`), long-lived read transactions |
| `runtime:orphan_worktrees` / `runtime:streams_disk_bloat` | `aaf gc --all`, `crates/aaf-engine/src/gc.rs`, `crates/aaf-engine/src/git/worktree.rs` |

### Quy chuẩn sửa code bắt buộc
- Mọi file `.rs` dưới `crates/` phải `<= 600` dòng; zero `unsafe`, zero `tokio`, zero `unwrap`/`expect` trong non-test code.
- Nếu cần review kiến trúc/diff bằng Claude CLI, luôn dùng Claude Opus:
  `CLAUDE_CONFIG_DIR=/Users/alex/.claude-finos claude -p --model opus --no-session-persistence --strict-mcp-config`
- Bắt buộc pass test liên quan + `cargo test --test repo_rules` + `cargo clippy --all-targets -- -D warnings`, rồi cài lại bằng `update_aaf.sh --local`. Không tự `git commit`/`git push` khi user chưa yêu cầu.
