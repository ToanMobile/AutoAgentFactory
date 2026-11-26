---
name: aaf-studio-reset
description: >-
  Xoá sạch dữ liệu thống kê/telemetry cũ của AAF Studio (`/studio`, `/api/report`)
  sau khi sửa bug AAF để đo lại số liệu mới hoàn toàn sạch mà không làm gián đoạn
  các task đang chạy (`running`), tài khoản, khoá ký receipt hay lịch sử cấu trúc
  task. Tự động backup SQLite trước khi xoá, dọn `runs` đã kết thúc, sự kiện
  telemetry cũ, `root_budgets`, file `streams/*.jsonl` cũ, `~/.cache/aaf-health/metrics.jsonl`,
  tuỳ chọn ẩn (archive) các task `done`/`cancelled` cũ và checkpoint WAL.
  Kích hoạt khi gõ `/aaf-studio-reset`, `/aaf-delete`, `/delete-studio`,
  "xoá studio", "xóa studio", "xoá data studio", "reset studio", "clean studio",
  hoặc "làm sạch data studio sau khi fix bug".
---

# AAF Studio Data Reset (`/aaf-studio-reset`)

Skill dùng để **làm sạch toàn bộ dữ liệu thống kê cũ trên `/studio` (`/api/report`)** ngay sau mỗi đợt sửa lỗi AutoAgentFactory (AAF), giúp bảng điều khiển Studio và `/aaf_heath` bắt đầu ghi nhận số liệu hoàn toàn sạch từ mốc hiện tại (0 wasted tokens, 0 stale alerts, 0 unmeasured-long lịch sử) **mà không làm chết các worker đang chạy**.

> [!CAUTION]
> - Tuyệt đối **KHÔNG** đụng vào `/Volumes/Data/Toan/agent-workbench` (repo đã blacklisted).
> - Script **KHÔNG** xoá `accounts`, `tasks` đang mở, `task_deps`, `receipt.key`, `approvals/`, `audits/`, `receipts/`, `throttle/`, `ui-service.token`, `ui-supervisor.json`, và **KHÔNG** xoá các lượt chạy đang có `status = 'running'` hay file stream `.jsonl` của chúng.
> - Luôn tự động tạo bản backup SQLite nóng (`sqlite3.backup`) tại `~/.local/share/autofactory/backups/` (giữ tối đa 5 bản gần nhất) trước khi sửa `aaf.db`.

---

## 1. Cách dùng nhanh

### Chạy mặc định (khuyến nghị sau mỗi lần fix bug AAF)
```bash
bash ~/.gemini/config/skills/aaf-studio-reset/scripts/reset_studio.sh
```
*(Đường dẫn trực tiếp trong repo: `bash /Volumes/Data/Toan/AutoAgentFactory/.agent/skills/aaf-studio-reset/scripts/reset_studio.sh`.)*

### Các tuỳ chọn hữu ích

| Cờ | Tác dụng |
|---|---|
| `--dry-run` | Chỉ thống kê số dòng/file sẽ bị xoá/archive, **không** thay đổi DB hay ổ đĩa. |
| `--keep-closed` | Giữ nguyên các task `done`/`cancelled` hiển thị trên board & Studio (mặc định sẽ đặt `archived_at = now` để ẩn khỏi bảng thống kê mới mà vẫn giữ dedup trong DB). |
| `--reset-cycles` | Đặt lại `unblock_cycles = 0` cho các task đang ở `ready`/`gating` (hữu ích khi task trước đó bị lặp chu kỳ do bug AAF vừa sửa). |
| `--restart-ui` | Khởi động lại dịch vụ `com.autoagentfactory.ui` để xoá cache RAM ngay lập tức *(cảnh báo: nếu có `aaf run` con đang chạy dưới UI supervisor thì việc restart sẽ ngắt lượt chạy hiện tại; mặc định **không** restart, chỉ chờ cache TTL tự hết sau vài giây)*. |
| `--no-backup` | Bỏ qua bước tạo bản sao lưu SQLite trước khi xoá (không khuyến nghị). |

