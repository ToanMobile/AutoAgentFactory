---
name: aaf_heath
description: >-
  Bật, chạy 1 lần, hoặc tắt bộ giám sát toàn diện 4 trụ cột (Lỗi/Rò rỉ + Chất lượng
  + Tốc độ/Slot + Hiệu quả Token) định kỳ mỗi 30 phút cho AutoAgentFactory (AAF) trên
  GeelyEx2, OfficeReader và các repo đang auto-run: kiểm tra `aaf status --all`,
  `/api/report?since=24`, DB tasks/runs/events, `throttle/` locks, supervisor logs và
  Cockpit UI; đánh giá Factory Score, First-Pass Gate, Cross-Audit & mechanical_score,
  vacuous tests, tốc độ gate/coding/audit, worker treo/deadlock heavy slot, tốc độ đốt
  token & token/task done, rò rỉ RAM/FD/WAL/worktree, tự chẩn đoán & sửa lỗi hoặc tối
  ưu AAF khi cần. Kích hoạt khi gõ `/aaf_heath`, `/aaf_health`, `/aaf-health`,
  "monitor aaf", "bật monitor 30 phút aaf", "theo dõi board aaf", "check sức khỏe aaf",
  hoặc "tắt monitor aaf".
---

# AAF Board Health, Quality, Speed & Token Monitor (`/aaf_heath`)

Skill giám sát toàn diện **4 trụ cột** của AutoAgentFactory (`aaf`) định kỳ mỗi 30 phút (hoặc chạy 1 lần theo yêu cầu):
1. **Lỗi & Rò rỉ hệ thống** (factory bug, deadlock, RAM/FD trend, orphan worker, WAL/disk/worktree bloat).
2. **Chất lượng đầu ra** (Factory Score & 5 pillars, First-Pass Gate, `mechanical_score` & `llm_average` của Cross-Audit, cảnh báo `vacuous test`, vòng lặp rework/churn).
3. **Tốc độ & Thông lượng** (thời gian `coding`/`audit`/`gate` p50/p90, throughput `done_1h`/`done_24h`, hàng đợi `throttle/queue/*.wait`, phát hiện worker đứng im không xuất stream và deadlock `drain.lock` ↔ dead orphan slot).
4. **Hiệu quả Token & Chi phí** (tổng token, `in:out`, `wasted%`, `audit%`, `avg_coding`/`avg_audit`, `tokens/done task`, top 3 task ngốn token nhất, `unmeasured_token_leak`, `duplicate_post_gate_audit`, `root_budget`, cooldown tài khoản).

> [!CAUTION]
> - Tuyệt đối **KHÔNG** đụng vào `/Volumes/Data/Toan/agent-workbench` (repo đã blacklisted).
> - Tuyệt đối **KHÔNG** tự ý `git commit`, `git push`, hoặc `gh pr create` nếu người dùng chưa yêu cầu tường minh.
> - Tuyệt đối **KHÔNG** dùng `sleep` trong shell để vòng lặp chờ — chỉ dùng tool `schedule`.
> - Tuyệt đối **KHÔNG** tự ý bật `ui-supervisor` nếu `ui-supervisor.json` đang đặt `"enabled": false`.

---

## 1. Xác định chế độ chạy

| Ý định người dùng | Hành động |
|---|---|
| Mặc định (`/aaf_heath`, "bật monitor 30 phút") | Chạy kiểm tra ngay lập tức 1 lần → kiểm tra `manage_task(Action="list")` để tránh trùng → tạo cron `*/30 * * * *` (`IsDaemon: true`). |
| Kiểm tra nhanh 1 lần (`/aaf_heath once`, "check 1 lần") | Chỉ chạy script kiểm tra và báo cáo đủ 4 trụ cột, **không** tạo cron. |
| Tắt lịch monitor (`/aaf_heath stop`, "tắt monitor") | Gọi `manage_task(Action="list")` tìm task cron có `[AAF Board Auto-Monitor Tick]` → gọi `manage_task(Action="kill", TaskId=...)`. |

---

## 2. Chạy snapshot 4 trụ cột trong 1 lệnh duy nhất

