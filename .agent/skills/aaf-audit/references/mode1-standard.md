# Chế độ 1 — Vòng Audit Định Kỳ (Standard, Bước 0 → 7)

> Đọc sau [SKILL.md](../SKILL.md). Chỉ đọc thêm [mode1-templates.md](mode1-templates.md) khi tới Bước 3 / 4b / 5 / 6, và [channel-b-redteam.md](channel-b-redteam.md) ở Bước 4 khi có `>= 1` RFC Kênh B.

## 1. Nguyên tắc vận hành CLI-first

- PM không tự viết code production. PM đo, chọn việc, viết kế hoạch và prompt, kiểm bằng chứng, nghiệm thu.
- **Tự chạy xuyên suốt một vòng (Bước 0 → 7) qua CLI, KHÔNG dừng hỏi giữa các bước** trừ khi gặp mục `chờ User` (vùng cấm / Kênh B RFC) hoặc kết thúc Bước 7 báo nhánh `đạt-chờ merge` để User merge:
  - Lấy tài khoản Claude gắn với repo bằng thu giữ im lặng từ `$REPO` (KHÔNG `echo` stdout hay `$CLAUDE_CONFIG_DIR`): `CLAUDE_CONFIG_DIR=$(cd "$REPO" && aaf accounts next --json | python3 -c 'import json,sys; print((json.load(sys.stdin).get("account") or {}).get("config_dir",""))')`.
  - **QUÉT (Mẫu A) — chỉ đọc:** tạo worktree sạch `SCAN_WT="/Volumes/Data/Toan/.aaf-audit-tmp/aaf-scan-${SHA}-$$"` (`git worktree add --detach "$SCAN_WT" "$SHA"`, gắn `trap` dọn), chạy song song qua `claude -p --model opus --effort high --no-session-persistence --tools "Read,Grep,Glob,Bash" --allowedTools "Read,Grep,Glob,Bash(git grep:*)"`, đổ stdout trực tiếp vào `docs/audit/<YYYY-MM-DD>/<NN>-<lane>-report.md` (stderr vào `/Volumes/Data/Toan/.aaf-audit-tmp/<NN>-<lane>.stderr.log`, KHÔNG ghi `.stderr.log` vào `docs/audit/`). Xoá `$SCAN_WT` sau khi xong.
  - **XÁC MINH (Mẫu V):** giao thợ bằng **model KHÁC** thợ quét (ví dụ quét `claude opus` → xác minh bằng subagent Antigravity / Gemini / `codex`) trên worktree `aaf-verify-<ID>` + `CARGO_TARGET_DIR` riêng dưới `/Volumes/Data/Toan/.aaf-audit-tmp/`; PM tự chạy lại test đỏ, đóng băng `git hash-object` rồi tự duyệt các mã Kênh A đủ điều kiện (Bước 4c) và chạy thẳng sang Bước 5.
  - **SỬA (Mẫu B):** PM tạo trước worktree nhánh `audit/<ID>` từ `<sha>`, copy file oracle `<file test>` đã đóng băng từ `aaf-verify-<ID>` sang (xác nhận đúng `git hash-object`), xoá `aaf-verify-<ID>`, rồi chạy 2 bước `claude -p --model opus --effort high` (**Pha 1:** `--output-format json --tools "Read,Grep,Glob,Bash" --allowedTools "Read,Grep,Glob,Bash"` chỉ đọc + chạy oracle, lưu `session_id`, không bật `--no-session-persistence` → PM soát nhanh → **Pha 2:** `claude -p --resume "$SESSION_ID" --permission-mode acceptEdits --allowedTools "Read,Edit,Write,Grep,Glob,Bash" "Làm"`, hoặc truyền phản biện Pha 1 vào lệnh Pha 2 mới).
  - **PHẢN KIỂM (Mẫu C) — chỉ đọc, model KHÁC thợ sửa:** nếu Mẫu B chạy `claude opus`, chạy Mẫu C bằng subagent Antigravity / Gemini / `codex`; nếu Mẫu B dùng model khác và Mẫu C chạy `claude -p`, truyền `--allowedTools "Read,Grep,Glob,Bash(git grep:*),Bash(git diff:*)"` và bảo đảm `model(Mẫu C) != model(Mẫu B)`.
  - Chỉ dừng và in danh sách `đường dẫn prompt — model cần chọn — mở hội thoại mới` khi CLI không có sẵn, hết quota hoặc lỗi đăng nhập.

