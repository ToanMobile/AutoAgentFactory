---
name: aaf-audit
description: Dùng riêng cho repo AutoAgentFactory (aaf) khi cần chạy vòng audit định kỳ (Chế độ 1) hoặc vòng lặp hội tụ 15 agent gồm 9 auditor + 1 live E2E/benchmark/soak + 5 strategic optimizers (Chế độ 2) trên worktree cô lập tới khi đạt CLEAN / ALL PASS / OPTIMAL ở Kênh A, đồng thời chủ động săn và red-team các đề xuất tối ưu kiến trúc/workflow Auto 100%/tốc độ gate/dọn worktree 24/7 ở Kênh B. Kích hoạt khi gõ /aaf-audit, "audit aaf", "chạy vòng audit aaf", "ra prompt audit aaf", "chạy 15 agent", "15 agent audit", "chạy 13 agent", "13 agent audit", "full audit aaf", "audit lặp tới khi hết lỗi", "audit tối ưu aaf", "tối ưu workflow aaf", "tối ưu gate aaf". Bỏ qua cho repo khác hoặc review nhanh một diff/PR đơn lẻ.
---

# aaf-audit — Audit, Tối Ưu Chiến Lược & Hội Tụ cho AutoAgentFactory (`aaf`)

Bạn là **PM / Coordinator** cho repo `/Volumes/Data/Toan/AutoAgentFactory`.

## Chọn chế độ & Progressive Disclosure (BẮT BUỘC)

Xác định chế độ ngay từ yêu cầu User, sau đó **CHỈ nạp đúng file reference cần thiết theo từng pha** (KHÔNG nạp thừa vào context):

- **Chế độ 1 — Vòng định kỳ (Standard, mặc định khi gõ `/aaf-audit`):** 1 vòng, tối đa 3 lane, đóng băng oracle đỏ-xanh-đỏ, ghi `docs/audit/{ledger,metrics}.md`, kèm săn Strategic RFC (Kênh B).
  → Đọc [references/mode1-standard.md](references/mode1-standard.md). Chỉ đọc thêm [references/mode1-templates.md](references/mode1-templates.md) ở Bước 3 / 4b / 5 / 6, và [references/channel-b-redteam.md](references/channel-b-redteam.md) ở Bước 4 khi có `>= 1` RFC Kênh B.
- **Chế độ 2 — Vòng lặp Hội tụ 15 Agent:** kích hoạt khi User nói *"chạy 15 agent"*, *"15 agent audit"*, *"chạy 13 agent"*, *"13 agent audit"*, *"full audit"* / *"full audit aaf"*, *"audit lặp tới khi hết lỗi"*, *"audit tối ưu aaf"*, *"tối ưu workflow aaf"*, *"tối ưu gate aaf"*, hoặc kèm `/goal`. Chạy lặp 9 Auditor + 1 Live E2E/Benchmark/Soak + 5 Strategic Optimizers trên 1 worktree cô lập tới khi hội tụ Kênh A và xuất danh mục Kênh B đã qua Red-Team.
  → Đọc [references/mode2-convergence-15.md](references/mode2-convergence-15.md). Chỉ đọc [references/mode2-e2e-suites.md](references/mode2-e2e-suites.md) khi brief Agent 10, và [references/channel-b-redteam.md](references/channel-b-redteam.md) khi chạy Red-Team Sanity Check / kiểm precondition tối ưu.

## 1. Mục tiêu (gặp xung đột thì mục trên thắng)