```bash
bash ~/.gemini/config/skills/aaf_heath/scripts/aaf_health_check.sh
```

*(Đường dẫn trực tiếp trong repo: `bash /Volumes/Data/Toan/AutoAgentFactory/.agent/skills/aaf_heath/scripts/aaf_health_check.sh`.)*

Script **đồng bộ trực tiếp với `/api/report?since=24` của `http://127.0.0.1:4317/health`** (tên cũ `/studio` vẫn hoạt động song song; `score`, `ops_diagnostics`, `token_diagnostics`, `runtime_health` — **Single Source of Truth**, không tính trùng trong bash; tự đọc token từ `~/.local/share/autofactory/ui-service.token`, không in lộ token) **và** bổ sung các check tầng OS (`metrics.jsonl` 5-tick RSS/FD trend, macOS `memory_pressure`/swap, orphan worker process) vào chuỗi mẫu vòng tròn atomic tại `~/.cache/aaf-health/metrics.jsonl` (tối đa 288 mẫu = 6 ngày @ 30 phút):

### A. Trụ cột Chất lượng (Quality — từ Studio `score` + `ops_diagnostics.quality`)
- **Factory Score & Grade** (`score/100`, 5 pillars: `first_pass_gate`, `autonomy_discipline`, `audit_integrity`, `block_health`, `flow_efficiency`, và top bottlenecks).
- **First-Pass Gate & Cross-Audit:** `studio_ops:quality:low_first_pass_gate` (`< 70%` WARN, `< 40%` ANOMALY khi `>= 3` task), `studio_ops:quality:high_cross_audit_fail_rate` (`> 45%` WARN khi `>= 3` audit).
- **Điểm cơ học & Oracle:** `mech_avg`, `audit_effective_avg`, `llm_avg`, phát hiện ngay **`studio_ops:quality:clear_gate_with_mechanical_score_zero`** (gate `CLEAR` nhưng `mechanical_score == 0`), `studio_ops:quality:vacuous_test_warnings` (`>= 3` trong 24h), và **`studio_ops:quality:high_rework_churn`** (task `running`/`gating` có `unblock_cycles >= 4`).

### B. Trụ cột Tốc độ & Thông lượng (Speed, Slots & Stalls — từ Health `ops_diagnostics.speed`)
- **Độ trễ & Thông lượng:** `done_1h`, `done_24h`, `coding_dur` (p50/p90 — đã loại `plan`/`split`), `audit_dur` (p50), `gate_dur` (p50/p90), `gate_avg` (`studio_ops:speed:slow_gate_avg` `> 3m` WARN, `> 7m` ANOMALY), `lead_time` (p50).
- **Hàng đợi Heavy Slot & Deadlock:** Health probe trực tiếp `$AAF_DATA_DIR/throttle/` (gồm cả host `heavy.*.lock` và Seatbelt `guest/heavy.*.rec`):
  - `studio_ops:speed:dead_orphan_heavy_slots` (lock hoặc guest `.rec` chứa PGID đã chết).
  - `studio_ops:speed:heavy_slot_drain_deadlock` (`drain.lock` giữ khi `active_slots == 0 && waiters > 0 && oldest_wait >= 30s`).
  - `studio_ops:speed:heavy_slot_queue_slow` / `heavy_slot_queue_stalled` (`oldest_wait >= 3m` WARN / `>= 5m` ANOMALY).
- **Task bị kẹt, Đói tài khoản & Vòng lặp (`studio_ops:speed:*`):**
  - `account_pool_exhausted`: toàn bộ tài khoản đang leased/cooling hoặc supervisor báo `waiting for an account` khi còn task `ready`/`gating` (HIGH nếu toàn bộ tài khoản cạn sạch).
  - `stuck_gating_tasks`: task kẹt trong `gating` `>= 20p` (WARN) / `>= 45p` (HIGH), kèm nguyên nhân cụ thể (`all_accounts_leased`, `all_accounts_cooling`, `waiting_for_account`, `heavy_slot_waiting`, `audit_or_worker_running`, `idle_post_gate_no_run`).
  - `gating_spin_loop`: task lặp `>= 3` sự kiện `gate_result` trong 30p mà không thoát khỏi `gating` (HIGH).
  - `worker_command_loop`: worker lặp cùng 1 tool/target `>= 5` lần liên tiếp trong stream tail 16 KiB (HIGH).
  - `stalled_worker`: `streams/<run_id>.jsonl` im lặng `>= 5p` (WARN) / `>= 10p` (HIGH), kèm `last_tool: last_target` và `waiting_on_slot`.
  - `long_running_worker`: worker chạy liên tục `>= 25p` (WARN) / `>= 40p` (HIGH).
  - `dag_upstream_bottleneck`: 1 blocker đang chặn `>= 2` task `ready` phía sau (WARN).
  - `stale_review_tasks`: task nằm ở `review` `>= 4h` chưa được duyệt/merge (WARN).

