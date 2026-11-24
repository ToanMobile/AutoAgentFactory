# Chế độ 2 — Vòng Lặp Hội Tụ 15 Agent (`15-Agent Convergence Loop`)

> Đọc sau [SKILL.md](../SKILL.md). Áp dụng toàn bộ Cổng Kênh A/B, Luật cứng & Hạ tầng (§2–§3) của `SKILL.md`.
> - Chỉ đọc [mode2-e2e-suites.md](mode2-e2e-suites.md) khi brief **Agent 10**.
> - Chỉ đọc [channel-b-redteam.md](channel-b-redteam.md) khi kiểm precondition tối ưu & chạy **Red-Team Sanity Check** cho Kênh B.

---

## 1. Thiết lập Worktree cô lập, Seed Ledger & Redacted Live-Env Probe (BẮT BUỘC trước Vòng 1)

```bash
set -euo pipefail
REPO="/Volumes/Data/Toan/AutoAgentFactory"
BASE_BRANCH=$(git -C "$REPO" branch --show-current)
BASE_SHA=$(git -C "$REPO" rev-parse --short HEAD)
RUN_ID="$(date +%Y-%m-%d-%H%M)"
TMP_ROOT="/Volumes/Data/Toan/.aaf-audit-tmp"
WT="$TMP_ROOT/fix-$RUN_ID"
TGT="$TMP_ROOT/target-e2e"

# BẮT BUỘC chụp trạng thái checkout gốc trước Vòng 1 (Zero-Touch main suốt phiên audit):
MAIN_STATUS_BEFORE=$(git -C "$REPO" status --porcelain)
MAIN_DIFF_BEFORE=$(git -C "$REPO" diff | shasum -a 256)
MAIN_HEAD_BEFORE=$(git -C "$REPO" rev-parse HEAD)
if [ -n "$MAIN_STATUS_BEFORE" ]; then
  echo "WARN: $REPO đang có thay đổi chưa commit; $WT chỉ audit commit $BASE_SHA (không gồm WIP chưa commit)."
fi

mkdir -p "$TMP_ROOT"
git -C "$REPO" worktree add "$WT" -b "audit/15-agent-fixes-$RUN_ID" "$BASE_SHA"

# Mồi target bằng APFS CoW nếu có:
if [ ! -d "$TGT" ]; then
  if [ -d "$REPO/target" ]; then
    cp -cR "$REPO/target" "$TGT"
  else
    mkdir -p "$TGT"
  fi
fi

# BẮT BUỘC build binary mới đúng $BASE_SHA TRƯỚC Vòng 1 cho Agent 10:
cd "$WT"
CARGO_TARGET_DIR="$TGT" aaf slot --label build-bins -- cargo build --workspace --bins
```

**3 chuẩn bị bắt buộc của Coordinator trước khi thả 15 Agent Vòng 1:**
1. **Seed `docs/audit/ledger.md`:** trích toàn bộ mã `chờ User`, `loại`, `đạt-chờ merge` để tiêm vào *"Đã biết — không báo lại"* của 9 Auditor & 5 Optimizer.
2. **Kiểm độ phủ fail-closed mức file:** chạy lệnh:
   `git -C "$WT" ls-files --cached --others --exclude-standard -- ':(glob)crates/*/src/**/*.rs' ':(glob)crates/*/src/**/*.html' ':(glob)templates/skills/**/*.md' ':(glob)packaging/**' ':(glob)scripts/**' ':(glob).github/workflows/**' 'Cargo.toml' ':(glob)crates/*/Cargo.toml' 'clippy.toml' 'rust-toolchain.toml' | sort`
   Xác nhận `count > 0` và 100% file thuộc đúng `1` trong 9 lane Nhóm A.