---

## 2. Định nghĩa, Khoá, Mức độ & Bảng Lane (Phủ 100% mã nguồn, skill, packaging, scripts & CI/Cargo manifest)

**Loại phát hiện (CAT):**

| CAT | Nghĩa |
|---|---|
| GATE | gate sai hoặc chặn oan |
| FAST | gate hay pipeline chậm |
| AUTO | làm task phải cần người |
| SEC | lách guard, fail-open, lộ bí mật |
| COR | sai logic, đường panic |
| RES | rò tiến trình/khoá/fd/worktree, thiếu timeout |
| PERF | tốn RAM/CPU/token |
| OBS | thiếu đo lường |
| TST | thiếu test hai nhánh, test không xác định |
| DEL | code chết |
| SIM | trùng lặp |
| DOC | tài liệu/skill sai so với code |
| RFC | Đề xuất chiến lược Kênh B (`Chờ User`): kiến trúc, Auto-100%, tốc độ gate, dọn worktree 24/7, guard mới cho Zero-Touch `main`, nới false-positive có thiết kế giữ nguyên `SECURITY.md` |

**ID và Khoá:**
- **ID** = `<CAT>-<NNN>`, với `NNN` là số tăng dần toàn cục lấy từ `docs/audit/ledger.md` (ví dụ `GATE-007`, `RFC-022`).
- **Khoá** = `<đường dẫn file hoặc phạm vi>::<tên fn/impl hoặc chủ đề>::<CAT>`. Hai phát hiện trùng nhau ⇔ trùng Khoá, bất kể số dòng.

**Mức độ:**
- **Critical:** fail-open hoặc lách guard, mà agent trong sandbox với tới được.
- **High:** chặn oan trên đường phổ biến, mất dữ liệu, hoặc task kẹt không tự gỡ được (hoặc RFC có Impact cao rõ rệt).
- **Medium:** sai ở đường hiếm, hoặc chậm ≥ 10% có số đo.
- **Low:** phần còn lại. Không có `Đường tới` (Mẫu A) thì tối đa Medium.

**Trạng thái trong ledger** (giữ nguyên đúng 11 cột, một cột Trạng thái duy nhất):
`giả thuyết → xác minh → duyệt | chờ User → đang sửa → đạt-chờ merge → xong`
- Nhánh rẽ: `loại (<lý do>)`, `trùng (<ID>)`, `hoãn`.
- `giả thuyết` sau 2 vòng không ai xác minh → `loại (hết hạn)`. **Mục `chờ User` (bao gồm `RFC-<NNN>` Kênh B) KHÔNG tự hết hạn sau 2 vòng và KHÔNG chặn điều kiện bão hoà lane.**
- Mỗi mã bị trả việc tối đa 2 lượt; lượt thứ 3 → `hoãn` và báo User. Hồi quy sau khi `xong` → mở mã mới trỏ về mã cũ.

**Bảng Lane (chủ sở hữu duy nhất, phủ 100% phạm vi lệnh `git ls-files` bên dưới):**