### C. Trụ cột Hiệu quả Token & Chi phí (Token Efficiency — từ Health `token_diagnostics` + `ops_diagnostics.efficiency`)
- Đồng bộ 100% từ `/health` (`/api/report?since=24`): `total_tokens`, tỷ lệ `in:out`, `wasted%`, `audit%`, `avg_coding_tokens`, `avg_audit_tokens`, `burn_rate` (`5m`/`1h`/`24h`), `unmeas_long`, `zero_tok`, `dup_audits`, và **top 3 task đốt nhiều token nhất**.
- Cảnh báo hiệu quả: `studio_ops:efficiency:high_tokens_per_done` (`> 450K tok/done`, mẫu số chỉ tính task `done` có run thật), `studio_ops:efficiency:high_burn_zero_done` (`> 800K tok` mà `0 done`), `token_spike_since_prev_tick` (`> +900K` / `<= 60p`), `token_burn_spike_1h` (theo `burn_rate.spike_limit_1h` hoặc mặc định `> 350K` / `1h`), `accounts_in_cooldown`.

### D. Trụ cột Sức khoẻ Hệ thống, OS & Đồng bộ (Memory / FD / WAL / Disk / Daemons)
- `health_sync_failed`: UI `/api/health` sống nhưng `/api/report?since=24` timeout/lỗi HTTP (ANOMALY ngay lập tức + tự kích hoạt fallback SQLite/FS cho `stuck_gating`, `stalled_worker`, `long_running_worker`, `queue/*.wait`).
- `ui_memory_leak_trend` / `ui_fd_leak_trend` (tăng 5 tick liên tiếp `>= +20 MB` / `>= +32 FDs` trên cùng PID), `ui_memory_high`, `ui_fds_high`, `ui_log_panic_or_fatal` (panic/fatal trong 35p cuối của `aaf-ui.log`).
- `low_disk_space` (`< 15 GiB` hoặc `>= 88%` WARN; `< 5 GiB` hoặc `>= 95%` ANOMALY), `orphan_gradle_daemons` / `gradle_daemons_high` (tổng số & RSS của `GradleDaemon`/`KotlinCompileDaemon`).
- `orphan_running_runs_no_aaf_run` (DB có run `running` nhưng `active_aaf_run_procs == 0` — phát hiện crash `aaf run` tức thì 0 giây), `supervisor_repo_backoff` (repo bị backoff trong `/api/status`), `orphan_worker_process`, `system_memory_pressure` (`free < 15%`), `high_swap_usage` (`> 6 GB`), `sqlite_wal_bloat` (`> 64 MB`).
- `cockpit_ui_unreachable`, `factory_frozen`, `factory_blocked_tasks` (chỉ báo lỗi factory thật, bỏ qua `needs_human`/`dependency`).

---

## 3. Đăng ký cron 30 phút (chế độ mặc định)

Trước khi tạo cron, gọi `manage_task(Action="list")`. Nếu đã có cron `[AAF Board Auto-Monitor Tick]` đang chạy thì **không** tạo trùng (chỉ báo lại TaskId đang chạy).

Nếu chưa có, gọi tool `schedule`:
- `CronExpression`: `"*/30 * * * *"`
- `IsDaemon`: `true`
- `ExactTiming`: `false`
- `Prompt`:

```text
[AAF Board Auto-Monitor Tick] Kiểm tra toàn diện 4 trụ cột (Lỗi/Rò rỉ + Chất lượng + Tốc độ/Slot + Hiệu quả Token) của board AAF trên GeelyEx2, OfficeReader và các repo đang auto-run:
1. Chạy `bash ~/.gemini/config/skills/aaf_heath/scripts/aaf_health_check.sh` để thu thập snapshot đầy đủ: `aaf status --all`, Factory Score/pillars, First-Pass Gate, Cross-Audit & mechanical_score, vacuous tests, tốc độ coding/audit/gate & throughput, hàng đợi heavy slot & deadlock, worker treo stream, hiệu quả token (`wasted%`, `audit%`, `tok/done`, top task đốt token, `unmeasured_token_leak`, `duplicate_post_gate_audit`, `root_budgets`), và xu hướng RAM/FD/WAL/worktree.
2. Nếu phát hiện BẤT KỲ lỗi AAF, deadlock, suy giảm chất lượng do factory bug, rò rỉ bộ nhớ hoặc rò rỉ token nghiêm trọng nào (`ANOMALY:` hoặc `WARN:` lặp/bất thường như `quality:high_rework_churn` / 1 task `>= 10` runs hoặc `>= 3` lần fail Cross-Audit, `vacuous_test_warnings` tỉ lệ cao, `high_cross_audit_fail_rate` kéo dài, `speed:heavy_slot_drain_deadlock`, `speed:dead_orphan_heavy_slots`, `quality:clear_gate_with_mechanical_score_zero`, `ui_memory_leak_trend`, `ui_fd_leak_trend`, `sqlite_wal_bloat`, `unmeasured_token_leak`, `duplicate_post_gate_audit`, `token_burn_spike`, task bị `blocked` do lỗi AAF, `frozen=true`, UI down):
   - Chủ động drill-down sự kiện/log tìm root cause thật sự trong `/Volumes/Data/Toan/AutoAgentFactory` (không chỉ lặp lại số liệu bề mặt hay đợi user hỏi mới điều tra).
   - Sửa triệt để root cause + viết test hồi quy + chạy test liên quan, `cargo test --test repo_rules`, `cargo clippy --all-targets -- -D warnings`.
   - Rebuild & cài lại bằng `bash /Volumes/Data/Toan/AutoAgentFactory/.agent/skills/aaf-update/scripts/update_aaf.sh --local`.
   - Unblock/sync lại các task bị ảnh hưởng để board chạy tiếp tự động.
   - Báo cáo ngắn gọn cho user về lỗi/nút thắt vừa phát hiện + bản fix vừa cài.
3. Nếu không có factory bug (hoặc chỉ dừng hợp lệ vì `needs_human`, `aaf approve`, máy thật ADB, PR đã mở, tài khoản cooldown): báo cáo gọn đủ 4 nhóm chỉ số (Board/Chất lượng, Tốc độ/Slot, Token/Hiệu quả, RAM/FD/WAL) + nêu rõ nếu có task mới sang `review`/`done`, nút thắt tốc độ, hoặc cảnh báo `WARN:` cần tối ưu.
4. **Quy tắc trình bày cảnh báo bắt buộc:** Mọi vấn đề, rủi ro hoặc bất thường phát hiện được (bug/race scheduler, acc mất login/quarantine, dung lượng đĩa tụt/thấp, root budget sát trần, task kẹt `gating`/`running`, v.v.) **BẮT BUỘC** phải luôn chủ động báo rõ nguyên nhân + hướng xử lý (không đợi người dùng hỏi, không giấu vào ghi chú phụ) trong khối riêng với icon đỏ nguy hiểm **`🚨 VẤN ĐỀ PHÁT HIỆN`** đặt ở **DÒNG CUỐI CÙNG** của báo cáo.
5. Khi toàn bộ các task tự động hoá được trên board chạy mượt 100% đạt chất lượng, tốc độ và hiệu quả token tốt không còn lỗi/rò rỉ AAF, báo cáo tổng kết cho user và đề xuất tắt lịch monitor.
```

---

## 4. Playbook điều tra nhanh theo từng cảnh báo