3. **Coordinator Redacted Live-Env Probe (1 script `python3` cục bộ duy nhất, lọc & ẩn danh TRONG BỘ NHỚ trước khi in JSON cho Agent 15):**
   - **CẤM tuyệt đối** cả Coordinator lẫn Subagent gọi `Read` trên file `$HOME` hay in raw stdout của `aaf doctor`/`aaf status`/`git config` ra tool output. **KHÔNG gọi `aaf accounts list|next --json`** (vì mở ghi `aaf.db` và tạo file trong `leases/`).
   - Script `python3` cục bộ gom đủ 5 nguồn trong bộ nhớ, chuẩn hoá `$HOME` → `~`, ẩn danh tên/id/thư mục account (`acct-1`, `~/<redacted>`), scrub toàn bộ chuỗi qua regex secret đầy đủ (`secret_mask.rs` gồm `github_pat_*`, `://[^/@\s]+@`, JWT `eyJ...\.eyJ...`, `Basic `, `KEY|TOKEN|SECRET|PASSWORD=...`, `sk-`, `gh*_`, `AKIA`, `AIza`, `xox*`, `Bearer `, PEM, Telegram bot token), rồi chỉ in JSON tóm tắt cuối:
     - **A (CLI thuần chỉ đọc, gọi với `cwd="$REPO"` bên trong script `python3` — KHÔNG chạy từ `$WT` để không lệch `repo_root` & binding):** chạy `aaf doctor --json` (đã mở `aaf.db` read-only và trả `accounts.{selection,binding,settings_account}`), `aaf status --json`, `aaf executors list --json`, `aaf skills list --json`; chỉ giữ trường allowlist, ẩn danh đường dẫn/tên account, scrub cả `checks[].detail` và `config_warnings`.
     - **B (Global gitignore):** tra `git config --global --includes --path --get core.excludesfile` (fallback `~/.config/git/ignore`, kèm cờ boolean nếu `~/.gitignore_global` tồn tại nhưng chưa nối vào `core.excludesfile`); giới hạn kích thước đọc, chỉ lấy dòng glob không phải comment (không in đường dẫn `$HOME` thô).
     - **C (`"${AAF_CONFIG_DIR:-$HOME/.config/autofactory}/config.toml"` qua `tomllib`, allowlist tường minh — CẤM wildcard `machine.*`/`pipeline.*`):**
       - `accounts.{selection,max_concurrent_per_account}`
       - `machine.{max_heavy_parallel,max_load_percent,max_cpu_percent,heavy_wait_secs,low_priority,exclusive_build_tools,max_unity_parallel,idle_max_load_percent}` + `writable_caches_count: len` (không in đường dẫn thật)
       - `cross_audit.{audit_scope,score_gate,dual_review,require_different_provider}` + `has_executor_cmd: bool`
       - `pipeline.{plan_stage,plan_debate,require_plan_approval,max_plan_rounds,allow_tier_s_skip_debate,max_audit_reworks,escalate_implementer_after_reworks}` + mỗi stage (`plan_author`, `plan_challenger`, `implementer`, `implementer_escalated`) chỉ lấy `{executor,model,effort,has_executor_cmd}`
       - `run.max_open_review`, `telegram.{enabled,allow_approve,allow_mutating}`
       - Chỉ báo trạng thái `"<SET_REDACTED>"` / `"<UNSET>"` cho `notifications.webhook_url`, `telegram.{api_base_url,allowed_chat_ids,bot_token_env}`, `safety.owner_github_allowlist`.
     - **D (Multi-host settings & xung đột skill):** đọc cấu trúc `~/.claude/settings.json`, `~/.claude/settings.local.json`, `~/.gemini/settings.json` (`hooksConfig.enabled`), `~/.codex/hooks.json` → chỉ xuất top-level keys, `disableAllHooks`/`hooksConfig.enabled`, hook event + matcher + basename lệnh, **chỉ tên key** trong `env`; đối chiếu thư mục skill trong `~/.claude/skills`, `~/.gemini/config/skills`, `~/.codex/skills` và chỉ xuất **tên trùng với `all_skills()` + tổng số lượng** (không liệt kê tên skill dự án riêng tư).
     - **E (Global instruction files `~/.gemini/GEMINI.md`, `~/.claude/CLAUDE.md`, `~/.codex/AGENTS.md` nếu có):** trích cờ boolean xung đột workflow (`commit|worktree|push|branch|main|master|develop|blacklist|aaf|ban|cấm`) + trích đoạn ngắn đã scrub secret, đường dẫn ngoài `AutoAgentFactory`, email, `@handle`, URL — bọc trong khung `<<<UNTRUSTED_LIVE_ENV>>>...<<<END_UNTRUSTED_LIVE_ENV>>>` (chống prompt injection; cấm chép nguyên văn vào `docs/audit/`). Đặc biệt săn rule dạy *"làm trực tiếp trên `main`"* hoặc *"không cần `git worktree`"*.

---

## 2. Quyền Subagent (`define_subagent`) & Đội hình 15 Agent

