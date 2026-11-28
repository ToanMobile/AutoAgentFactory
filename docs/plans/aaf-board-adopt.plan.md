# Plan v6: `aaf board adopt <PR>` — luồng adopt tách riêng

v1–v2 vá ngoại lệ trong pipeline chung → không hội tụ. v3 tách trạng thái riêng `Adopting` (vòng 3–5 cả hai
reviewer xác nhận hướng). Chi tiết nối primitive: **`docs/plans/aaf-board-adopt.checklist.md` (bắt buộc)**.
v5 cắt phạm vi: lane chỉ chạy ở `Mode::Auto`. v6: đo trên 3 PR thật của GeelyEx2 thì luật v5 "chạm file khoá →
huỷ" huỷ cả 3, nên chủ repo chốt lại I4: ký approval sau khi 2 reviewer cùng PASS (trừ `.autofactory/**`,
`.github/workflows/**`).

## Đã chốt
- `adopt` Tier-B: session chat tự gọi; agent trong factory (`AAF_RUN_ID`) bị chặn.
- Hỏng → `Cancelled` + comment lý do lên PR; chat sửa, push cùng PR, adopt lại. Factory tự sửa (F1) để sau.
- Lệnh cấm sửa `TaskState::can_transition_to` ở brief T0001 là luật PM tự đặt, không phải của chủ repo;
  v4 gỡ có chủ đích (thêm đúng 2 cạnh), ghi rõ ở đây.

## Bất biến và điểm thực thi
**I1 Cô lập.** `TaskState::Adopting` (thêm vào `ALL`, `as_str`/`parse`, `task_lifecycle.rs`,
`wire_names.rs::task_state`). Cạnh mới duy nhất: `Adopting → Done | Cancelled`; insert thẳng `Adopting`.
Lệnh ghi theo id **từ chối `Adopting`** (chi tiết checklist C1): `board done` kể cả `--force`, trước khi
reset counter; `aaf merge --task` trước `merge_pr_with_cwd`; `steer` (`factory/snapshots.rs`); `Db::add_dep`
(`dep add` và `board add --blocked-by`); `approve_and_regate`.
`board cancel` được (dừng lượt), trả lỗi nếu lane đang giữ run. Rà mọi `match` có `_` trên `TaskState`
(`schedule_order.rs`, `gc.rs`, `outcome.rs`, `handover/sections.rs`, `github_board.rs`, `watch.rs`,
`telemetry/report`) và SQL `NOT IN ('done','cancelled')`; mỗi chỗ ghi rõ xử lý `adopting`.

**I2 Ghim head.** SHA trong event `PrAdopted` là chân lý. Trước verify, review, merge: `HEAD == pinned`,
`refs/heads/aaf/<id> == pinned`, worktree sạch (`ensure_clean`); sai → huỷ.

**I3 Gate tin cậy.** Không gate riêng: `merge_pr_with_cwd(dry_run = true)` (cây đã merge lên base mới nhất,
config base siết tier 1, `HiddenPolicy::Remove`). Cờ mới `PrMergeRequest.no_receipt_reuse` →
`GateRunOptions.no_receipt_reuse`: tắt `probe_common_receipt` **và** `reuse::reusable_receipt` trong
`run_gate_with_options` (cả warm-slot lẫn common), và `allow_common_receipt_write = false`.

**I4 Head-lock (v6, chủ repo chốt: ký sau 2 reviewer PASS).** Cả 3 PR thật của GeelyEx2 đều sửa
`.agents/**` (Hard) và test cũ (Soft), nên cắt phạm vi kiểu v5 làm tính năng vô dụng.
- Lane tự `head_lock::evaluate` trên worktree adopt, **bỏ qua mọi approval sẵn có**; `evaluate` lỗi → huỷ.
- Thay đổi chạm `.autofactory/**` hoặc `.github/workflows/**` (cấu hình gate/CI kiểm chính PR) → huỷ
  `needs_human_head_lock`, luôn chờ người.
- Mọi file khoá khác (Hard hay Soft): chỉ sau I5 đạt (2 provider canonical khác nhau cùng PASS trên diff có các
  file đó), lane ký approval cho `lock_fp` vừa tính trên worktree adopt (reviewer ghi
  `adopt-dual:<p1>+<p2>`, `same_account=false`). Lý do không yếu hơn factory thường: factory thường ký Hard
  bằng **1** auditor khác `[run].executor`; ở đây có 2 provider khác nhau nên ít nhất 1 khác tác giả thật.
- `merge_pr` tự đánh giá lại lock trên cây đã merge; base đổi chính các file khoá → `lock_fp` lệch →
  `cross_approval_required` → huỷ `lock_changed` (an toàn, không merge).