| Lane | Phạm vi (disjoint) | Câu hỏi chính |
|---|---|---|
| `gate-ok` | `crates/aaf-fp/src/**`, `E/gate/{head_lock,hidden,static_checks,changes,base,oracle,oracle_classify,oracle_worktree,oracle_worktree_lexer}.rs`, `E/gate/changes/**`, `E/gate/tests/**` | Có nhánh lỗi nào ra CLEAR không? Dương tính giả nào? `tree_fp` / `oracle` có điểm mù không? |
| `gate-fast` | `E/gate/{runner,proc,docs_fast,reuse,mod,gate_helpers}.rs`, `E/gate/docs_fast/**`, `E/factory/{batch,commit_lock,finish,finish_land}.rs`, `E/factory/finish/**` | Bước nào chạy thừa? Receipt/warm-slot tái dùng được nhiều hơn mà vẫn đúng không? Chia đôi lô / landing có chạy thừa hay để rác không? |
| `core` | `crates/aaf-domain/src/**`, `E/config/**`, `E/git/**`, `E/lib.rs`, `E/factory/{mod,ports,adapters,base_ref,git_real,git_real_base,snapshots,prompt}.rs`, `E/factory/git_real/**`, `E/factory/tests/**` | Luật tầng config / `[worktree]` Tier-2 có bị lách không? Mọi task có bắt buộc chạy trên worktree riêng (lỗi engine fallback chạy thẳng trên `main` → `[CH-A]`)? Git/hydration/warm-slot/merge có chạy thừa hay rò worktree/rác `main` không? |
| `throttle` | `E/throttle/**`, `E/process.rs`, `crates/aaf-cli/src/cmd/slot.rs` | Thời gian chờ slot, p75 peak, rò token/khoá, tiến trình mồ côi, tín hiệu |
| `auto` | `E/unblocker/**`, `E/factory/{worker,unblock,scheduler,schedule_order,select_pool,sweep,post_audit,post_audit_helpers,audit_backend,audit_backend_policy,audit_turn,audit_turn_watch,rework_filter}.rs`, `E/factory/scheduler/**`, `E/factory/unblock/**`, `E/detector/**`, `E/cross_audit/**`, `E/accounts/**`, `E/roles/**` | Vì sao task vào blocked? `extract_gate_diagnostics` & thang T1–T4 có gỡ được nhiều hơn không? Có retry/LLM vô ích hay tắc xoay account không? |
| `hook` | `crates/aaf-hook/src/**` | Lệnh hợp lệ nào bị chặn oan (`[CH-B]`)? Còn đường lách guard hiện có (`[CH-A]`)? Thêm guard mới chặn sửa/commit thẳng lên `main`/`master`/`develop` ngoài worktree riêng (`[CH-B]`)? |
| `sandbox` | `E/executors/**`, `E/hook_settings.rs`, `E/hook_settings/**` | Seatbelt / `bwrap` có chặn oan build hợp lệ hay thiếu parity macOS↔Linux không? Worker sandbox có thể ghi ngược ra checkout `main` gốc **hoặc** `<main>/.git/{HEAD,index,packed-refs,refs/heads/<protected>}` không? Env và bí mật có lọt không? |
| `cli` | `crates/aaf-cli/src/**` (trừ `cmd/slot.rs`), `E/telegram/**`, `E/web_ui/**` (gồm `index.html`), `E/sop/**`, `E/updater/**`, `packaging/**`, `scripts/**`, `.github/workflows/**`, `Cargo.toml`, `crates/*/Cargo.toml`, `clippy.toml`, `rust-toolchain.toml` | Lỗi khó hiểu, cờ CLI rườm rà, `--json` thiếu trường, `doctor`/`run`/`init` & `aaf sop run` (`Gate`/`read_only` trên `repo_root`) có làm bẩn `main` không (lỗi fallback → `[CH-A]`, guard/chính sách mới → `[CH-B]`), `ui`/`install`/Docker/CI manifest thiếu kiểm? *(Chuẩn hoá `E/sop/**` thuộc `cli`; đồng bộ `docs/audit/ledger.md` ở vòng kế tiếp nếu còn ghi `auto`.)* |
| `store` | `E/store_compliance/**` | CRITICAL dương tính giả, trường hợp `unresolved` bị im lặng |
| `obs` | `E/{telemetry,watch,webhook,plan_mode}.rs`, `E/gc.rs`, `E/gc/**`, `E/skills*.rs`, `templates/skills/**/*.md`, `E/db/**`, `E/handover*.rs`, `E/proof*.rs`, `E/github_board*.rs`, `E/split*.rs` | Có đo được tỉ lệ tự chủ/gate/token không? DB/log/`gc`/worktree có phình hay rò 24/7 không? `templates/skills/` có lệch sự thật CLI / thiếu cấm host-cmd (`[CH-A]`) hay cần chính sách Zero-Touch `main` mới ở `aaf-workflow` (`[CH-B]`) không? |
| `wip` | File chưa commit (`git status`) | Chỉ quét khi User yêu cầu |

