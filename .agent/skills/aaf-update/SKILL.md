---
name: aaf-update
description: >-
  Cập nhật và cài đặt bản mới nhất của AutoAgentFactory (`aaf` & `aaf-hook`)
  toàn máy để tất cả repo tự dùng bản mới ngay lập tức, kèm kiểm tra `aaf doctor`
  và tuỳ chọn sync skills hoặc multi-host hooks sang từng repo. Dùng khi người
  dùng nói "update aaf", "cập nhật aaf", "cài lại aaf", "build aaf",
  "sync aaf all repo", hoặc "/aaf-update".
---

# AAF Global Update (`aaf-update`)

Skill một bước (lưu trực tiếp trong repo tại `.agent/skills/aaf-update/`) để build/cài đè 2 binary toàn máy (`~/.cargo/bin/aaf` và `~/.cargo/bin/aaf-hook`) từ `AutoAgentFactory`. Vì mọi repo trên máy đều gọi chung 2 binary qua `PATH`, cài 1 lần là **toàn bộ các repo tự động chạy bản `aaf` mới nhất**.

> [!CAUTION]
> - Tuyệt đối **KHÔNG** đụng vào `/Volumes/Data/Toan/agent-workbench` (repo cũ đã blacklisted).
> - Tuyệt đối **KHÔNG** tự ý `git commit`, `git push`, hay `git reset --hard` trên bất kỳ repo nào nếu người dùng chưa yêu cầu tường minh.

---

## 1. Quy trình chuẩn (Mặc định — Cập nhật binary toàn máy)

Chạy script đi kèm skill (tự động nhận diện đường dẫn repo từ vị trí script, không cần cấu hình tay sau khi cài lại máy):

```bash
bash ~/.gemini/config/skills/aaf-update/scripts/update_aaf.sh
```

*(Hoặc chạy trực tiếp từ repo: `bash .agent/skills/aaf-update/scripts/update_aaf.sh`)*

Script tự động thực hiện 5 bước, **không cần cờ nào**:
1. `git fetch origin main` rồi build **đúng bản mới nhất của `origin/main`** (mọi thứ đã merge, kể cả web UI Cockpit/Studio nhúng trong binary) trong worktree riêng `~/.cache/aaf-update/src`. Checkout `AutoAgentFactory` đang ở nhánh nào, có file chưa commit gì cũng **không bị đụng**. Cache build ở `~/.cache/aaf-update/target` nên lần sau nhanh.
2. Cài đè 2 binary vào `~/.cargo/bin`:
   - `cargo install --path <src>/crates/aaf-cli --locked --force`
   - `cargo install --path <src>/crates/aaf-hook --locked --force`
3. Xác minh binary cài đặt thành công:
   - `which aaf aaf-hook` & `aaf --version`
   - `aaf executors list`
   - Smoke check `aaf-hook` fail-closed (payload JSON lỗi → exit `2`; `git push --force` → exit `2`).
4. Tự khởi động lại (hoặc cài lần đầu trên macOS) dịch vụ nền `com.autoagentfactory.ui` (`aaf ui --service` trên `127.0.0.1:4317`, giữ nguyên `--allow-actions` nếu đã bật) và kiểm tra `/api/health` để `http://127.0.0.1:4317/health` luôn dùng bản UI mới nhất (đặt `AAF_SKIP_UI_SERVICE=1` nếu muốn bỏ qua).
5. Tự đồng bộ symlink các global skill (`aaf-update`, `aaf-audit`, `aaf-workflow`) qua `scripts/link-global-skills.sh` và báo cáo ngắn gọn version, đường dẫn binary và kết quả kiểm tra.

---

## 2. Các tuỳ chọn mở rộng (chỉ chạy khi cần)

| Mục đích | Lệnh |
|---|---|
| **Tăng version + tạo tag + push GitHub Release + cài toàn máy** *(chỉ chạy khi user yêu cầu tường minh)* | `bash .../update_aaf.sh --release patch` (hoặc `minor` / `major`) |
| Build nhánh khác thay vì `origin/main` | `bash .../update_aaf.sh --branch <tên-nhánh>` |
| Build đúng checkout local như đang có (kể cả chưa commit) | `bash .../update_aaf.sh --local` (thêm `--pull` để fast-forward trước) |
| Chỉ nâng SemVer cục bộ (`Cargo.toml` + `Cargo.lock`, không commit/push) | `bash .../update_aaf.sh --local --bump patch` (hoặc `minor` / `major`) |
| Kiểm tra `aaf doctor` trên nhiều repo sau khi cài | `bash .../update_aaf.sh --check-repos <repo1> [repo2...]` |
| Chỉ đồng bộ lại `.claude/skills` (khi skill pack đổi) | `bash .../update_aaf.sh --sync-skills <repo1> [repo2...]` |
| Đăng ký thêm hook Codex + Gemini cho repo | `bash .../update_aaf.sh --sync-hooks <repo1> [repo2...]` |

*Lưu ý:*
- Chế độ `--release <patch|minor|major>` tự kiểm tra nhánh `main` sạch (không có tracked diff), chạy `cargo fmt --all --check` + `repo_rules`, tăng version trong `Cargo.toml` + đồng bộ `Cargo.lock`, commit `chore(release): vX.Y.Z`, tạo annotated tag `vX.Y.Z`, `git push origin main vX.Y.Z` (kích hoạt `.github/workflows/release.yml` build 4 tarball lên GitHub Release), rồi build & cài bản mới toàn máy. **Chỉ chạy khi người dùng yêu cầu tường minh release/đẩy bản mới lên GitHub.**
- Khi dùng `--sync-hooks` hoặc `--sync-skills`, script chỉ ghi file cục bộ (không ghi đè skill đã sửa tay) và **không tự commit**; nhắc người dùng kiểm tra `git diff` trước khi commit.

---

## 3. Khôi phục nhanh sau khi cài lại máy tính

Chỉ cần clone repo `AutoAgentFactory` về máy rồi chạy đúng 1 lệnh:

```bash
bash scripts/link-global-skills.sh && bash .agent/skills/aaf-update/scripts/update_aaf.sh
```

Lệnh này tự gắn lại toàn bộ global skills (`aaf-update`, `aaf-audit`, `aaf-workflow`) cho Antigravity, Claude Code và Codex, build & cài `aaf` + `aaf-hook`, và bật dịch vụ nền Cockpit/Studio.