**I5 Dual review cố định.** Lane tự chạy đúng 2 lượt `run_reviewer_turn`, **không** dùng
`missing_reviewers`/partial cũ/`start_dual_review`/`finalize_dual_review`/phân xử. Reviewer chọn bằng helper
mới `adopt_reviewers(&RepoConfig, installed)` độc lập với `cfg.dual_review` (≥ 2 provider canonical thật khác
nhau, `gemini` = `antigravity`; thiếu → huỷ). `run_reviewer_turn` thêm tham số `ReviewInputs { policy,
diff_base }`: policy cố định (`score_gate` theo repo, không adjudicate), diff = diff riêng của PR
`merge-base(base, pinned)..pinned` (không `self.start()`). Đường cũ truyền giá trị hiện tại → không đổi hành vi.
Đạt = cả 2 PASS, `p0 = p1 = 0`, trên đúng head ghim. `--author-executor` chỉ ghi log.

**I6 Merge đúng PR.** `merge_pr_with_cwd(dry_run = false, no_receipt_reuse, expect_head = pinned,
strict_merged_head = true, keep_non_aaf_local = true)`. Không `open_pr`/`merge_queue`/push base.
`strict_merged_head`: `MERGED` mà `headRefOid != expect_head` → `Blocked("merged_unpinned")`
(`pr_merge.rs:109` và `:407`). `keep_non_aaf_local`: `finish_merged`/`cleanup_already_merged` bỏ qua xoá
worktree + nhánh local theo `head_ref_name`; remote vẫn xoá có lease. Lane tự dọn `aaf/<id>` + worktree adopt
bằng `cleanup_verified_merged` sau Done. `aaf merge` không bật cờ nào → hành vi cũ (test hồi quy).

**I7 Freeze.** Không gọi `record_root_result`, không `block`. Token vẫn ghi `root_budgets` như mọi run.

**I8 Đầu vào.** `gh pr view --json ...,author,body` (`author: Option<PrAuthor{login}>`, `#[serde(default)]`).
OPEN, không draft, không fork; `base.remote.is_some()` và `baseRefName == BaseRef.branch`; head không
bắt đầu `aaf/` (không phân biệt hoa thường), không phải nhánh bảo vệ; `author.login` (null → từ chối) ∈
`[safety].owner_github_allowlist` tier 1, rỗng → từ chối; diff rỗng → từ chối (`merge-tree` so
`base^{tree}`); OID `gh` == OID vừa fetch, có commit + tree local.

## Luồng `adopt_lane` (`factory/adopt_lane.rs`)
- Job riêng trong vòng scheduler, **chỉ `Mode::Auto`** (`Once`/`Drain` bỏ qua `Adopting`); lỗi/panic/
  `Deferred` của job adopt có nhánh riêng, không bao giờ `block_infra`/`handle_worker`/`record_root_result`
  (checklist C7). `sweep::recover` reap run zombie mà giữ state; `audit_reserve` tính `Adopting`.
- Pha (event `AdoptStep{phase, attempt}`): `verify` → `review` → `merge`.
- Mã tạm (`ci_pending`, `merge_in_progress`, `merge_error`, base đỏ qua `full_gate::fix_task`): đặt
  `tasks.retry_after` (cột sẵn có, bền qua restart), backoff 60 s → 30 phút, tối đa 12 lần → huỷ. Mã khác →
  huỷ. `Merged`/`AlreadyMerged` (đúng head) → Done.
- Huỷ = `Cancelled` + `AdoptAborted{code, reason}` + `GhClient::comment_pr` (default impl cho fake cũ).
  Không ghi `closed_fingerprints`.

## Lệnh `aaf board adopt <N> [--author-executor <tên>]` (`cmd/board/adopt.rs`)
`refuse_inside_agent_run`; root = `main_work_tree` canonicalize; kiểm I8. Thứ tự: (1) sinh `id`; (2) ghim:
`update-ref refs/heads/aaf/<id> <oid> ""` rồi `git worktree add <worktrees_dir/...> aaf/<id>` (không `-b`); lỗi →
gỡ, không insert; (3) transaction `BEGIN IMMEDIATE`: dedup (`Adopting` cùng `gh-pr:<repo>#<N>` → từ chối;
Done/Cancelled không chặn) + insert `state = Adopting` (spec = tiêu đề + body bọc `frame_untrusted`) + event
`TaskCreated`, `PrAdopted{pr_number, url, head_sha, head_ref, author_login, declared_executor}`; lỗi → gỡ ref +
worktree; (4) comment PR "🤖 aaf-bot: factory đang xử lý; push thêm thì lượt này huỷ, adopt lại sau".

## Hook (`human_guard.rs`)
`adopt` Tier-B: arm có cấu trúc trước arm `unknown`, cạnh `rework`; danh sách `matches!` text (cũng là
đường `fallback.rs` gọi `names_action_in`); doc module; test `v3_human_guard.rs`, `plan_v3_hooks.rs`.
Không đổi Tier-A.

