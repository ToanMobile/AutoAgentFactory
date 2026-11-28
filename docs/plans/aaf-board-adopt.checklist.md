# Checklist bắt buộc khi code `aaf board adopt` (đi kèm plan v6)

Gom từ phản biện vòng 4–5 của Antigravity và Grok. Số dòng là con trỏ do reviewer đưa (main 15e9799), người code
phải tự đối chiếu. Mỗi mục là việc **phải làm**, kèm chỗ trong code (dòng theo
main 15e9799). Plan nói "làm gì"; checklist nói "nối vào đâu".

## C1. Lệnh ghi theo id phải từ chối `Adopting` (I1)
- `db/done.rs` `complete_task_manual_atomic`: từ chối `Adopting` **kể cả `--force`**, đặt trước insert
  `closed_fingerprints` (~274-295) và trước reset `consecutive_root_fails` (~297-303).
- `cmd/merge.rs`: với `--task`, từ chối `Adopting` **trước** `merge_pr_with_cwd` (~165, ~301), không chỉ ở
  `mark_task_done` (~225).
- `factory/snapshots.rs:277` `steer_task` (dùng chung cho CLI và Telegram).
- `Db::add_dep` (chặn cả `board dep add` ở `cmd/board/dag.rs:55-74` lẫn `board add --blocked-by` ở
  `cmd/board.rs:269`); `dep rm` cũng từ chối khi một đầu là `Adopting`.
- `cmd/task_ops.rs:501` `approve_and_regate` (dùng bởi `aaf approve --task`, Web UI, cockpit, Telegram).
- `db/cancel.rs`: được huỷ root `Adopting`; nếu còn run `running` thì trả lỗi (đã có `first_running_run`).
- `adopt_lane` chuyển state bằng CAS `compare_and_set_state(Adopting → Done|Cancelled)`, kiểm lại state ngay
  trước `gh pr merge`.

## C2. Rà `TaskState` (I1)
- Thêm `Adopting` vào `ALL`, `as_str`, `parse`, `can_transition_to` (chỉ `→ Done | Cancelled`),
  `tests/wire_names.rs::task_state`, `tests/task_lifecycle.rs`.
- Rà match có `_` và điều kiện so state: `schedule_order.rs`, `gc.rs`, `scheduler/outcome.rs`,
  `handover/sections.rs`, `github_board.rs` (`map_state_to_column` là match đủ nhánh), `telemetry/pipeline.rs:344`
  và `:357`, `telemetry/report`. Mỗi chỗ một dòng comment vì sao xử lý như vậy.
- Đếm việc tồn đọng: `cmd/run.rs:246` `has_work_in_db`, `cmd/run.rs:260` `has_work`, `cmd/run/deps.rs:291`
  (fair share) tính cả `Adopting`. **Không** tính `Adopting` ở `cmd/ui/supervisor.rs:391` và `:443`: supervisor
  spawn `aaf run` không `--auto` (= `Mode::Drain`, `supervisor_ops.rs:210`, `cmd/run.rs:516-521`), Drain bỏ qua
  `Adopting`, và cờ pending ở `:443` làm `apply_rapid_exit_outcomes` (`:495-503`) backoff rồi spawn lại liên tục.
  Không thêm `--auto` vào supervisor.
- `db/queries.rs:189`, `:226` thêm `'adopting'` (hiển thị event mới nhất).
- `factory/sweep.rs:103-116` `recover`: nhánh **mới** cho `Adopting`: reap run zombie, **giữ** state (không
  copy nhánh Running → Ready).

## C3. Không dùng lại receipt (I3)
- `GateRunOptions` (`gate/base.rs:15`) thêm `no_receipt_reuse: bool`. Khi bật: bỏ `reusable_receipt` ở
  `gate/mod.rs:205` **và** `:272`, và `write_common = false` bất kể cây sạch (`gate/mod.rs:472-473`).
- `PrMergeRequest.no_receipt_reuse` → bỏ `probe_common_receipt` (`pr_merge.rs:199-213`) và truyền xuống
  `GateRunOptions`; không đặt `allow_common_receipt_write: true` (`pr_merge.rs:270`).