| Cảnh báo | Nơi điều tra trong `/Volumes/Data/Toan/AutoAgentFactory` |
|---|---|
| `speed:heavy_slot_drain_deadlock` / `dead_orphan_heavy_slots` | `crates/aaf-engine/src/throttle/` (`slots.rs`, `slots/orphan_reap.rs`, `admit.rs`, `drain.rs`), `throttle/guest/*.rec` |
| `speed:account_pool_exhausted` / `stuck_gating_tasks` / `gating_spin_loop` | `crates/aaf-cli/src/cmd/run/stage_wiring.rs` (`with_audit_reserve`), `crates/aaf-engine/src/factory/scheduler/dispatch.rs`, `aaf accounts list` |
| `speed:worker_command_loop` / `stalled_worker` / `long_running_worker` | `streams/<run_id>.jsonl`, `crates/aaf-engine/src/executors/claude.rs`, `crates/aaf-cli/src/cmd/ui/studio_ops/probes.rs` |
| `speed:slow_gate_avg` / `heavy_slot_queue_slow` / `dag_upstream_bottleneck` | `scripts/aaf_scoped_test.sh` của repo đích, `crates/aaf-engine/src/throttle/` (`pools.rs`, `queue.rs`), `aaf board list` |
| `health_sync_failed` / `ui_log_panic_or_fatal` | `crates/aaf-cli/src/cmd/ui/studio.rs`, `report_cache.rs` (singleflight), `~/Library/Logs/aaf-ui.log` |
| `low_disk_space` / `orphan_gradle_daemons` | `aaf gc --all`, `./gradlew --stop`, `df -h`, `~/.cache/autofactory/worktrees` |
| `quality:clear_gate_with_mechanical_score_zero` | `crates/aaf-engine/src/factory/post_audit/helpers.rs`, `crates/aaf-engine/src/proof/capture.rs`, `crates/aaf-engine/src/cross_audit/scoring.rs` |
| `quality:low_first_pass_gate` / `high_cross_audit_fail_rate` / `high_rework_churn` | `crates/aaf-engine/src/factory/post_audit.rs`, `crates/aaf-engine/src/factory/prompt.rs`, `crates/aaf-engine/src/cross_audit/prompt.rs` |
| `ui_memory_leak_trend` / `runtime:ui_memory_high` / `ui_fd_leak_trend` | `crates/aaf-engine/src/web_ui/` (`sse.rs`, `server.rs`), `crates/aaf-cli/src/cmd/ui/report_cache.rs` |
| `studio:unmeasured_token_leak` / `studio:zero_token_failures` | `crates/aaf-engine/src/executors/claude.rs`, `codex.rs`, `stream_json` parser khi bị `killed`/`timed_out` |
| `studio:duplicate_post_gate_audit` / `studio:high_audit_overhead` | `crates/aaf-engine/src/factory/post_audit.rs`, `post_audit/helpers.rs`, `executors/claude_flags.rs` |
| `token:high_tokens_per_done` / `studio:high_wasted_tokens` / `token_burn_spike_1h` | `crates/aaf-engine/src/factory/unblock.rs`, `scripts/aaf_scoped_test.sh` của repo đích |
| `runtime:sqlite_wal_bloat` / `sqlite_wal_bloat` | `crates/aaf-engine/src/db/mod.rs` (`journal_size_limit`), long-lived read transactions |
| `runtime:orphan_worktrees` / `runtime:streams_disk_bloat` | `aaf gc --all`, `crates/aaf-engine/src/gc.rs`, `crates/aaf-engine/src/git/worktree.rs` |

### Quy chuẩn sửa code bắt buộc
- Mọi file `.rs` dưới `crates/` phải `<= 600` dòng; zero `unsafe`, zero `tokio`, zero `unwrap`/`expect` trong non-test code.
- Nếu cần review kiến trúc/diff bằng Claude CLI, luôn dùng Claude Opus:
  `CLAUDE_CONFIG_DIR=/Users/alex/.claude-finos claude -p --model opus --no-session-persistence --strict-mcp-config`
- Bắt buộc pass test liên quan + `cargo test --test repo_rules` + `cargo clippy --all-targets -- -D warnings`, rồi cài lại bằng `update_aaf.sh --local`. Không tự `git commit`/`git push` khi user chưa yêu cầu.