| TypeName | `EnableWriteTools` | Quy tắc sắt |
|---|---|---|
| `aaf-auditor` | `false` | Chỉ đọc trong `$WT` (`Read`/`Grep`/`git diff`/`wc -l`). Cấm chạy `cargo`. Cấm đọc `$HOME`. (Cũng dùng cho Red-Team fresh-context ở Tiêu chí 5.) |
| `aaf-optimizer` | `false` | Chỉ đọc trong `$WT`. Cấm chạy `cargo`. Cấm đọc thô `$HOME` (Agent 15 chỉ dùng JSON live-env đã redact từ Coordinator). Tôn trọng Luật chống đánh đổi bảo mật & Precondition ([channel-b-redteam.md](channel-b-redteam.md)). |
| `aaf-tester` | `true` | Chỉ tạo/xoá trong `$E2E_DIR` tạm với env cô lập hoàn toàn ([mode2-e2e-suites.md](mode2-e2e-suites.md)). **Cấm sửa `$WT` hay `$REPO`.** Dùng binary tuyệt đối `"$TGT/debug/{aaf,aaf-hook}"`. |
| `aaf-fixer` | `true` | Chỉ sửa đúng danh sách file disjoint Kênh A được giao trong `$WT` + file test hồi quy (có thể sửa `templates/skills/**/*.md` cho lỗi Kênh A theo `SKILL.md §2` mục 6). **Cấm nhận việc Kênh B. Cấm chạy `cargo` / `git commit` / `git push` / `git checkout`.** |

### Nhóm A — 9 Auditor chuyên biệt (CHỈ ĐỌC, file-disjoint phủ 100% phạm vi lệnh `git ls-files`)

| # | Lane | Phạm vi sở hữu duy nhất (disjoint) | Trọng tâm soi |
|---:|---|---|---|
| 1 | **Throttle, Slot & Process** | `E/throttle/**`, `E/process.rs`, `crates/aaf-cli/src/cmd/slot.rs` | Race, deadlock, orphan, fd/flock leak, CPU 2 tầng, Layer-1/2 lock order, `SignalRelay` |
| 2 | **Gate Correctness, Oracle & HeadLock** | `crates/aaf-fp/src/**`, `E/gate/{changes,head_lock,hidden,static_checks,base,oracle,oracle_classify,oracle_worktree,oracle_worktree_lexer}.rs`, `E/gate/changes/**`, `E/gate/tests/**` | Fail-open, `unquote`, HMAC receipt/approval/partial_audit binding, TOCTOU `approve_matching`, `base_cfg` subset, G6 `red_green_oracle` |
| 3 | **Gate Speed, `docs_fast`, Batch & Landing** | `E/gate/{mod,docs_fast,reuse,runner,proc,gate_helpers}.rs`, `E/gate/docs_fast/**`, `E/factory/{batch,commit_lock,finish,finish_land}.rs`, `E/factory/finish/**` | `//go:embed` & `include_dir!`, `probe_common_receipt`, preflight song song, bisection, `DiffEnvFiles`, dọn worktree/branch sau land |
| 4 | **Core, Git, Hydration & PR Merge** | `crates/aaf-domain/src/**`, `E/git/**`, `E/config/**`, `E/lib.rs` | `onto_sha` TOCTOU, ép mỗi task 1 worktree riêng (cấm fallback chạy trên checkout `main`), `hydration` (`base_sha` `[worktree]`, CoW, RAII `GateSecretStage`), `warm_slot`/`warm_slot_ops`, `ScratchWorktree`, `MergeLock`, config tier tightening |
| 5 | **Factory, Cross-Audit & Autonomy** | `E/factory/**` (trừ `{batch,commit_lock,finish,finish_land}.rs` và `E/factory/finish/**`), `E/cross_audit/**`, `E/unblocker/**`, `E/detector/**`, `E/accounts/**`, `E/roles/**` | `audit_turn` read-only & repair guard, pre/post audit reuse, `extract_gate_diagnostics`, G1–G3/G7, `Mode::Auto` starvation/throttle, xoay account |
| 6 | **Hook Security & Performance** | `crates/aaf-hook/src/**` | Parser bypass (`env -S`, heredoc, `awk`/`sed`/`tar`/`base64`/`printf`/`git -c`/`--config-env`), `expand_real_dotdot`, APFS case-fold, `fast_repo_root` INI, **chặn ghi/sửa/commit thẳng lên `main`/`master`/`develop` ngoài worktree riêng** (`[CH-A]` nếu bypass guard hiện có, `[CH-B]` nếu thêm guard mới), false positive (Kênh B RFC) |
| 7 | **Sandbox (`bwrap`/Seatbelt) & Executors** | `E/executors/**`, `E/hook_settings.rs`, `E/hook_settings/**` | `pin_ancestors`, leaf-symlink trailing `/` & `/.`, `TMPDIR=/tmp/`, `write_denied` đóng chặt checkout `main` gốc **và `<main>/.git/{HEAD,index,packed-refs,refs/heads/<protected>}`**, parity macOS↔Linux `bwrap`, `read_tail_lines`, reaper, `sandbox_check.rs` (`unsafe` duy nhất). *Trên macOS phải soi kỹ khối `#[cfg(target_os = "linux")]` bằng đọc mã.* |
| 8 | **CLI, SOP, Updater, Telegram, Web UI, Packaging & CI** | `crates/aaf-cli/src/**` (trừ `cmd/slot.rs`), `E/web_ui/**` (gồm `index.html`), `E/telegram/**`, `E/sop/**`, `E/updater/**`, `packaging/**`, `scripts/**`, `.github/workflows/**`, `Cargo.toml`, `crates/*/Cargo.toml`, `clippy.toml`, `rust-toolchain.toml` | `StatusCache` generation guard, `approve_matching` 3 kênh, cross-repo fail-closed (`sop`), `doctor`/`run`/`init` & `aaf sop run` (`Gate`/`read_only` trên `repo_root`) giữ Zero-Touch `main`, SSE/Condvar/CSRF/XSS (`index.html`), path-traversal `updater`, `Dockerfile`/`install.*`/CI manifest drift |
| 9 | **DB, Telemetry, GC, Skills, Store & Artifacts** | `E/db/**`, `E/{telemetry,watch,webhook,plan_mode}.rs`, `E/gc.rs`, `E/gc/**`, `E/skills*.rs`, `templates/skills/**/*.md`, `E/store_compliance/**`, `E/handover*.rs`, `E/proof*.rs`, `E/github_board*.rs`, `E/split*.rs` | `BEGIN IMMEDIATE` atomicity, `gc`/`gc/periodic` tôn trọng `<scratch>.lock` & `Blocked`, WAL/`RoDbCache`, `scan_throttle`, XSS `handover_html`, `templates/skills/` ép worktree-per-task / cấm sửa `main` / headless & host-cmd drift |