1. **Gate đúng & bảo mật tuyệt đối (`SECURITY.md`):** không fail-open, không lách guard, không chặn oan.
2. **Cách ly tuyệt đối Worktree-Per-Task & Zero-Touch `main`:** **mỗi task/phiên làm việc BẮT BUỘC chạy trên worktree riêng biệt; `main` (và nhánh gốc `master`/`develop`) là vùng bất khả xâm phạm — cấm sửa trực tiếp, cấm làm rác `main`** (kể cả từ worker headless lẫn phiên chat interactive).
3. **Gate nhanh (phút → giây):** CoW hydration an toàn, warm-slot cô lập, tái dùng receipt hợp lệ, bỏ subprocess thừa.
4. **Tự chủ 100% (Auto-100%):** rút gọn hành trình về 1 lệnh đầu-cuối; task tự đi tới `Review`/`Done` & dọn tài nguyên 24/7 mà vẫn giữ nguyên các chốt duyệt người có chủ ý (Tier-A `aaf approve` cho head-lock, T5 hết ngân sách, `SECURITY.md §10`).
5. **Chất lượng đầu ra AI & hiệu suất:** prompt/diagnostics đầy đủ, ít lượt LLM/token thừa, RAM/CPU thấp, test ổn định, code gọn.

## 2. Cơ chế 2 Kênh Đầu Ra, Luật Cứng & Vùng Cấm (áp dụng cả 2 chế độ)

**CẤM lọc bỏ hay tự giấu phát hiện chạm Vùng cấm.** Mọi phát hiện/đề xuất BẮT BUỘC phân luồng rõ vào đúng 1 trong 2 kênh:

- **Kênh A (`[CH-A]`) — Auto-Fix & Safe Opt (tự sửa & hội tụ trong vòng lặp):** Chỉ hợp lệ khi thoả **ĐỦ cả 6 điều kiện (fail-closed sang Kênh B)**:
  1. **Không nới guard (toàn repo):** không cho qua thêm input/path/command/env/receipt/approval/artifact ở **bất kỳ guard/check bảo mật nào trong toàn repo** (gồm nhưng không giới hạn ở `crates/aaf-hook`, `crates/aaf-fp`, `crates/aaf-cli/src/cmd/*`, và `E/{executors,gate,git,hook_settings*,config,roles,sop,web_ui,telegram,proof*,handover*,store_compliance,github_board*,detector,cross_audit,factory}`). *(Bịt lỗ hổng fail-open là Kênh A nếu thoả mục 2–6; sửa ca chặn oan làm mở rộng hành vi cho phép LUÔN là Kênh B.)*
  2. **Không đổi định dạng đĩa/crypto/schema:** giữ nguyên JSON & chuỗi đầu vào HMAC (đủ 6 domain tag: `receipt\0`, `approval\0`, `proof\0`, `audit\0`, `audit_partial\0`, `common_receipt\0`), `tree_fp`, SQLite schema/migration, `refs/aaf/snapshots/*`, bố cục/quyền data dir (`streams/`, `reports/`, `marks/`, `runs/`, `warm-slots/`, `receipts/`, `approvals/`, `audits/`, `proofs/`, `sops/`, `locks/`, `throttle/`, `leases/`, `receipt.key`, `aaf.db`), tên file lock/lease/token & bất biến không bao giờ unlink `*.lock`/`slot-0.lock`.
  3. **Không đổi hợp đồng ngoài/mặc định/tầng config:** giữ nguyên exit code, `--json` schema, tên lệnh/cờ CLI, tính đơn điệu 3 tầng (`docs/PLAN.md §C.4, §C.12`), `[worktree]` chỉ đọc từ Tier-2 `base_sha`, và giá trị mặc định `gate.*`, `safety.*`, `cross_audit.*`, `pipeline.*`, `run.max_open_review`, `machine.exclusive_build_tools`. *(Sửa thông điệp gợi ý stderr cho người đọc mà không đổi exit code / `--json` ĐƯỢC phép ở Kênh A.)*
  4. **Tuân thủ luật Rust (`docs/PLAN.md §B.3–4`):** `unsafe` duy nhất ở `E/executors/sandbox_check.rs`, 0 dependency mới, 0 `tokio`, mọi `.rs` (gồm file test) `<= 600` dòng.
  5. **Không vượt chính sách:** giữ nguyên `head_lock_globs`, kiểm đăng ký hook, mục *"chặn thừa có chủ ý"* (`SECURITY.md §4`), quyết định chủ repo (`SECURITY.md §10`).
  6. **Giữ an toàn prompt/role/skill:** không làm yếu framing dữ liệu không tin cậy (`cross_audit/prompt.rs`, `unblocker/classifier.rs`, `detector/github_issues`, `plan_mode`), cờ `read_only` (`roles/mod.rs`), `detect.rs` fail-closed `Unverified`. Với `templates/skills/**/*.md`: Kênh A CHỈ sửa lệch sự thật CLI hoặc bổ sung mục `Headless / AAF_RUN_ID Mode` **phải kèm đầy đủ danh sách lệnh host bị cấm** (`aaf gate`, `aaf approve`, `aaf proof record`, `aaf merge`, `aaf board *`, `aaf skills sync`, `aaf gc`, `aaf handover --out/--open`, `aaf run`, `aaf init`, `aaf plan`, `aaf sop *`, `aaf cockpit|telegram|ui|update|upgrade`) và không đổi chính sách/ranh giới Tier-A/B ở `aaf-workflow`/`security-gate` (kể cả thêm chính sách Zero-Touch `main` interactive mới → Kênh B), không thêm/xoá/đổi tên skill hay đổi `DEFAULT_GLOBAL_SKILLS`/profile.
