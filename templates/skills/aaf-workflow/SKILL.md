---
name: aaf-workflow
description: AutoAgentFactory task execution, gating, DAG planning, handover reporting, rework, gc, and telemetry workflow for headless workers and interactive AI orchestrators.
---

# AutoAgentFactory Workflow

## Headless Worker (`AAF_RUN_ID` Set — Inside `aaf run`)

When `AAF_RUN_ID` is set, you are running **headlessly with no human present** inside an isolated worktree and sandbox:
- **Autonomy**: **NEVER** stop to ask questions, **NEVER** wait for user confirmation, and **NEVER** hand off manual steps. Make pragmatic engineering decisions from the repository and task spec, verify your changes with automated tests, and finish cleanly with your work committed on the task branch.
- **Allowed**: read/edit repo source, write paired RED -> GREEN tests (`tdd-workflow`), run project unit/lint/build commands (wrap heavy builds via `aaf slot -- <cmd>` when needed), and capture device screenshots via `mobile-mcp` (`mobile_save_screenshot`) if configured.
- **Blocked by policy & sandbox**: do **not** run host/operator commands (`aaf gate`, `aaf proof record`, `aaf approve`, `aaf merge`, `aaf board *`, `aaf skills sync`, `aaf gc`, `aaf handover --out/--open`). The data dir and `receipt.key` are intentionally unreachable inside the sandbox; `aaf run` runs the gate, post-gate cross-audit, and PR/merge flow automatically after your turn completes.
- **Discipline**: never modify `.claude/settings.json`, `.mcp.json`, or head-locked tests unless the task explicitly requires it; keep changes scoped to explicit paths and never use `git add -A`. For merge conflicts, use `git rebase <base>` instead of merge commits. Include a standalone `Nguyên nhân: ...` line in your task plan/notes when documenting a fix so the handover report extracts root cause cleanly.

## Interactive AI Orchestrator / Operator (`AAF_RUN_ID` Unset)

Outside `AAF_RUN_ID` (interactive chat session or operator terminal):
1. **Execute Tier-B Orchestration Directly**:
   - When the user asks to run or manage tasks with AAF, **directly execute** Tier-B orchestration commands instead of asking the user to copy-paste them:
     - Queue & plan: `aaf board add`, `aaf board list`, `aaf board trace <task_id>`, `aaf board dep add|rm|list`, `aaf board graph`, `aaf plan "<goal>" [--file ...]`, `aaf status`, `aaf watch`.
     - Adopt a PR you opened from this chat: `aaf board adopt <N> [--author-executor <name>]`. Then do **not** merge it yourself (`gh pr merge` and `aaf merge` are not yours to run): the adopt lane gates it, has two reviewers of different providers pass it, and merges it while `aaf run --auto` is running. To keep a PR out of the factory, leave it a **draft**. If the lane cancels it (`aaf board show <task_id>` gives the code and reason), fix the PR, push to the same branch, and run `aaf board adopt <N>` again.
     - Run tasks: `aaf run`, `aaf run --detach`, `aaf run "<goal>"` (do not pass privilege-escalating flags such as `--unsandboxed`, `--dangerously-bypass-permissions`, `--land auto`, or `--auto-merge`).
     - Verify & sync local skills: `aaf doctor`, `aaf skills sync [--host claude|agents|agent|all]`.
2. **Hand Off Tier-A Human Sign-Offs**:
   - Never attempt Tier-A operator sign-off or privileged commands from an AI session; present the exact command for the human to run in their terminal:
     - `aaf approve [--task <id>] --reviewer <name>` (head-locked file approval)
     - `aaf proof record --image <path> --tool mobile-mcp:mobile_save_screenshot --device <id> --task-id <id>`
     - `aaf board unblock|unfreeze|approve-plan|undo|steer`
     - `aaf sop approve|send-back`, `aaf merge`, `aaf upgrade`, `aaf skills sync --global|--force|--dir`, `aaf init --force|--commit`, `aaf gc --force|--all`.