*(Auditor có thể đọc file lane khác để kiểm caller, nhưng chỉ báo Kênh A cho file thuộc lane sở hữu trừ lỗi tích hợp `[CROSS-LANE]`.)*

**Định dạng kết quả bắt buộc của mỗi Auditor:**
- Kiểm `wc -l <= 600` mọi `.rs` trong lane, `0` `unwrap`/`expect`/`panic!` trong prod, `0` `unsafe` ngoài `sandbox_check.rs` (và không `unsafe`/`tokio` trong inline `#[cfg(test)]`).
- Gắn thẻ rõ `[CH-A]` (≥ Medium, vượt Cổng Kênh A 6 điểm) hoặc `[CH-B]` kèm `Khoá: <phạm vi>::<chủ đề>::RFC` (Strategic RFC / chạm Vùng cấm — **bắt buộc báo theo mẫu 4 phần, CẤM giấu**). Low ghi mục chú thích ngắn.
- Kết luận đúng 1 trong 2 mẫu:
  - `VERDICT: CLEAN (0 Kênh A ≥ Medium) | Kênh B RFCs: <N>` + điểm `/10`
  - `VERDICT: FINDINGS (<M> Kênh A ≥ Medium) | Kênh B RFCs: <N>` + chi tiết từng mục.

### Nhóm B — Agent 10: Live Adversarial E2E, Benchmark & Soak Tester (7 Suites)

- **Đọc & tuân thủ toàn văn [mode2-e2e-suites.md](mode2-e2e-suites.md):** cô lập `env -i` đủ mọi biến `AAF_*`/`GIT_CONFIG_*`/`CARGO_TARGET_DIR`, giữ `git -C "$REPO" worktree list` bất biến, chạy đủ **7 Suites**:
  1. Hook Adversarial · 2. Latency Benchmark · 3. Cross-repo & Guard E2E · 4. Slot Concurrency & Signal Cleanup · 5. Web UI SSE E2E · **6. Real-Workload Gate Benchmark (`cold` vs `warm-incremental` vs `warm-receipt` vs `hydrated`)** · **7. Worktree, Zero-Touch `main`/`develop` & Cleanup Soak**.