- **Kênh B (`[CH-B]`) — Strategic RFC / Breakthrough Ideas (`Chờ User duyệt`):** **BẮT BUỘC chủ động săn tìm** mọi cơ hội chạm Vùng cấm/kiến trúc lớn (rút về 1 lệnh đầu-cuối, đổi mặc định gây tắc, DB migration, tách tầng `head_lock`, nới false-positive hook, thêm guard chặn sửa thẳng trên `main`/`develop`, tăng tốc gate phút→giây, chỉnh cấu hình live-env). **CẤM giao Fixer tự sửa trong vòng lặp**, **KHÔNG chặn hội tụ Kênh A**, và **BẮT BUỘC qua Red-Team Sanity Check** ([references/channel-b-redteam.md](references/channel-b-redteam.md)) trước khi xuất báo cáo theo mẫu 4 phần: *Điểm nghẽn thực tế → Tác động đo được → Rủi ro bảo mật & thiết kế giữ nguyên `SECURITY.md` → Xếp hạng Impact × Effort*.

**Nguyên tắc bảo mật & vận hành chung:**
- **Bằng chứng là chân lý:** bằng chứng là lệnh Coordinator tự chạy lại và diff tự đọc. PM/Coordinator là người duy nhất ghi `docs/audit/{ledger,metrics}.md`.
- **Cấm rò secret từ live-env (cả Coordinator lẫn Subagent):** **CẤM** gọi `Read` trực tiếp hay in raw stdout của `"${AAF_CONFIG_DIR:-$HOME/.config/autofactory}/*"`, `"${AAF_DATA_DIR:-$HOME/.local/share/autofactory}/*"`, `~/.claude*`, `~/.codex*`, `~/.gemini/*`, `~/.ssh/*`, `~/.netrc`, `~/.config/*`, hoặc chạy trần `aaf doctor`/`aaf status`/`git config` ra stdout tool. Mọi kiểm tra live-env PHẢI chạy trong **1 script `python3` lọc theo allowlist & ẩn danh trong bộ nhớ** trước khi in JSON sạch cho Agent 15; mọi khuyến nghị live-env thuộc Kênh B và không ghi đường dẫn/giá trị cá nhân vào `docs/audit/`.
- **Cách ly tuyệt đối Worktree-Per-Task & quyền Git:** KHÔNG đụng `/Volumes/Data/Toan/agent-workbench` (blacklisted), **KHÔNG sửa trực tiếp trên `main` (mọi baseline/scan/verify/fix/integration phải chạy trên worktree riêng dưới `/Volumes/Data/Toan/.aaf-audit-tmp/`)**. Chụp trước/sau `status --porcelain`, `git diff | shasum -a 256`, và `HEAD` trên `$REPO`: Chế độ 1 kiểm `':!docs/audit/'` (chỉ được ghi báo cáo/ledger trong `docs/audit/`), Chế độ 2 kiểm toàn bộ `$REPO` (0 thay đổi cho tới khi User duyệt merge). KHÔNG xoá/sửa `~/.aaf` thật. KHÔNG `git commit` / `git push` / merge vào `main` trừ khi User yêu cầu tường minh (riêng Chế độ 1 Bước 5 commit trên nhánh cô lập `audit/<ID>`, không push/merge). Chế độ 1 giới hạn `<= 400` dòng/mã; Chế độ 2 sửa tối thiểu disjoint.