3. **Operator Gate, Proof, Handover & Maintenance Reference**:
   - Capture UI proof via `mobile-mcp` (`mobile_save_screenshot`), bind it with `aaf proof record|verify`, and run `aaf gate` (`--scoped`, `--full`, or `--release`) to write an HMAC-signed receipt bound to `tree_fp`.
   - Return a `review` task to gating or ready with `aaf board rework <task_id> --reason "..." [--rerun]`.
   - Generate a read-only acceptance handover report with `aaf handover <task_id> [--format md|html|json] [--out <path>] [--open]` (emits `XONG` only when all required gate, proof, and score-gate evidence passes; HTML enforces strict CSP and embeds re-verified PNG proof `<= 4 MiB`).
   - Clean orphaned worktrees, finished task branches, snapshot refs, and expired reports via `aaf gc [--dry-run] [--all] [--include-cancelled] [--force] [--older-than-days N]`.

## Báo Cáo Nghiệm Thu & Tự Động Xuất (DevKit Handover Template)

Áp dụng cho cả headless worker (`AAF_RUN_ID` set) lẫn interactive orchestrator/operator (`AAF_RUN_ID` unset):

- **Quy chuẩn mở đầu 3 dòng B10 (đúng thứ tự):**
  1. **Dòng 1 — Trạng thái 1 từ:** `XONG` (chỉ khi toàn bộ gate, proof và score-gate bắt buộc đều đạt) hoặc `CHƯA XONG`.
  2. **Dòng 2 — Kết quả người dùng nhận được:** tóm tắt ngắn gọn thay đổi/giá trị thực tế.
  3. **Dòng 3 — Bằng chứng & bước tiếp theo:** trạng thái/mã thoát gate + ảnh nghiệm thu (hoặc `ảnh: không cần — <lý do>`) + bước tiếp theo.
- **5 mục nghiệm thu** (mọi con số/kết quả phải gắn nhãn nguồn **`[tự chạy]`** từ lệnh/biên nhận đã xác minh hoặc **`[agent báo]`** từ tường thuật của agent):
  1. **Đã làm gì / Đã fix gì** — kèm dòng riêng **`Nguyên nhân: ...`** nêu rõ căn nguyên gốc.
  2. **Chặn bug cũ** — regression test / guard ngăn lỗi tái diễn.
  3. **Nguy cơ bug mới** — phép so cũ–mới, hoặc ghi rõ `chưa so — <lý do>`.
  4. **An toàn mã nguồn** — head-lock, kiểm tra tĩnh, secret scrubbing, phạm vi diff.
  5. **Còn hở / chưa kiểm** — phần chưa chạy thật hoặc giới hạn còn lại.
- **Bằng chứng đính kèm trong báo cáo:**
  - Bảng **Automated QA Suite (`[tự chạy]`)** trích từ gate receipt đã ký.
  - **7 Chuẩn Vàng (`G1..G7`, `/100`)** (+ `MechanicalScore` khi có).
  - **Biên bản Dual-Review** (cross-audit / auditor độc lập) khi có.
  - **Ảnh PNG nghiệm thu** đã xác minh HMAC/SHA-256 (`<= 4 MiB`), nhúng base64 trong HTML.
- **Tự động xuất & dọn dẹp:**
  - `aaf run` (kể cả chế độ `--auto` 24/7 ngay khi từng task hoàn tất sang `Review` / `Done`) và `aaf merge` tự động xuất `<data_dir>/reports/handover-<task_id>.{html,md}` (mode `0600`; bản HTML áp dụng CSP chặt + nhúng base64 PNG; file `.html`/`.md`/`.tmp` quá hạn được `aaf gc --older-than-days N` tự dọn).
  - **Nguồn `Nguyên nhân:`:** `aaf handover` tự trích dòng riêng `Nguyên nhân: ...` từ `context_notes` (đã lọc note nội bộ), event `Plan*` hoặc spec/description của task.