**Kiểm độ phủ fail-closed mức file mỗi vòng (chạy trên `$SCAN_WT` hoặc `--cached` của `HEAD`, tránh hút file WIP chưa commit trên `main`):**
```bash
git -C /Volumes/Data/Toan/AutoAgentFactory ls-files --cached -- \
  ':(glob)crates/*/src/**/*.rs' \
  ':(glob)crates/*/src/**/*.html' \
  ':(glob)templates/skills/**/*.md' \
  ':(glob)packaging/**' \
  ':(glob)scripts/**' \
  ':(glob).github/workflows/**' \
  'Cargo.toml' \
  ':(glob)crates/*/Cargo.toml' \
  'clippy.toml' \
  'rust-toolchain.toml' | sort
```
Xác nhận kết quả khác rỗng (`count > 0`) và mọi file thuộc đúng `1` lane. File mới ngoài bảng phải phân bổ ngay vào lane phù hợp và ghi vào `docs/audit/ledger.md`.

---

## 3. Vòng chạy (Bước 0 → 7)

### Bước 0 — Khôi phục và dọn
1. Chụp trạng thái `$REPO` trước vòng (`BASE_BRANCH=$(git -C "$REPO" branch --show-current)`, `MAIN_HEAD_BEFORE=$(git -C "$REPO" rev-parse HEAD)`, `MAIN_STATUS_BEFORE=$(git -C "$REPO" status --porcelain -- ':!docs/audit/')`, `MAIN_DIFF_BEFORE=$(git -C "$REPO" diff -- ':!docs/audit/' | shasum -a 256)`) để xác minh Zero-Touch `main` ở Bước 7 và dùng `"$BASE_BRANCH"` ở các bước sau.
2. Đọc `docs/audit/ledger.md`. Mã nào đang ở **pha giữa (`xác minh | duyệt | đang sửa`)** thì tiếp tục pha đó trước; không mở lane mới khi vòng cũ còn việc dở *(mục `chờ User` và `giả thuyết` KHÔNG phải pha giữa, không chặn mở vòng mới)*.
3. Chạy `git -C "$REPO" branch --list 'audit/*' --no-merged "${BASE_BRANCH:-main}"` **và** kiểm tra mọi tên nhánh trong cột `Nhánh/commit` của các dòng `đạt-chờ merge` (kể cả `fix/*`):
   - Mã `đạt-chờ merge` chưa merge: nhắc User merge; chưa merge thì không quét lane chứa Khoá của mã đó.
   - Nhánh đã merge: `git worktree remove <thư mục>`, `git branch -d <nhánh>`, đổi mã sang `xong`.
4. Lần đầu chạy trên repo mới: hỏi User repo đích đo gate & cách xử lý `docs/audit/` rồi ghi vào ledger.
5. Nếu trong `docs/prompts/` có backlog prompt mới chưa seed trong `docs/audit/ledger.md` thì nạp Khoá vào ledger để tránh báo trùng.

### Bước 1 — Đo hiện trạng (trên worktree tạm cô lập — KHÔNG chạy `cargo` trên checkout `main`)
Nếu `HEAD` trùng dòng cuối của `docs/audit/metrics.md` thì chép lại số. Nếu không, tạo worktree tạm tại `$HEAD` dưới `/Volumes/Data/Toan/.aaf-audit-tmp/`:

```bash
set -euo pipefail
REPO="/Volumes/Data/Toan/AutoAgentFactory"
SHA=$(git -C "$REPO" rev-parse --short HEAD)
BASE_WT="/Volumes/Data/Toan/.aaf-audit-tmp/aaf-base-${SHA}-$$"
BASE_TGT="/Volumes/Data/Toan/.aaf-audit-tmp/target-base-${SHA}"
trap 'rm -f "${out:-}" "${clip_out:-}"; git -C "$REPO" worktree remove --force "$BASE_WT" 2>/dev/null || true' EXIT
git -C "$REPO" worktree add --detach "$BASE_WT" "$SHA"
[ -d "$BASE_TGT" ] || ([ -d "$REPO/target" ] && cp -cR "$REPO/target" "$BASE_TGT" || mkdir -p "$BASE_TGT")

cd "$BASE_WT"
memory_pressure | tail -1
sysctl -n kern.memorystatus_vm_pressure_level   # 1=Normal, 2=Warn, 4=Critical
out=$(mktemp)
test_exit=0
CARGO_TARGET_DIR="$BASE_TGT" AAF_REQUIRE_REAL=1 aaf slot --label cargo-test-ws -- python3 -c 'import os, signal; signal.signal(signal.SIGHUP, signal.SIG_DFL); os.execvp("sh", ["sh", "-c", "cargo test --workspace --no-run -q && /usr/bin/time -p cargo test --workspace --no-fail-fast"])' > "$out" 2>&1 || test_exit=$?
echo "test_exit=$test_exit"
awk '/^test result/{p+=$4;f+=$6;i+=$8} END{print "pass="p, "fail="f, "ignored="i}' "$out"
grep -c 'SKIPPED ' "$out" || true
grep '^real' "$out" || true
rm -f "$out"
clip_out=$(mktemp)
clippy_exit=0
CARGO_TARGET_DIR="$BASE_TGT" aaf slot --label cargo-clippy -- cargo clippy --workspace --all-targets --message-format=short -- -D warnings > "$clip_out" 2>&1 || clippy_exit=$?
echo "clippy_exit=$clippy_exit"
grep -cE ': (warning|error):' "$clip_out" || true
rm -f "$clip_out"
# Chạy trong $REPO (mở DB chỉ đọc; chạy trong $BASE_WT sẽ lệch repo_root → trả 0) và lọc qua python3 để không lộ accounts/runner.log_path:
(cd "$REPO" && aaf status --json) | python3 -c 'import json,sys; d=json.load(sys.stdin); t=d.get("tasks",{}); br=sorted((d.get("blocked_reasons") or {}).items(), key=lambda kv:(-kv[1],kv[0]))[:3]; print(json.dumps({"task_done":t.get("done",0),"task_blocked":t.get("blocked",0),"blocked_top3":br}))'
```

Ghi 1 dòng vào `docs/audit/metrics.md`:
`ngày | HEAD | mem_pressure | test_pass | test_fail | test_skipped | test_time_s | clippy_warn | gate_scoped_ms | gate_full_ms | task_done | task_blocked | blocked_top3 | báo | xác_minh | loại | trùng | ghi chú`

- Cột `báo | xác_minh | loại | trùng` **CHỈ đếm Kênh A** (ghi tổng hợp Kênh B dạng `RFC: <pass>/<total>` vào cột `ghi chú` để không làm lệch `Precision`).
- `test_skipped > 0` hoặc `kern.memorystatus_vm_pressure_level != 1`: không so `test_pass` với vòng trước. Lệnh nào không chạy được ghi `"không đo được — <lý do>"`.
- **Đo gate** (chỉ khi vòng có lane `gate-fast` hoặc mã vừa merge chạm `E/gate/**` hay `E/factory/{batch,finish,finish_land}.rs`): máy phải rảnh (`Normal`, 0 slot nặng), tạo worktree tạm của repo đích dưới `/Volumes/Data/Toan/.aaf-audit-tmp/`, mỗi lần đo thêm `aaf-bench-<n>.tmp` (không dùng `.md`), 1 lần làm nóng + 5 lần `aaf gate --json` & 5 lần `aaf gate --full --json`, bỏ mẫu khác `CLEAR` / `reused_receipt: true` / `docs_fast_path: true`, lấy trung vị, xoá worktree tạm.

### Bước 2 — Chọn lane (tối đa 3)
Thứ tự ưu tiên: (1) Chỉ số xấu đi → (2) Nguyên nhân đầu `blocked_top3` → (3) Lane lâu nhất chưa quét.
- **Bỏ qua lane bão hoà:** `git diff --quiet <HEAD lần quét trước> HEAD -- <phạm vi lane>` exit 0 **và** lần quét trước có 0 mã Kênh A `≥ Medium` xác minh (mục `chờ User` Kênh B không chặn bão hoà).
- **Vòng rỗng hợp lệ:** không có lane cần quét, không có mã pha giữa, chỉ số không xấu đi → báo `"vòng rỗng"` rồi dừng.