## Hiển thị
`latest_event_by_task` (`queries.rs:189,226`) thêm `'adopting'`; list/show/cockpit/`/health` hiện pha + lý do
huỷ. Skill `aaf-workflow` + `docs/GUIDE.md`: chat mở PR → `aaf board adopt <N>`; giữ PR → draft; bị huỷ →
sửa, push, adopt lại.

## Test (RED trước, GREEN sau)
1. I1: không đường chung nào chọn `Adopting`; `board done`, `merge --task`, `steer`, `dep add` từ chối;
   `cancel` được; transition khác bị từ chối; mọi `match _` có xử lý.
2. I2: đổi ref/HEAD/làm bẩn worktree sau adopt → huỷ ở từng pha.
3. I3: chat `aaf gate` ghi receipt chung (và warm-slot) trước → dry-run vẫn chạy test thật, không ghi chung.
4. I4: approval mirror sẵn bị bỏ qua; file khoá chỉ được ký sau 2 PASS (1 PASS + 1 FAIL → không ký); chạm
   `.autofactory/**` hoặc `.github/workflows/**` → huỷ; `evaluate` lỗi → huỷ; base đổi chính file khoá sau
   khi ký → `merge_pr` chặn → huỷ `lock_changed`; base thêm file Hard không gây huỷ nhầm.
5. I5: `dual_review=false`, `cross_adjudicate=true` → vẫn đúng 2 lượt, không phân xử; partial cũ cùng
   `tree_fp` không được dùng; 1 PASS + 1 P1 → huỷ; 1 provider / `gemini`+`antigravity` → huỷ; diff reviewer
   là `merge-base..pinned`; `--author-executor` giả không đổi reviewer.
6. I6: `MERGED` head khác → `merged_unpinned`; worktree/nhánh local của chat còn nguyên; `aaf/<id>` dọn
   sạch; `aaf merge` cũ: hồi quy không đổi.
7. I7: huỷ không đổi `consecutive_root_fails`.
8. I8: đóng, draft, fork, base sai/không remote, `aaf/`/`AAF/`, author null, ngoài allowlist, allowlist rỗng,
   diff rỗng, OID lệch.
9. Lane: backoff bền qua restart, huỷ sau 12 lần; `Once`/`Drain` bỏ qua `Adopting`; recover giữ `Adopting`;
   lỗi/panic/`Deferred` của job adopt không `Blocked`, không đổi `consecutive_root_fails`.
10. CLI: linked worktree; dedup song song (transaction); adopt lại sau Cancelled; rollback.
11. Hook: cho phép tương tác, chặn khi có `AAF_RUN_ID`, cả text-fallback.
12. `wire_names.rs` (state + 3 event).
`cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --check`.

## Phạm vi file
aaf-domain: `task.rs`, `event.rs`, `tests/wire_names.rs`, `tests/task_lifecycle.rs`. aaf-engine:
`factory/adopt_lane.rs` (mới), `factory/scheduler.rs`, `factory/sweep.rs`, `factory/dual_audit/turn.rs`,
`factory/audit_backend/policy.rs` (helper reviewer), `pr_ops/pr_merge.rs`, `pr_ops/pr_cleanup.rs`,
`gate/base.rs`, `gate/mod.rs`, `git/gh_client.rs`, `db/done.rs`, `db/cancel.rs`, `db/queries.rs`,
`db/tasks.rs`, `factory/snapshots.rs`, `factory/ports.rs`, `factory/git_real.rs`, `factory/mod.rs`,
`factory/worker.rs`, `factory/scheduler/dispatch.rs`, `factory/stage_router.rs`, `detector/github_issues.rs`,
các file có `match TaskState` dùng `_` (checklist C2). aaf-cli: `cmd/board.rs`, `cmd/board/adopt.rs` (mới),
`cmd/board/dag.rs`, `cmd/merge.rs`, `cmd/task_ops.rs`, `cmd/run.rs`, `cmd/run/deps.rs`,
`cmd/run/stage_wiring.rs`, `cmd/run/audit_reserve.rs`, `cmd/ui/supervisor.rs`, hiển thị. aaf-hook:
`human_guard.rs` + test. Docs/skill.
**Cấm:** nới Tier-A, `pr_merge_guard.rs`, `data_guard.rs`; đổi hành vi `aaf merge` của người.

## Base, chưa kiểm
Base = main hiện tại (có 864a1d5, 15e9799). Triển khai bằng task mới tạo từ HEAD, cấu hình test + proof
shell trước. Chưa kiểm: `GateJob` dựng từ worktree adopt có đủ cho `run_reviewer_turn` (with_clean_tree,
integration tree) không.

## Câu hỏi phản biện (vòng 5)
Chỉ một câu: plan v5 **cộng** checklist còn lỗi **thiết kế** nào (sai hướng, phải đổi kiến trúc, hoặc một
bất biến không thể thực thi) không? Chi tiết nối primitive đã có trong checklist thì không cần liệt kê lại;
chỉ nêu mục checklist **sai** hoặc **thiếu** (file:dòng).