---

## 2. Những gì script thực hiện (chi tiết kỹ thuật)

1. **Backup an toàn**: Dùng API `sqlite3.Connection.backup()` ghi ra `~/.local/share/autofactory/backups/aaf-before-studio-reset-<YYYYMMDD-HHMMSS>.db` và tự giữ lại 5 bản gần nhất.
2. **Giao dịch SQLite nguyên tử (`BEGIN IMMEDIATE ... COMMIT`)**:
   - Đọc danh sách `id` các lượt chạy đang có `status = 'running'` để bảo toàn tuyệt đối.
   - Xoá toàn bộ lịch sử lượt chạy đã kết thúc: `DELETE FROM runs WHERE status != 'running'`.
   - Tạm gỡ và **tái tạo chính xác nguyên bản** trigger `events_no_delete` ngay trong cùng transaction để xoá các sự kiện telemetry cũ (`run_finished`, `gate_result`, `cross_audit`, `unblock_step`, `budget_frozen`, `factory_frozen`, `factory_unfrozen`, `factory_halted`, `account_cooldown`, `rework_requested`, `task_state_changed`, `merge_result`, `pr_feedback`, và `run_started` của các run không còn `running`), đồng thời **giữ nguyên** sự kiện cấu trúc (`task_created`, `detector_hit`, `plan_generated`, `pr_opened`, và `run_started` của run đang chạy).
   - Đặt lại `root_budgets` (`tokens_used=0, wall_secs_used=0, cycles_used=0, frozen=0`) cho các root đang mở và xoá các dòng budget mồ côi của root đã đóng.
   - Đặt lại `meta`: `consecutive_root_fails = '0'`, `factory_frozen = '0'`, `factory_unfrozen_at = '<now>'` (mốc tính spike 1h của Studio bắt đầu từ thời điểm reset).
   - Mặc định (trừ khi truyền `--keep-closed`): đặt `archived_at = <now>` cho các task `done`/`cancelled` cũ để Studio không cộng lại số liệu lịch sử trước bản fix (vẫn giữ bản ghi trong DB để không import trùng GitHub issue).
   - Nếu truyền `--reset-cycles`: đặt `unblock_cycles = 0` cho các task ở trạng thái `ready`/`gating`.
3. **Nén WAL & dọn file rác**:
   - Chạy `PRAGMA wal_checkpoint(TRUNCATE);` để thu gọn `aaf.db-wal`.
   - Xoá các file `~/.local/share/autofactory/streams/<run_id>.jsonl` của những run đã kết thúc (giữ lại stream của run đang `running`).
   - Đặt lại chuỗi mẫu lịch sử `~/.cache/aaf-health/metrics.jsonl` (có khoá `fcntl.flock`) để `/aaf_heath` không báo giả delta từ mốc trước khi reset.
4. **Xác minh lại Studio**:
   - In tóm tắt trước/sau (`runs`, `events`, `root_budgets`, `streams`, `wal`, backup path) và truy vấn nhanh `aaf status --all` + `http://127.0.0.1:4317/api/report?since=24` (nếu Cockpit UI đang chạy) để xác nhận Studio đã sạch.

---

## 3. Quy trình khi agent thực thi skill này

1. Chạy `bash ~/.gemini/config/skills/aaf-studio-reset/scripts/reset_studio.sh` (thêm `--reset-cycles` nếu vừa sửa bug gây vòng lặp rework/unblock; dùng `--dry-run` trước nếu người dùng chỉ muốn xem thử).
2. Xác nhận kết quả in ra cho thấy `total_tokens`, `wasted_runs`, `unmeasured_long_runs`, và `alerts` cũ đã về sạch (`0` / không còn cảnh báo lịch sử cũ), còn các task/worker đang chạy (`running`) vẫn nguyên vẹn.
3. Báo cáo ngắn gọn đường dẫn file backup và số liệu Studio mới cho người dùng.