- Kết luận đúng 1 trong 2 mẫu:
  - `VERDICT: ALL PASS (Suites 1–7, 0 Kênh A issues) | Kênh B observations: <N>` + bảng số đo `ms`
  - `VERDICT: FAIL (<M> Kênh A issues in Suites <list>) | Kênh B observations: <N>` + log lỗi.

### Nhóm C — 5 Strategic Optimizers (Agents 11–15, CHỈ ĐỌC, lens-disjoint xuyên lane)

| # | Optimizer | Phạm vi trọng tâm & Yêu cầu |
|---:|---|---|
| 11 | **Speed & Gate Architect** *(Tốc độ nhanh)* | Đọc trước `E/git/hydration.rs`, `E/git/warm_slot*.rs`, `E/gate/reuse.rs` + số đo `ms` từ Agent 10. Săn tối ưu cấp phút → giây: warm-slot cô lập, APFS CoW hydration `write_denied`, incremental test/lint scoping, tái dùng receipt xuyên worktree/PR, bỏ subprocess thừa. |
| 12 | **Auto-100% & Workflow Journey Architect** *(Workflow tối ưu)* | Mô phỏng tĩnh xuyên suốt **5 hành trình thật** bằng đúng lệnh CLI thực tế (KHÔNG có `aaf goal`): **(1)** End-to-end (`aaf init` → `aaf plan "<goal>" [--enqueue]` / `aaf board add` / `aaf run "<goal>"` → `aaf run` [agent → `aaf gate` → `cross_audit` → land/PR] → `aaf merge` / `aaf gc` → DAG `aaf board dep`/`graph`); **(2)** Giao việc từ chat interactive (`aaf-workflow` — **bắt buộc soi mọi điểm khiến phiên chat sửa thẳng lên `main` thay vì tách worktree riêng**); **(3)** Solo dev rapid loop (tiện 1 lệnh nhưng **tuyệt đối không sửa thẳng trên `main`**); **(4)** Repo mobile/web/Unity (`detect*`, `store_compliance`, `proof`, `skills`); **(5)** Chạy qua đêm (T1–T5 unblocker, `watch`, `handover`, xoay account). Đếm từng điểm bắt user gõ lệnh/confirm và đề xuất rút về 1 lệnh đầu-cuối mà vẫn giữ chốt Tier-A & Zero-Touch `main`. |
| 13 | **Output Quality & Verification Craft** *(Chất lượng cao)* | Soi `E/factory/prompt.rs`, `E/cross_audit/{prompt,checklist}.rs`, `E/roles/**`, `E/plan_mode.rs`, `E/split.rs`, `E/unblocker/{classifier,proposer}.rs` (độ đầy đủ `extract_gate_diagnostics` tránh sửa mù), `E/gate/{oracle*,static_checks}.rs`, `E/store_compliance/**`. Cắt lượt LLM/token thừa, giảm false positive, nâng chất lượng code AI. |
| 14 | **Lifecycle, Cleanup & 24/7 Resilience** *(Dọn worktree & bền bỉ)* | Soi xuyên 4 lane: `E/git/{worktree,warm_slot,warm_slot_ops}.rs`, `E/factory/{finish,finish_land,sweep,snapshots}.rs`, `E/gc.rs`, `E/gc/**`, `E/db/**` (`db/snapshots.rs`), `E/executors/{stream_log,reaper}.rs`, `E/throttle/orphans.rs`, `E/accounts/**`, `E/unblocker/**` + kết quả soak Agent 10. Đảm bảo **bất biến 1 task = 1 worktree riêng & Zero-Touch `main`**, qua đêm 0 rò worktree/branch/lock, chống `ENOSPC`, tự hồi phục `Blocked` có trần, tự xoay account khi quota/auth lỗi. |
| 15 | **DX, Skills & Live-Env Alignment** *(Trải nghiệm thực tế)* | Soi `templates/skills/**/*.md`, `E/skills.rs`, `E/config/{detect,detect_scripts,detect_unity}.rs`, `crates/aaf-cli/src/cmd/init.rs`, `crates/aaf-cli/src/cmd/init/**`, `E/web_ui/index.html`, `packaging/**`, `scripts/**`, CLI ergonomics (chống phình cờ), `handover`/`status`/`ui`/`telegram`, đối chiếu **JSON live-env đã redact** do Coordinator cấp — **đặc biệt săn mọi xung đột skill/rule (`GEMINI.md`, `CLAUDE.md`, `AGENTS.md`, `settings.json`, `aaf-workflow`) khiến phiên interactive sửa thẳng lên `main` làm rác `main`** và đề xuất cơ chế chặn cứng. |