- Test 3 kiểm "gate thật sự chạy, không lấy receipt có sẵn" (PR chỉ docs đi docs fast path là đúng hành vi).

## C4. Head-lock (I4, v6: ký sau 2 PASS)
- Lane tự gọi `head_lock::evaluate` trên worktree adopt sạch tại head ghim, **trước** dry-run. Tách hai mốc:
  `diff_base = merge-base(base, pinned)` cho `changes::collect` (`gate/changes.rs:88-90` diff một rev trên
  worktree; truyền tip base sẽ thấy file base mới thêm như deletion → huỷ nhầm), còn `extra_hard`
  (`load_gate_base_capture_cmd`, `head_lock.rs:344`, `proof.capture_lock_globs`) đọc từ **tip base mới nhất** đã
  siết tier 1 — không đọc từ merge-base cũ. Có thể cần gọi `evaluate_changes_with_common` trực tiếp.
- `status == Blocked` (gồm `evaluate` lỗi, `head_lock.rs:358-363`) → huỷ. `Clear` với `lock_fp` rỗng = không
  có gì bị khoá (`head_lock.rs:221-222`, `:301-307`) → cho qua, không ký gì.
- `locked_changes` có path khớp `.autofactory/**` hoặc `.github/workflows/**` → huỷ `needs_human_head_lock`.
- Còn lại (`locked_changes` không rỗng, kể cả `Approved` từ approval cũ — **bỏ qua** approval đó): ghi nhớ
  `lock_fp`; chỉ sau C5 đạt mới ký bằng API ký có kiểm fingerprint (`approve_expecting`/`write_approval_everywhere`,
  `head_lock.rs:366`, `:426-495`) trên **chính worktree adopt** với `lock_fp` đã nhớ; lệch → huỷ `lock_changed`.
  Reviewer ghi `adopt-dual:<p1>+<p2>`, `same_account=false`. Không bao giờ ký khi I5 chưa đạt.
- `merge_pr` tự đánh giá lại trên cây đã merge; base đổi chính file khoá → `lock_fp` lệch →
  `cross_approval_required` → huỷ `lock_changed`.

## C5. Dual review (I5)
- Helper `adopt_reviewers(&RepoConfig, installed)` độc lập `cfg.dual_review`; `cmd/run/stage_wiring.rs:247` và
  `factory/stage_router.rs` luôn đăng ký `StageTarget` + pool (kể cả `AntigravityLockedPool`) cho reviewer
  adopt khi repo có task adopt; `cmd/run/audit_reserve.rs:64-90`, `:170`, `:196` tính `Adopting` và không phụ
  thuộc `dual_review`.
- `GitOps` thêm `diff_from(worktree, base_sha)` (`factory/ports.rs:145`, `git_real.rs:241`); reviewer chấm
  **diff riêng của PR**: `merge-base(base, pinned)..pinned` (cùng mốc với C4). Đây là thứ được duyệt; base tiến
  lên sau đó thì `merge_pr` gate lại cây đã merge trên base mới nhất — giống chuẩn của factory hiện nay (duyệt
  diff task, merge lên base mới có gate lại). Nội dung PR không thể đổi vì head bị ghim (I2, `expect_head`).
- `build_stage_router` (`cmd/run.rs:408`, chạy một lần lúc khởi động) **luôn** đăng ký `StageTarget` + pool cho
  tập `adopt_reviewers(...)` vào trường riêng `StageRouter.adopt_reviewers`, không phụ thuộc `dual_review` và
  không phụ thuộc DB đang có task `Adopting`.
- `ReviewInputs` có `proof_ok: Option<bool>`: lane truyền `Some(true)` (chỉ có `DryRunClear` khi gate Clear đã
  qua `finalize_gate_proof` và `verified_proof_needs_human_reason`); đường cũ truyền `None` → giữ
  `post_audit::proof_verified` như cũ. Không gọi `verify_proof` trên `ScratchWorktree` đã bị drop.
- `run_reviewer_turn` thêm `ReviewInputs { policy, diff }`: policy cố định cho adopt (từ config base: `score_gate`,
  ngưỡng điểm, `max_diff_bytes`; không adjudicate); `dual_mech_score` lấy proof từ receipt dry-run, không từ
  `job.worktree.path`. Đường cũ truyền giá trị hiện tại.