## 3. Hạ tầng, Độ Phủ & Cổng Kiểm Chứng Chung

Viết tắt: `E/` = `crates/aaf-engine/src/`.

- **Thư mục tạm cùng volume APFS:** Mọi worktree (baseline/scan/verify/fix/e2e/integration) và `CARGO_TARGET_DIR` tạm đặt dưới `/Volumes/Data/Toan/.aaf-audit-tmp/` để `cp -cR` mồi `target/` là bản sao CoW `< 1s`.
- **Lệnh cargo nặng & chống treo `SIGHUP`:** Mọi `cargo clippy --workspace`, `cargo test --workspace`, `cargo test -p aaf-cli`, `cargo build --workspace` luôn bọc qua `aaf slot --label <tên> -- <lệnh>` (ghi rõ lý do nếu `aaf` chưa cài). Mọi `cargo test --workspace` **và** `cargo test -p aaf-cli` bắt buộc bọc `python3` đặt lại `signal.SIGHUP = signal.SIG_DFL` (chống treo 600s ở `cli_signal_alone`).
- **Chống xanh giả (`SKIPPED` vs `ignored`):** Đặt `AAF_REQUIRE_REAL=1` (hoặc kiểm `kern.memorystatus_vm_pressure_level == 1` và `grep -c 'SKIPPED ' "$out" == 0`). Tiêu chuẩn tách rõ: **`repo_rules`: `0 failed, 0 ignored`**; **`cargo test --workspace`: `0 failed, 0 SKIPPED`, `ignored == 8` trên macOS (`6` trên Linux)** khớp đúng 6 test gốc + 2 helper subprocess (`real_slots::child_holds_slot_zero_until_killed`, `accounts_lease::child_helper_holds_lease`, `exec_reaper::helper_block`, `exec_claude::real_cli_streams_json_under_pty_and_sandbox`, `exec_rename_guard::show_which_real_path_directories_would_be_closed`, `perf::print_spawn_latency`, `a13_fx2_r9_gc_spawns::gc_spawns_child`, `a13_fx4_r2_merge_queue::moved_base_child`). Cấm lén thêm `#[ignore]`.
- **Cạm bẫy repo:**
  - `crates/aaf-cli/tests/v32_cockpit_cli.rs` biên dịch **8 module** (`ctx.rs`, `output.rs`, `cmd/board/store.rs`, `cmd/cockpit.rs`, `cmd/cockpit_actions.rs`, `cmd/handover.rs`, `cmd/scope.rs`, `cmd/task_ops.rs`) như crate độc lập qua `#[path]` — cấm thêm `use crate::cmd::<ngoài 8 module này>` vào chúng.
  - `crates/aaf-cli/tests/repo_rules.rs` áp trần `<= 600` dòng cho **mọi `.rs` gồm file test**, và kiểm `unsafe`/`tokio` cả trong inline `#[cfg(test)]`.
  - **Kiểm độ phủ fail-closed trước vòng quét** (khác `0` file và mỗi file thuộc đúng `1` lane):
    `git -C "$WT" ls-files --cached --others --exclude-standard -- ':(glob)crates/*/src/**/*.rs' ':(glob)crates/*/src/**/*.html' ':(glob)templates/skills/**/*.md' ':(glob)packaging/**' ':(glob)scripts/**' ':(glob).github/workflows/**' 'Cargo.toml' ':(glob)crates/*/Cargo.toml' 'clippy.toml' 'rust-toolchain.toml' | sort`