**Luật chống đánh đổi bảo mật & hallucination cho Optimizers:**
- **CẤM đề xuất ở CẢ Kênh A lẫn Kênh B:** chia sẻ *mutable* build cache giữa sandbox và host/PR, chia sẻ `sccache` với sandbox, bỏ/nới guard không có biên an toàn (`tree_fp`, `head_lock`, `docs_fast`, `has_work_tree_warning`, `sandbox`, `aaf-hook`), bật lại Gradle Daemon, thêm sleep cố định gom batch, hoặc micro-opt `< 5ms` / cosmetic refactor đẩy file sát trần 600 dòng.
- **ĐƯỢC PHÉP đề xuất (Kênh A nếu vượt Cổng 6 điểm; Kênh B nếu đổi kiến trúc/contract):** tối ưu kiến trúc an toàn — APFS CoW + `SandboxPaths::write_denied`, warm-slot cô lập, tái dùng receipt hợp lệ — **bắt buộc đáp ứng toàn bộ Precondition Matrix ở [channel-b-redteam.md](channel-b-redteam.md)**.
- Mỗi đề xuất gắn `[CH-A]` (ngưỡng Kênh A: giảm `>= 25%` hot-path latency hoặc cắt lỗi/LLM lãng phí rõ rệt) hoặc `[CH-B]` kèm `Khoá: <phạm vi>::<chủ đề>::RFC` (theo mẫu 4 phần).
- Kết luận bắt buộc: `VERDICT: OPTIMAL (0 Kênh A high-ROI safe opts remaining) | Kênh B RFCs: <N>` hoặc `VERDICT: PROPOSALS (<M> Kênh A safe opts) | Kênh B RFCs: <N>`.

---

## 3. Chu trình lặp Hội tụ (Round `R`), Vòng đời Kênh B & Circuit Breaker

- **Trần vòng lặp & Circuit Breaker:** `MAX_ROUNDS = 4` mặc định (trừ khi User yêu cầu chạy tới sạch tuyệt đối bằng `/goal`). Đếm **CHỈ phát hiện Kênh A**: nếu cùng 1 lỗi Kênh A sửa thất bại **2 lượt liên tiếp** hoặc tổng Kênh A không giảm sau 2 vòng → dừng cứng theo **Giao thức Dừng Hạ cấp (§4)**.
- **Vòng 1 (Chuẩn 2 Wave để Agents 11 & 14 nhận số đo thật của Agent 10):**
  - **Wave 1A (10 agents):** 9 Auditor + Agent 10 (Live 7 Suites).
  - **Wave 1B (5 agents):** 5 Strategic Optimizers (nạp bảng số đo `ms`/soak của Agent 10 + JSON live-env đã redact từ Coordinator). *(Nếu chạy song song cả 15 agent để tiết kiệm thời gian, Agents 11 & 14 chạy pass tĩnh trước rồi đối chiếu bổ sung số đo Agent 10.)*
  - Coordinator gom toàn bộ `[CH-B]` (và mọi `[CH-A]` bị hạ fail-closed sang `[CH-B]`) khử trùng theo `Khoá` thành **Kênh B RFC Backlog phiên (`RFC-01..`)** đã đóng băng *(khi ghi vào `docs/audit/ledger.md` mới cấp ID toàn cục `RFC-<NNN>`)*.
  - **Nhánh Vòng 1 có `0` lỗi tĩnh Kênh A:** chạy thẳng **Full Workspace Gate** bên dưới; nếu Gate đỏ (clippy/test/repo_rules sẵn có trên `$BASE_SHA`), chuyển các lỗi Gate thành đầu việc Kênh A Vòng `1-A`.
- **Vòng `R >= 2` (Thu hẹp Kênh A):**
  - Cập nhật & tiêm Kênh B RFC Backlog đã đóng băng (gồm cả mục mới phát sinh từ `R-A`, mục `[CH-A]` bị hạ sang `[CH-B]`, và danh sách ứng viên `[OBSOLETE: RFC-XX]`) vào *"Đã biết — Kênh B Backlog, KHÔNG báo lại"* (tránh báo lặp gây sập Circuit Breaker).
  - Chạy lại: các lane có file bị sửa + mọi lane consumer nếu đổi `pub`/`pub(crate)` signature hoặc bất biến + Optimizer có phạm vi file bị sửa hoặc còn Kênh A mở (và Agents 11/14 nếu Agent 10 Suites 2/6/7 hồi quy) + **LUÔN chạy lại Agent 10 (Suites 1–7) khi `crates/**` / `templates/skills/**` bị sửa HOẶC Agent 10 vòng trước chưa `ALL PASS`**.
  - Agent vòng `R >= 2` chỉ báo Kênh A, Kênh B mới phát sinh do diff `R-A`, hoặc `[OBSOLETE: RFC-XX]` nếu bản sửa Kênh A đã giải quyết một RFC cũ.