### Bước 3 — Quét song song qua `claude -p` (Mẫu A)
1. Viết `docs/audit/<YYYY-MM-DD>/<NN>-<lane>-scan.md` theo Mẫu A ([mode1-templates.md](mode1-templates.md)) kèm mọi Khoá đã biết trong ledger (kể cả `loại` và `chờ User` / `RFC`).
2. Tạo worktree sạch `SCAN_WT="/Volumes/Data/Toan/.aaf-audit-tmp/aaf-scan-${SHA}-$$"` (`trap 'git worktree remove --force "$SCAN_WT"' EXIT`).
3. Thu giữ im lặng `CLAUDE_CONFIG_DIR=$(cd "$REPO" && aaf accounts next --json | python3 -c 'import json,sys; print((json.load(sys.stdin).get("account") or {}).get("config_dir",""))')` (không `echo`) và chạy song song tối đa 3 lane trong `$SCAN_WT` bằng `claude -p --model opus --effort high --no-session-persistence --tools "Read,Grep,Glob,Bash" --allowedTools "Read,Grep,Glob,Bash(git grep:*)"`, đổ stdout vào `docs/audit/<YYYY-MM-DD>/<NN>-<lane>-report.md` và stderr vào `/Volumes/Data/Toan/.aaf-audit-tmp/<NN>-<lane>.stderr.log`.
4. Xoá `$SCAN_WT`, đi thẳng sang Bước 4.

### Bước 4 — Kiểm báo cáo, Xác minh chéo (4b), Red-Team Kênh B & Duyệt (4c)
1. **Kiểm trích dẫn:** `git grep -nF -e "<dòng đầu Trích nguyên văn>" <sha> -- <file>`. Sai hoặc lệch > ±5 dòng → `loại (trích sai)`.
2. **Kiểm trùng & phân kênh:** trùng Khoá → `trùng (<ID>)`. Áp dụng **Cổng Kênh A 6 điểm** ở `SKILL.md §2`:
   - **Kênh A (`[CH-A]`):** Low → ghi `giả thuyết`, không chọn sửa. `≥ Medium` → Bước 4b.
   - **Kênh B — Strategic RFC hoặc lỗi chạm Vùng cấm (`[CH-B]`, `RFC-<NNN>` hoặc `<CAT>-<NNN>` gắn `[Kênh B]`):** KHÔNG vào Bước 4b test đỏ. Nếu có `>= 1` mục Kênh B, PM đọc [channel-b-redteam.md](channel-b-redteam.md) và chạy **Red-Team Sanity Check** (10 điểm Security + 5 điểm State-Machine). Đạt `PASS` → ghi ledger với trạng thái `chờ User`, cột `Oracle` = `— (Kênh B RFC)`, cột `Ghi chú` = `[Kênh B] <Impact × Effort + tóm tắt>`. Bị `REJECT` → `loại (red-team: <lý do>)` (không tính vào `Precision` Kênh A).
3. **Bước 4b — Xác minh chéo Kênh A (Mẫu V, model KHÁC thợ quét):**
   - `GATE/SEC/COR/RES/AUTO/TST` (và Kênh A `DOC` ở `templates/skills/**/*.md` qua `E/skills/tests.rs`): viết test trên worktree `aaf-verify-<ID>` (chỉ sửa file test), phải **ĐỎ ở assertion** (lỗi biên dịch không tính).
   - `FAST/PERF`: PM đo 1 làm nóng + 5 lần, lấy trung vị.
   - `DEL`: `git grep -nw <symbol>` không còn caller + `cargo check --workspace --all-targets` xanh khi xoá thử.
   - `OBS/DOC` khác: output lệnh thực tế chứng minh.
   - PM tự chạy lại, dán bằng chứng vào ledger; đóng băng oracle bằng `git hash-object <file test>`. Không tái hiện → `loại (không tái hiện)` và xoá `aaf-verify-<ID>`.
4. **Bước 4c — Duyệt & Tái nhập `Approve: <ID>`:**
   - Tối đa **5 mã Kênh A/vòng** (ưu tiên theo `SKILL.md §1` → mức → chi phí thấp; Medium chỉ chọn khi ảnh hưởng mục tiêu 1–4). PM tự duyệt ⇔ đã xác minh và vượt qua Cổng Kênh A 6 điểm; nếu mới phát hiện chạm Vùng cấm ở Bước 4b → hạ sang `chờ User` (`Approve: <ID>`).
   - **Khi User gõ `Approve: <ID>` cho một mã `chờ User`:** nếu chưa có oracle đóng băng (ví dụ RFC hoặc lỗi Vùng cấm bỏ qua 4b) → chạy Bước 4b (xác minh & đóng băng oracle) → Bước 5 → Bước 6 (bắt buộc Bước 6.5 phản kiểm Mẫu C).

