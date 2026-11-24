# Agent 10 — Live Adversarial E2E, Benchmark & Soak Suites (Suites 1–7)

> Chỉ đọc khi Coordinator chuẩn bị brief cho **Agent 10** trong [mode2-convergence-15.md](mode2-convergence-15.md).

---

## 1. Quy tắc Cô lập Tuyệt đối & Wrapper `env -i` (BẮT BUỘC cho mọi Suite 1–7)

Agent 10 chỉ được tạo/xoá file dưới `E2E_DIR="/Volumes/Data/Toan/.aaf-audit-tmp/e2e-r<N>-$$"` (`trap 'rm -rf "$E2E_DIR"' EXIT`). **CẤM sửa file trong `$WT` hay `$REPO`.**

Trước khi chạy Suite 1 và sau khi kết thúc Suite 7, ghi nhận và đối chiếu `git -C "$REPO" worktree list` phải **giống hệt từng byte** (không đụng repo thật).

**Mọi fixture `git init` (Suites 2, 6, 7) BẮT BUỘC cài git identity cục bộ trên repo** (`git config user.name aaf-e2e && git config user.email aaf-e2e@example.invalid`), vì `run_isolated()` tắt toàn bộ global/system git config và `RealGit::commit_all`/`rebase`/`merge` kế thừa config cục bộ của fixture cho mọi linked worktree.