- **Vòng `R-A` (Sửa & Tối ưu Kênh A song song):**
  - Coordinator gom & khử trùng **toàn bộ `[CH-A]`** từ Auditor & Optimizer, **gộp theo đường dẫn file** rồi chia cho **2–4 Fixer có danh sách file hoàn toàn disjoint** (khoá sẵn signature nếu chia sẻ interface mới). Nếu Auditor và Optimizer xung đột trên cùng file, **ưu tiên theo `SKILL.md §1`** (bảo mật/đúng đắn của Auditor thắng; hoãn tối ưu xung đột sang vòng `R+1`). **TUYỆT ĐỐI KHÔNG đưa `[CH-B]` vào brief Fixer.**
  - Nhắc mọi Fixer 3 bẫy repo: `v32_cockpit_cli.rs` (**8 module** gồm `cmd/board/store.rs`, `cmd/handover.rs`, `cmd/scope.rs`, `cmd/task_ops.rs`), `repo_rules.rs` (`<= 600` dòng gồm file test; không `unsafe`/`tokio` trong inline `#[cfg(test)]`), và không chạy `cargo`/`git`.
- **Cổng Xác Minh Bắt Buộc của Coordinator (sau mỗi `R-A`):**
  1. **Diff Hygiene Gate:** soát `git -C "$WT" diff` đảm bảo 0 mục Kênh B/Vùng cấm lọt vào, không xoá `assert`/`#[test]`, không thêm `#[ignore]`/`should_panic`/`aaf:allow(`, không lộ secret.
  2. **Fast RGR Vacuity Check (an toàn, KHÔNG làm mất sửa chưa commit trong `$WT`):** bắt buộc với mọi fix Critical/High ở `gate`/`hook`/`sandbox`/`aaf-fp` và các vùng bảo mật trọng yếu (`E/git/{hydration,warm_slot*,pr_merge*,merge_queue}`, `E/config/{load,merge*}`, `E/sop`, `E/web_ui`, `E/telegram`, `E/proof*`, `E/handover*`, Tier-A CLI handlers). Sao lưu trước rồi khôi phục bằng `trap`:
     `cp "$WT/$f" "$TMP_ROOT/rgr-bak" && trap 'mv -f "$TMP_ROOT/rgr-bak" "$WT/$f" 2>/dev/null || true' EXIT && git -C "$WT" checkout HEAD -- "$f"` → chạy test hồi quy mới **ĐỎ ở assertion** → `mv -f "$TMP_ROOT/rgr-bak" "$WT/$f"` → chạy lại **XANH**.
  3. **Full Workspace Gate:**
     ```bash
     cd "$WT"
     cargo fmt --all --check
     CARGO_TARGET_DIR="$TGT" aaf slot --label clippy-ws -- cargo clippy --workspace --all-targets -- -D warnings
     CARGO_TARGET_DIR="$TGT" aaf slot --label repo-rules -- cargo test -p aaf-cli --test repo_rules
     CARGO_TARGET_DIR="$TGT" aaf slot --label build-bins -- cargo build --workspace --bins
     out="$TMP_ROOT/test-r${R}.log"
     CARGO_TARGET_DIR="$TGT" AAF_REQUIRE_REAL=1 aaf slot --label test-ws -- python3 -c 'import os, signal; signal.signal(signal.SIGHUP, signal.SIG_DFL); os.execvp("cargo", ["cargo", "test", "--workspace", "--no-fail-fast"])' > "$out" 2>&1
     ```
     - Yêu cầu: `clippy` **0 warning**, `repo_rules` **100% PASS (`0 failed, 0 ignored`)**, workspace test **`0 failed, 0 SKIPPED`**, `ignored == 8` trên macOS (`6` trên Linux) khớp đúng 8 test quy định ở `SKILL.md §3`.
  - **Nhánh chuyển khi Gate thất bại:** nếu bất kỳ bước 1–3 nào thất bại, **TUYỆT ĐỐI KHÔNG sang Vòng `R+1` hay thả Agent 10** (tránh test binary cũ trong `$TGT/debug/`); quay lại ngay trong `R-A` giao Fixer mục tiêu sửa (tối đa 2 lượt/lỗi, tính vào Circuit Breaker).