### Bước 5 — Sửa Kênh A (Mẫu B)
- PM tạo trước worktree `git -C "$REPO" worktree add "/Volumes/Data/Toan/.aaf-audit-tmp/aaf-<ID>" -b "audit/<ID>" "$SHA"`, copy file oracle `<file test>` đã đóng băng từ `aaf-verify-<ID>` vào (xác minh `git hash-object` khớp `<hash>`), rồi `git -C "$REPO" worktree remove --force "/Volumes/Data/Toan/.aaf-audit-tmp/aaf-verify-<ID>"`.
- Viết `docs/audit/<YYYY-MM-DD>/<NN>-<ID>-fix.md` theo Mẫu B; ghi rõ file oracle + hash CẤM sửa; các mã chạm cùng file xếp nối tiếp.
- Chạy 2 bước `claude -p --model opus --effort high` trong `aaf-<ID>` với `CARGO_TARGET_DIR` riêng:
  - **Pha 1 (chỉ đọc + chạy oracle):** `claude -p --model opus --effort high --output-format json --tools "Read,Grep,Glob,Bash" --allowedTools "Read,Grep,Glob,Bash" < fix.md` → lấy `session_id`, PM soát phản biện.
  - **Pha 2 (thực hiện):** `claude -p --resume "$SESSION_ID" --permission-mode acceptEdits --allowedTools "Read,Edit,Write,Grep,Glob,Bash" "Làm"` (commit 1 commit gồm oracle đóng băng + sửa prod tối thiểu).

### Bước 6 — Nghiệm thu từng mã (PM tự làm)
1. **Soát diff (`git diff <base>...audit/<ID>`):** trả việc ngay nếu đổi hash oracle, xoá `assert`/`#[test]`, lén thêm `#[ignore]`/`should_panic`/`aaf:allow(`, sửa file ngoài phạm vi, vi phạm Cổng Kênh A 6 điểm, bất kỳ file `.rs` nào bị sửa `wc -l > 600`, hoặc diff `> 400` dòng.
2. **Đỏ – xanh – đỏ** (worktree + `CARGO_TARGET_DIR` riêng):
   - (a) Base + `git checkout audit/<ID> -- <file test>` → ĐỎ ở assertion.
   - (b) Nhánh `audit/<ID>` → XANH.
   - (c) Nhánh + `git checkout <base> -- <file prod>` → ĐỎ lại.
3. Trong worktree nhánh: `cargo fmt --all --check`, `aaf slot --label clippy-pkg -- cargo clippy -p <crate> --all-targets -- -D warnings`. Khi chạy `cargo test -p <crate>`:
   - Nếu `<crate>` là `aaf-cli`: **bắt buộc** `AAF_REQUIRE_REAL=1 aaf slot --label test-cli -- python3 -c 'import os, signal; signal.signal(signal.SIGHUP, signal.SIG_DFL); os.execvp("cargo", ["cargo", "test", "-p", "aaf-cli"])'`.
   - Nếu sửa `templates/skills/**/*.md`: chạy `cargo test -p aaf-engine` **và** `aaf slot --label build-bins -- cargo build --workspace --bins` (do `include_str!`).
4. **Mã `FAST`/`PERF`:** build binary nhánh, gọi đường dẫn tuyệt đối + data dir tạm, trung vị giảm `≥ 10%` **và** `max-sau < min-trước`.
5. **Phản kiểm (Mẫu C, model KHÁC thợ sửa):** bắt buộc với lane `gate-ok`, `hook`, `sandbox`, `auto`; mã `SEC`/`GATE`; mọi bản sửa được User duyệt nới guard; và mọi sửa chạm vùng bảo mật trọng yếu ở `core`/`cli` (`E/git/{hydration,warm_slot*,pr_merge*,merge_queue}`, `E/config/{load,merge*}`, `E/sop`, `E/web_ui`, `E/telegram`, `E/proof*`, `E/handover*`, Tier-A CLI handlers). Có finding hợp lệ → trả việc.
6. Đạt → `đạt-chờ merge`. Không đạt → trả việc kèm `file:dòng — sai gì`.