**BẮT BUỘC dùng wrapper `env -i` trong outer harness `bash`/`zsh` (KHÔNG chạy trong POSIX `/bin/sh`, và KHÔNG gõ `env -i ... aaf` trực tiếp trên dòng bash để tránh `aaf-hook`'s `human_action_guard` chặn nhầm):**

```bash
REAL_HOME="$HOME"
AAF_BIN="$TGT/debug/aaf"
HOOK_BIN="$TGT/debug/aaf-hook"
# Loại bỏ $REPO, $WT và mọi thư mục con bên trong khỏi PATH (SECURITY.md §8):
SAFE_PATH=$(python3 -c 'import os,sys; drop=[os.path.realpath(sys.argv[1]), os.path.realpath(sys.argv[2])]; out=[];
for p in os.environ.get("PATH","").split(":"):
    if not p: continue
    rp=os.path.realpath(p)
    if not any(rp==d or rp.startswith(d+os.sep) for d in drop): out.append(p)
print(":".join(out))' "$REPO" "$WT")

mkdir -p "$E2E_DIR"/{home,data,worktrees,config,warm-slots,tmp,xdg-cfg,xdg-data,xdg-cache}
# Tắt throttle CPU host để benchmark/soak không bị chờ tải ngẫu nhiên (vẫn giữ memory pressure thật):
printf '[machine]\nmax_load_percent = 0\n' > "$E2E_DIR/config/config.toml"

run_isolated() {
  # Dùng: run_isolated [--with-rust] <script.sh>
  # CHỈ truyền --with-rust cho Suite 6 (giữ Suites 1–5 & 7 tách hoàn toàn khỏi $REAL_HOME)
  local with_rust=0
  if [ "${1:-}" = "--with-rust" ]; then with_rust=1; shift; fi
  local rust_env=()
  if [ "$with_rust" = "1" ]; then
    [ -d "${RUSTUP_HOME:-$REAL_HOME/.rustup}" ] && rust_env+=(RUSTUP_HOME="${RUSTUP_HOME:-$REAL_HOME/.rustup}")
    [ -d "${CARGO_HOME:-$REAL_HOME/.cargo}" ] && rust_env+=(CARGO_HOME="${CARGO_HOME:-$REAL_HOME/.cargo}")
  fi
  env -i \
    PATH="$SAFE_PATH" \
    USER="${USER:-aaf}" \
    LOGNAME="${LOGNAME:-aaf}" \
    LANG="C.UTF-8" \
    HOME="$E2E_DIR/home" \
    AAF_DATA_DIR="$E2E_DIR/data" \
    AAF_WORKTREE_DIR="$E2E_DIR/worktrees" \
    AAF_CONFIG_DIR="$E2E_DIR/config" \
    AAF_WARM_SLOTS_DIR="$E2E_DIR/warm-slots" \
    TMPDIR="$E2E_DIR/tmp" \
    XDG_CONFIG_HOME="$E2E_DIR/xdg-cfg" \
    XDG_DATA_HOME="$E2E_DIR/xdg-data" \
    XDG_CACHE_HOME="$E2E_DIR/xdg-cache" \
    GIT_CONFIG_NOSYSTEM=1 \
    GIT_CONFIG_GLOBAL=/dev/null \
    ${rust_env[@]+"${rust_env[@]}"} \
    AAF_BIN="$AAF_BIN" \
    HOOK_BIN="$HOOK_BIN" \
    E2E_DIR="$E2E_DIR" \
    /bin/sh "$1"
}
```
*Bảo đảm vắng mặt hoàn toàn:* `CLAUDE_CONFIG_DIR`, `AAF_RUN_ID`, `AAF_HEAVY_SLOT`, `CARGO_TARGET_DIR`, `RUSTC_WRAPPER`, `SCCACHE_*`, và mọi biến `AAF_*` môi trường ngoài. **CẤM** gọi `aaf` hay `aaf-hook` từ `$PATH` bên trong các suite (chỉ dùng `"$AAF_BIN"` / `"$HOOK_BIN"`).

---

## 2. 7 Suite Bắt Buộc

### Suite 1 — Hook Adversarial
- Chạy lại toàn bộ bypass vòng trước + 3–5 probe sáng tạo thực tế thuộc threat model `SECURITY.md` → **MUST DENY (`rc=2`)**.
- Chạy tập lệnh dev bình thường → **MUST ALLOW (`rc=0`)**.

### Suite 2 — Micro-Latency Benchmark (median 20 runs)
- Repo `git init` thật trong `$E2E_DIR/bench-micro` (`core.bare=false`, local git `user.name`/`user.email`, `.gitignore` tracked ignore `target/`), gate cấu hình lệnh `true`.
- Đo trung vị 20 lần: `aaf-hook` allow/deny, `aaf gate --json` cold & warm, `aaf doctor --json`.

### Suite 3 — Cross-repo & Guard E2E
- `aaf sop approve` & `aaf sop send-back` từ Repo B sang checkpoint Repo A → **fail-closed (`rc != 0`, SHA-256 checkpoint giữ nguyên)**.
- Same-repo & outside-repo hợp lệ → `rc = 0`.

### Suite 4 — Slot Concurrency & Signal Cleanup
- Nested `aaf slot` + gửi `SIGTERM` → kiểm `ps -eo pid,ppid,pgid,stat,command | grep "$E2E_DIR" | grep -v grep` **rỗng (0 orphan)**.
- Stress song song `aaf-hook` / `aaf slot`.

### Suite 5 — Web UI SSE E2E
- `/api/status`: trả `401` khi thiếu token, `200` khi đủ token.
- `/api/events`: nhận frame `event: status` đầu tiên.
- Gửi `SIGTERM`: thoát sạch `< 1s` (**0 orphan**).

### Suite 6 — Real-Workload Gate Benchmark (`cold` vs `warm-incremental` vs `warm-receipt` vs `hydrated`)
- **Điều kiện chạy:** kiểm `sysctl -n kern.memorystatus_vm_pressure_level == 1` (Normal). Bọc **phía ngoài** qua script thực thi với **host `aaf slot --label e2e-bench -- /bin/bash "$E2E_DIR/run-suite6.sh"`** (trong đó `run-suite6.sh` gọi `run_isolated --with-rust "$E2E_DIR/suite6.sh"` — **KHÔNG** truyền thẳng hàm shell `run_isolated` vào `aaf slot` vì `aaf slot` gọi `Command::new` trực tiếp sẽ exit 127, và **KHÔNG** bọc `"$AAF_BIN" slot` quanh `"$AAF_BIN" gate` trên cùng `$E2E_DIR/data` vì sẽ tự deadlock).
- **Fixture 100% offline, deterministic (`< 30s`):** tạo Rust workspace 2 crate trong `$E2E_DIR/bench-rust` (`git init -b main`, local git `user.name`/`user.email`, `crates/lib` + `crates/app` phụ thuộc `lib = { path = "../lib" }`, 0 tải mạng; `.gitignore` tracked chứa `/target`; chạy `"$AAF_BIN" init` tự detect profile `rust`; commit `.autofactory/config.toml` + `.gitignore` + `.claude/settings.json`).
- **Đo đủ 4 mốc rõ ràng theo đúng thứ tự:**
  1. **`cold`:** chưa có `target/`, chưa có receipt (CHƯA cấu hình `[worktree]`) → `"$AAF_BIN" gate --scoped --json` → bắt buộc `status == "CLEAR"`, `reused_receipt == false`, `docs_fast_path == false`. *(Lượt này tự làm nóng cả `clippy` lẫn `test` vào `target/` của checkout chính.)*
  2. **`warm-incremental`:** sửa 1 dòng comment/hàm trong `crates/app/src/lib.rs` (giữ `target/`, làm mất hiệu lực receipt, không rơi vào `docs_fast_path`) → `"$AAF_BIN" gate --scoped --json` → `status == "CLEAR"`, `reused_receipt == false`.
  3. **`warm-receipt`:** chạy lại ngay trên cây chưa đổi → `"$AAF_BIN" gate --scoped --json` → `status == "CLEAR"`, `reused_receipt == true`.
  4. **`hydrated` (`[worktree]` APFS CoW thực tế qua `aaf run`):** trước hết `git checkout -- crates/app/src/lib.rs` (hoặc commit thay đổi Bước 2 để `main` sạch 100%), thêm `/.env.gate` vào `.gitignore` tracked, tạo file `.env.gate` trên đĩa checkout chính, commit vào `main`:
     - `[worktree]` với `clone_dirs = ["target"]` và `gate_copy_files = [".env.gate"]`
     - `[run]` với `executor = "custom"` và `executor_cmd = ["/bin/sh", "-c", "echo '// touch' >> crates/app/src/lib.rs", "{prompt}"]`
     - Tạo 1 task (`board add`), chạy **`"$AAF_BIN" run --land branch --unsandboxed --json`** (KHÔNG dùng `--auto-merge` để giữ nguyên worktree & receipt sau khi chạy).
     - **Đọc `commands[*].duration_ms` bằng cách giải mã 2 lớp phong bì `Signed`** tại `$AAF_DATA_DIR/receipts/<wt_repo_id>/latest.json` (với `<wt_repo_id> = sha256(realpath(TASK_WT))[:16]` hoặc khớp `json.loads(env["payload"])["repo_root"] == os.path.realpath(TASK_WT)` → `sum(c["duration_ms"] for c in json.loads(env["payload"])["commands"])`; **KHÔNG** đọc từ `aaf handover --format json` vì `HandoverData` không có `commands`/`duration_ms`).
     - Đối chiếu với lượt đối chứng không hydrate (`clone_dirs = []`) và mốc `cold`; xác minh: `hydrated_gate_ms < cold_gate_ms`, `[ -d "$TASK_WT/target" ]`, **`[ ! -e "$TASK_WT/.env.gate" ]`** (xác minh RAII `GateSecretStage` đã dọn sạch sau gate), và `[ -f "$E2E_DIR/bench-rust/.env.gate" ]` (bản gốc vẫn còn).

### Suite 7 — Worktree, Zero-Touch `main`/`develop` & Cleanup Soak
- **Offline custom executor & config fixture đầy đủ:** trong `.autofactory/config.toml` của mỗi fixture, commit đầy đủ:
  - `[commands]` với `lint = "true"` và `test_scoped` kiểm tra 1 file toggle offline (ví dụ `test ! -e "$E2E_DIR/fail.flag"` để Bước 1 pass và Bước 3 fail xác định, tránh lỗi `GateStatus::Unverified` do thiếu `test_scoped`)
  - `[unblocker]` với `max_unblock_cycles = 1` (để Bước 3 chuyển thẳng sang `Blocked` xác định thay vì lặp 10 vòng)
  - `[run]` với `executor = "custom"` và `executor_cmd = ["/bin/sh", "-c", "<script>", "{prompt}"]` (chạy `--unsandboxed` cho chuỗi soak chính + **1 lượt `custom` CÓ sandbox trên macOS** bỏ `--unsandboxed` để kiểm `$AAF_DATA_DIR/marks/` & Seatbelt write-deny).
- **Chạy trên 2 fixture tách biệt trong `$E2E_DIR`** (mỗi fixture cài local `git config user.name/user.email`):
  - **(A) `soak-main`:** `git init -b main` (`gate.base_ref = "auto"`).
  - **(B) `soak-develop`:** `git init -b develop` (`gate.base_ref = "auto"`, kiểm tra fallback sang nhánh hiện tại), **và** 1 subcase 2 nhánh `main` + `develop` có Tier-2 `[gate]\nbase_ref = "develop"` (vì `auto` ưu tiên `main`/`master` trước nhánh hiện tại).
- **Khẳng định Worktree-Per-Task & Zero-Touch `main`/`develop` trong suốt chuỗi soak:**
  - Trong `<script>` của `executor_cmd`, ghi log kiểm chứng vào **`"$TMPDIR/soak-agent.log"`** (được phép ghi trong cả `--unsandboxed` lẫn macOS Seatbelt) gồm `git rev-parse --show-toplevel` và nhánh hiện tại (`git branch --show-current`): **BẮT BUỘC** `toplevel` là worktree liên kết nằm trong `$AAF_WORKTREE_DIR` (tuyệt đối `!= "$FIXTURE"`), nhánh là `aaf/<id>` (tuyệt đối `!= main`/`develop`), và 2 task khác nhau có 2 đường dẫn worktree khác nhau.
  - Trong lượt chạy `custom` CÓ sandbox trên macOS: (1) thử ghi probe ra `"$FIXTURE/.sandbox-probe"` và xác minh bị từ chối (`EPERM`/non-zero) + `git -C "$FIXTURE" status --porcelain` rỗng; (2) kiểm tra probe quyền ghi vào `"$FIXTURE/.git/HEAD"`, `"$FIXTURE/.git/index"`, `"$FIXTURE/.git/refs/heads/main"` **trên fixture riêng `$E2E_DIR/soak-sandbox-probe` hoặc bằng `os.open(path, os.O_WRONLY | os.O_APPEND)` (0 byte, đóng ngay)** để tránh làm hỏng `.git` của `$FIXTURE` giữa chừng nếu hiện chưa chặn (nếu mở ghi thành công thay vì `EPERM`/`EROFS` → báo ngay phát hiện `[CH-A]` nghiêm trọng cho Auditor 7!).
  - Sau mọi bước (`Done`/`Rebase`/`Conflict`/`Fail`/`Blocked`/`Unblock`), xác minh **`git -C "$FIXTURE" status --porcelain` LUÔN RỖNG** — **0 file rác, 0 diff chưa stage, 0 untracked file rơi lên checkout `main`/`develop` gốc**!
- **Chuỗi kịch bản bắt buộc trên mỗi nhánh:**
  1. **`Done`:** task pass gate → `"$AAF_BIN" run --unsandboxed --auto-merge --json` → land lên base; xác minh worktree task & nhánh `aaf/<id>` tự xoá sạch và checkout `main`/`develop` sạch (`git status --porcelain` rỗng).
  2. **`Rebase` (tiến base + xung đột):** tiến commit trên base khi nhánh task đang tồn tại → xác minh rebase sạch & land. Với subcase xung đột thật, chạy trên fixture riêng `$E2E_DIR/soak-conflict` (hoặc giải xung đột + `board unblock <id>` → `Done` để không để sót task `Blocked` trước Bước 4): xác minh rebase abort sạch (không còn `.git/rebase-merge`) và checkout gốc `main`/`develop` không bị bẩn.
  3. **`Fail → Blocked → Unblock → Done`:** bật toggle fail → task fail gate vào `Blocked` → xác minh `git -C "$FIXTURE" status --porcelain` vẫn rỗng → chạy `"$AAF_BIN" gc --json` và **xác minh `gc` GIỮ NGUYÊN worktree/nhánh của task `Blocked`** (đúng thiết kế an toàn) → tắt toggle fail, gọi `"$AAF_BIN" board unblock <id>` → `"$AAF_BIN" run --unsandboxed --auto-merge --json` → về `Done` và tự dọn sạch. *(KHÔNG gọi `aaf board cancel` — lệnh này không tồn tại.)*
  4. **Orphan & GC:** tạo rác giả `aaf/batch-orphan123456` worktree/branch + `.git/aaf-index-999999-0` → chạy `"$AAF_BIN" gc --dry-run --json` rồi `"$AAF_BIN" gc --json` (và `"$AAF_BIN" gc --older-than-days 0 --json` để quét warm-slot nếu có).
- **Khẳng định 0 rò rỉ & 0 rác `main`/`develop` bắt buộc sau chuỗi soak:**
  - `git -C "$FIXTURE" status --porcelain` **rỗng tuyệt đối** (cả `soak-main` lẫn `soak-develop`).
  - `git worktree list --porcelain` chỉ còn đúng main checkout (0 task/batch/warm-slot rò).
  - `git branch --list 'aaf/*'` rỗng.
  - `git for-each-ref refs/aaf/snapshots/` rỗng.
  - `$AAF_DATA_DIR/marks/` rỗng.
  - **0 lock đang bị giữ** — quét **đệ quy (`Path.rglob("*.lock")`)** dưới `$AAF_DATA_DIR/{locks,throttle,leases}` và `$AAF_WARM_SLOTS_DIR` (bao phủ cả `throttle/guest/pools/*.lock` và `warm-slots/{merge,oracle}/<id>/slot-0.lock`), kiểm bằng Python `fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)` rồi nhả (**KHÔNG** kiểm file `*.lock` vắng mặt vì `slot-0.lock`/`*.lock` cố ý không bao giờ bị unlink!).
  - **0 tiến trình mồ côi:** `ps -eo pid,ppid,pgid,stat,command | grep "$E2E_DIR" | grep -v grep` rỗng.

---

## 3. Kết thúc Agent 10
- Sau khi thoát (trap dọn `$E2E_DIR`), xác nhận `[ ! -e "$E2E_DIR" ]` và `git -C "$REPO" worktree list` bất biến so với trước khi chạy.
- Xuất bảng số đo `ms` (Suites 2 & 6) + kết quả kiểm leak (Suite 7) + đúng 1 trong 2 dòng chốt:
  - `VERDICT: ALL PASS (Suites 1–7, 0 Kênh A issues) | Kênh B observations: <N>`
  - `VERDICT: FAIL (<M> Kênh A issues in Suites <list>) | Kênh B observations: <N>`