---

## 4. Điều kiện dừng Hội tụ, Red-Team Kênh B, Drift & Bàn giao

Chỉ kết thúc hội tụ thành công khi đồng thời đạt đủ **7 tiêu chí**:
1. **9/9 Auditor:** `VERDICT: CLEAN (0 Kênh A ≥ Medium)`.
2. **Agent 10:** `VERDICT: ALL PASS (Suites 1–7, 0 Kênh A issues)`, 0 leak, 0 held lock, 0 orphan process.
3. **5/5 Optimizer:** `VERDICT: OPTIMAL (0 Kênh A high-ROI safe opts remaining)`.
4. **Full Verification Gate:** `fmt` PASS, `clippy -D warnings` 0 warnings, `repo_rules` (`0 failed, 0 ignored`), `cargo test --workspace` (`0 failed, 0 SKIPPED`, `ignored == 8` macOS / `6` Linux).
5. **Post-Convergence Red-Team Sanity Check cho Kênh B (chạy ĐÚNG 1 LẦN nếu Kênh B Backlog có `>= 1` mục; bỏ qua nếu rỗng):** khởi chạy **1 subagent `aaf-auditor` fresh-context (chỉ đọc)** chỉ nạp [channel-b-redteam.md](channel-b-redteam.md), Kênh B Backlog + danh sách `[OBSOLETE: RFC-XX]`, và `git -C "$WT" diff "$BASE_SHA"`. Subagent xác minh từng `[OBSOLETE: RFC-XX]` với diff cuối (gỡ mục đã giải quyết sang danh mục Obsolete, hoặc khôi phục để thẩm định nếu giải quyết chưa đủ) và kiểm chéo từng RFC theo 10 điểm Security + 5 điểm State-Machine. Loại mọi RFC `REJECT` (ghi lý do 1 dòng) và xếp hạng RFC `PASS` theo `Impact × Effort`.
6. **Pre-Convergence Base Drift Check:**
   ```bash
   DRIFT=$(git -C "$REPO" rev-list --count "$BASE_SHA".."${BASE_BRANCH:-HEAD}")
   echo "base_drift_commits=$DRIFT"
   ```
   Nếu `DRIFT > 0`, báo rõ số commit nhánh gốc đã tiến thêm và lưu ý merge + chạy lại Full Verification Gate trước khi fast-forward.
7. **Vệ sinh & Báo cáo:**
   - Xoá log tạm (`$TMP_ROOT/test-r*.log`, `$TMP_ROOT/rgr-bak`) & `e2e-*`, **giữ nguyên `$WT`** (nhắc User xoá `$WT` và `$TGT` sau khi merge). Xác minh **Zero-Touch `main`**: `git -C "$REPO" status --porcelain == "$MAIN_STATUS_BEFORE"`, `git -C "$REPO" diff | shasum -a 256 == "$MAIN_DIFF_BEFORE"`, `git -C "$REPO" rev-parse HEAD == "$MAIN_HEAD_BEFORE"`, và `git -C "$REPO" worktree list` sạch (chỉ còn `$REPO` + `$WT`).
   - Xuất báo cáo tổng kết gồm: **(a)** Bảng 15 Agent (Kênh A verdict + số RFC Kênh B), **(b)** Bảng Benchmark Suites 2 & 6 & Soak Suite 7 (`main` & `develop`), **(c)** Các bản sửa/tối ưu Kênh A đã hội tụ + giới hạn cross-OS Linux trên macOS, **(d)** Danh sách **Kênh B — Strategic RFC (`Chờ User duyệt`)** đã qua Red-Team theo mẫu 4 phần (tách riêng mục Host/Live-Env, không in secret/giá trị cá nhân). Nếu User duyệt commit & merge lên `main`, cập nhật luôn `docs/audit/{ledger,metrics}.md` (cấp ID toàn cục `RFC-<NNN>`).

**Giao thức Dừng Hạ cấp (`STATUS: HALTED — DO NOT MERGE`):** khi chạm `MAX_ROUNDS` hoặc Circuit Breaker, dừng sửa ngay nhưng **vẫn bắt buộc chạy đủ Tiêu chí 5 (Red-Team Kênh B), Tiêu chí 6 (Drift) và Tiêu chí 7 (vệ sinh + kiểm Zero-Touch `main`)**, sau đó báo rõ các lỗi Kênh A còn treo và trạng thái Gate gần nhất.