### Bước 7 — Tích hợp cuối vòng (trên worktree tích hợp cô lập)
1. Tạo worktree tích hợp riêng (**KHÔNG `git checkout` trên `$REPO`**):
   `git -C "$REPO" worktree add "/Volumes/Data/Toan/.aaf-audit-tmp/aaf-integration-<YYYY-MM-DD>" -b "audit/integration-<YYYY-MM-DD>" "${BASE_BRANCH:-main}"` (kèm `CARGO_TARGET_DIR` riêng dưới `/Volumes/Data/Toan/.aaf-audit-tmp/`), rồi merge mọi nhánh `đạt-chờ merge` vào worktree đó.
2. Chạy trong worktree tích hợp:
   - `AAF_REQUIRE_REAL=1 aaf slot --label repo-rules -- cargo test -p aaf-cli --test repo_rules` (**100% PASS, `0 failed, 0 ignored`**).
   - `AAF_REQUIRE_REAL=1 aaf slot --label cargo-test-ws -- python3 -c 'import os, signal; signal.signal(signal.SIGHUP, signal.SIG_DFL); os.execvp("cargo", ["cargo", "test", "--workspace", "--no-fail-fast"])'` (**`0 failed, 0 SKIPPED`, `ignored == 8` trên macOS / `6` trên Linux** khớp đúng 8 test `#[ignore]` quy định ở `SKILL.md §3`).
   - Đỏ thật → chia đôi nhánh tìm thủ phạm.
3. Chạy thật theo `docs/PLAN.md §E` nếu chạm `aaf-hook` (mục 2), `E/gate`/`E/store_compliance` (mục 3), `E/executors`/`E/factory/worker.rs` (mục 6). Đổi threat model → cập nhật `SECURITY.md`.
4. Xác minh Zero-Touch `main` (`git -C "$REPO" rev-parse HEAD == "$MAIN_HEAD_BEFORE"`, `git -C "$REPO" status --porcelain -- ':!docs/audit/' == "$MAIN_STATUS_BEFORE"`, `git -C "$REPO" diff -- ':!docs/audit/' | shasum -a 256 == "$MAIN_DIFF_BEFORE"`), ghi `metrics.md` & `ledger.md`, báo User gọn: lane đã quét, `báo / xác minh / loại / trùng` (Kênh A), danh sách `đạt-chờ merge` + tên nhánh, chỉ số trước/sau, và mục **`Strategic RFC / Chờ User` (Kênh B)** đã qua Red-Team theo mẫu 4 phần.

---

## 4. Chọn model & Độ tin cậy (Chế độ 1)

- Mặc định quét/sửa dùng `claude -p --model opus --effort high` kèm `CLAUDE_CONFIG_DIR` thu giữ im lặng từ `$REPO` theo §1 (không `echo` stdout).
- Thợ xác minh (Mẫu V) ≠ model thợ quét (Mẫu A); thợ phản kiểm (Mẫu C) ≠ model thợ sửa (Mẫu B).
- `Precision` của model trên lane = `xác_minh / (xác_minh + loại)` (**tính riêng Kênh A, loại trừ mọi dòng `RFC-*`**). Nếu `< 50%` trong 2 vòng liên tiếp → đổi model cho lane đó.

## 5. Cấu trúc `docs/audit/ledger.md`

```markdown
## Cấu hình
- Repo đích đo gate: <đường dẫn> (User chọn ngày <…>)
- docs/audit: <commit hay ignore> (User chọn ngày <…>)

## Lane
| Lane | Lần quét gần nhất | HEAD | Phần chưa đọc |

## Mã
| ID | Khoá | Lane | Mức | Mục tiêu | Trạng thái | Model quét | Model xác minh | Oracle (file + hash) | Nhánh/commit | Ghi chú |
```

Giữ nguyên đúng **11 cột** để tương thích lịch sử. Không xoá dòng nào. Mã bị loại vẫn giữ lại để vòng sau không báo trùng; mã Kênh B dùng `RFC-<NNN>`, `Trạng thái = chờ User`, `Oracle = — (Kênh B RFC)`.