- `Deferred(Next)` (limit/infra) **không** đi `handle_worker` (sẽ `Blocked` + `record_root_result`): lane tự
  xử lý bằng `retry_after`. Partial audit mà hàm tự ghi không được đọc lại bởi lane (lane chỉ tin kết quả trả về
  trong lượt).

## C6. Merge và dọn (I6)
- `pr_merge.rs:109` và `:407`: với cờ `strict_merged_head`, so `headRefOid` với `expect_head` **trước** khi dựng
  `AlreadyMerged`; lệch → `Blocked("merged_unpinned")`.
- `PrMergeRequest.pre_merge_check: Option<&dyn Fn() -> bool>` gọi **ngay trước `gh.merge_pr`**
  (`pr_merge.rs:548`, đang giữ `MergeLock`): lane kiểm task vẫn `Adopting` và head vẫn ghim; `false` →
  `Blocked("cancelled_before_merge")`, không merge. Chặn race với `board cancel` giữa pha merge dài.
- `AlreadyMerged` đúng head → Done: hợp lệ vì chỉ người merge được (`aaf merge` Tier-A, `gh pr merge` bị
  `pr_merge_guard` chặn với agent).
- `keep_non_aaf_local` kiểm **trong** `cleanup_merged_pr_local_into` (`pr_cleanup.rs:346-369`), đường chung của
  `finish_merged` và `cleanup_already_merged`. Lane tự dọn `aaf/<id>` + worktree adopt bằng
  `cleanup_verified_merged_task` (`git/worktree/cleanup.rs:294-307`) sau Done.

## C6b. Base đỏ
- `merge_pr_with_cwd` không gọi `land_for` nên không biết base đỏ. Lane gọi `full_gate::fix_task(deps)`
  (`full_gate.rs:89`, trả `Option<String>`) **trước dry-run và trước merge thật**: `Some` → đặt `retry_after`,
  không merge. `gate_blocked` (gate của chính PR fail) vẫn là huỷ.

## C7. Lỗi, panic, chế độ chạy (I7)
- Job adopt là variant riêng; `worker::run_job` (`worker.rs:109`) và `catch_unwind` (`scheduler/dispatch.rs:152`)
  có nhánh riêng cho nó: không bao giờ `block_infra`/`block_untried`/`record_root_result`; lỗi → `retry_after`
  hoặc huỷ.
- Lane **chỉ chạy ở `Mode::Auto`** (daemon). `Once`/`Drain` bỏ qua task `Adopting` (ghi trong help).
- Infra của reviewer không được rơi vào nhánh halt factory (`sweep.rs:413-415`).

## C8. Đầu vào và lệnh (I8)
- `view_pr` (`gh_client.rs:122`) thêm `author,body`; `author: Option<PrAuthor{login}>`, `#[serde(default)]`.
- `GhClient` thêm `+ Send`, có `comment_pr` (default impl cho fake cũ), inject vào `Deps`; `Settings` mang đủ
  `GlobalConfig`/`MachineConfig` để dựng `PrMergeRequest`.
- Head: từ chối tên bằng `aaf` hoặc bắt đầu `aaf/` (không phân biệt hoa thường); kiểm riêng, không dựa
  `is_protected_branch`.
- Diff rỗng: so tree (`merge-tree` với `base^{tree}`), không dùng `empty_pr`.
- Comment PR bắt đầu bằng `🤖 aaf-bot:` (luật `gh_bot_guard.rs`).
- `cmd/board/adopt.rs`: ref + worktree tạo **trước**, rồi transaction `BEGIN IMMEDIATE` (dedup + insert + event);
  transaction lỗi → gỡ ref + worktree; ref/worktree lỗi → không insert.

## C9. Mã huỷ (`AdoptAborted.code`)
`needs_human_head_lock`, `lock_changed`, `needs_human_reviewers` (dưới 2 provider), `review_failed`, `gate_blocked`,
`head_changed` (I2), `merged_unpinned`, `pr_not_open`, `pr_is_draft`, `ci_failed`, `merge_conflict`,
`retry_exhausted`, `cancelled_by_user`. Comment PR ghi mã + lý do + việc cần làm.
