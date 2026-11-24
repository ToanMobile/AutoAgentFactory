# Kênh B — Security Preconditions & Red-Team Sanity Check

> Tài liệu dùng chung (on-demand) cho cả **Chế độ 1 (Bước 4)** và **Chế độ 2 (Post-Convergence & Optimizer Preconditions)**.
> Chỉ nạp khi có `>= 1` đề xuất Kênh B (`RFC`) cần thẩm định hoặc khi Optimizer thiết kế tối ưu kiến trúc. Nếu Kênh B Backlog rỗng → bỏ qua bước Red-Team.

---

## 1. Precondition Matrix cho 3 Mẫu Tối Ưu Kiến Trúc Được Phép

Optimizers **BẮT BUỘC đọc mã hiện hữu** (`E/git/hydration.rs`, `E/git/warm_slot.rs`, `E/git/warm_slot_ops.rs`, `E/gate/reuse.rs`, `E/executors/{sandbox,linux_sandbox,mod}.rs`) trước khi đề xuất mở rộng. Mọi đề xuất thuộc 3 nhóm sau chỉ hợp lệ khi bảo toàn 100% bất biến bên dưới:

### (A) APFS CoW Hydration + `SandboxPaths::write_denied` (`E/git/hydration.rs`)
- `[worktree]` chỉ đọc từ blob bất biến `git show <base_sha>:.autofactory/config.toml` (`100644`/`100755`), tuyệt đối không đọc từ PR tree hay Tier-3.
- Mọi đường dẫn `clone_dirs` / `copy_files` / `gate_copy_files` phải nằm trong `.gitignore` **đã tracked** (`-c core.excludesfile=/dev/null`) và chứa 0 file tracked.
- Dùng CoW (`cp -cRP` / `cp -a --reflink=auto --no-dereference`) với deep-copy fallback khi `EXDEV` — **cấm hardlink**; `assert_no_escaping_symlink` kiểm mọi thành phần của đường dẫn đích `rel` và `cp -cRP`/`--no-dereference` giữ nguyên symlink nội bộ không dereference.
- **`[worktree].setup_cmd`** là bề mặt chạy host `/bin/sh -c` — bắt buộc chỉ đọc từ `base_sha`, chạy trên worktree sạch (`status --porcelain` rỗng), và áp `SCRUBBED_GIT_ENV`.
- Cấm secret (`is_secret_file`) trong `copy_files`; chỉ stage secret phía host bằng RAII `GateSecretStage` (`gate_copy_files`), dọn ngay khi `Drop`.
- **Parity macOS ↔ Linux `bwrap`:** macOS `write_denied`/`write_denied_prefix` chặn được cả đường dẫn chưa tồn tại & symlink chain; trên Linux `bwrap`, `ro_cache_safe_subdirs` chỉ áp cho cache host (`.gradle`/`.cargo`/`.m2`/`.pub-cache`/`.android`), còn đường dẫn hydrated bên trong worktree writable dựa vào `protect_existing` + `pin_ancestors` (bắt buộc tồn tại sẵn lúc spawn). **Cấm promote thư mục build writable của agent worktree ngược về host cache/warm-slot (`SECURITY.md §5`).**

### (B) Isolated Warm-Slots (`E/git/warm_slot.rs`, `warm_slot_ops.rs`)
- Thư mục warm-slot quyền `0700`, bị đóng cả `read_denied` lẫn `write_denied` trong sandbox agent; tách biệt `SlotKind::{Merge,Oracle}`.
- `promote_clean_base_snapshot` chỉ snapshot cây `base_sha` thuần sạch (cấm PR/agent tree), từ chối symlink ở mọi độ sâu (`path_contains_symlink`) & file tracked, đổi tên atomic.
- **Không bao giờ unlink `slot-0.lock`**; `Drop` phải dọn sạch bằng `merge/rebase --abort` + `reset --hard` + `clean -ffdx`.
- Gate phía host trên warm-slot bắt buộc bật `strip_compiler_wrappers: true`.

### (C) Valid Receipt Reuse (`E/gate/reuse.rs`, `E/gate/mod.rs`)
- HMAC tách miền (`receipt\0` / `common_receipt\0`); fail-closed ngay nếu local receipt bị giả mạo hoặc lệch repo (không fallback sang common cache).
- Khớp đủ: `schema_version`, `GateStatus::Clear`, `repo_root`/`common_repo_id`, `tree_fp`, `base_sha` (đã resolve), `config_hash` (gồm `cross_audit`/`dual_review`/`reviewers`, hoặc `alt_config_hash` chặt hơn nghiêm ngặt), `mode_rank >= asked`, `!same_account` khi `require_independent || has_hard_lock()`, `!has_work_tree_warning` dưới `strict_hidden`/`Release`, và **`pre_proof_err.is_none()` / `!proof_needed`** trước khi tái dùng local receipt.
- Chạy `hidden_files` **TRƯỚC** reuse (`SECURITY.md §9`). Tắt `common_receipt` khi `proof.required`. Chỉ xuất bản `common_receipt` từ merge verifier tin cậy hoặc khi `clean_ignored_before && worktree_has_no_ignored_files`; xoá common receipt cũ khi kết quả không `Clear`.

---

## 2. Red-Team Sanity Check Bắt Buộc cho Mọi RFC Kênh B

Trước khi bàn giao danh sách Kênh B cho User (Chế độ 1 Bước 4 & Chế độ 2 Post-Convergence bằng 1 subagent `aaf-auditor` fresh-context), thực hiện kiểm chéo độc lập trên trạng thái code cuối cùng theo **10 điểm Security + 5 điểm State-Machine**. Mỗi RFC phải nhận phán quyết tường minh `PASS` hoặc `REJECT (<lý do>)` cho từng mục liên quan (và mọi `[OBSOLETE: RFC-XX]` phải được xác minh đối chiếu với diff cuối).

### Góc 1 — Security (`SECURITY.md` + hợp đồng hậu v3.2)
1. **Không tạo Signing Oracle:** không cho phép agent/đầu vào không tin cậy ký gián tiếp receipt, approval, proof, hay audit (`receipt\0`, `approval\0`, `proof\0`, `audit\0`, `audit_partial\0`, `common_receipt\0`).
2. **Không Host RCE trước `head_lock` & giữ an toàn git/hook host:** không thực thi lệnh cấu hình/script (`setup_cmd`, test runner) từ PR tree, nhánh chưa duyệt, hoặc trước khi kiểm `head_lock` & `base_sha`; bảo toàn `--no-verify`, `commit.gpgsign=false`, scrub `GIT_*`, và toàn vẹn `E/hook_settings/**` đa host (`.claude/settings*.json`, `.codex/hooks.json`, `.gemini/settings.json`, `disableAllHooks`, `hooksConfig.enabled`, `timeout >= 30s`, `merge-base` immutability).
3. **Không leo thang cờ nguy hiểm:** không tự bật hay đề xuất mặc định `--unsandboxed`, `--dangerously-bypass-permissions`, `--auto-merge` / `--land auto` vượt ranh giới người duyệt.
4. **Tính đơn điệu 3 tầng config & bất biến `base_sha`:** Tier-3/repo/CLI không bao giờ nới lỏng Tier-1/Tier-2 (`docs/PLAN.md §C.4, §C.12`); `[worktree]` chỉ đọc từ `base_sha`.
5. **Không hạ cấp `docs_fast` hay `LockTier::Hard`:** không cho `Hard` tụt xuống `Soft` (tránh tự duyệt `same_account = true`), không để file nhúng (`//go:embed`, `include_dir!`, `templates/`, biến thể hoa/thường APFS) lọt `docs_fast_path`.
6. **Điểm mù `tree_fp` & đầu độc ignored-file (`SECURITY.md §9`):** không bị đánh lừa bởi `skip-worktree`/`assume-unchanged`, `.git/info/exclude`, global excludes, hay marker `.gitignore`d do agent cài.
7. **Không thoát sandbox, bảo vệ `<main>/.git` & giữ parity macOS↔Linux `bwrap`:** không chia sẻ mutable cache/sccache, không hardlink/symlink escape, kiểm bảo vệ `<main>/.git/{HEAD,index,packed-refs,refs/heads/<protected>}`, chống spoof Linux `HardenedContainer`, validate `machine.writable_caches`, giữ toàn vẹn `RunMarks` reaper macOS, và bảo vệ tương đương trên Seatbelt lẫn `bwrap`.
8. **Không rò bí mật & giữ masking (`SECURITY.md §5, §6, §10, §12`):** bảo toàn `env_pass`, mask userinfo/token trong `streams/*.jsonl`, `gate_copy_files` RAII cleanup, giới hạn PR body `< 60 KB`, CSP HTML, không lộ giá trị/đường dẫn live-env.
9. **Fail-closed liên-repo / liên-account & vệ sinh host `PATH`:** Repo B không duyệt được Repo A (`sop`/approval), `owner_github_allowlist = []` fail-closed, `require_different_provider = true` fail-closed, `aaf_fp::program` không gọi `git`/`gh` trong repo.
10. **Chống Prompt Injection, chốt Tier-A chính xác & biên Read-Only/Auth:** giữ nguyên mọi untrusted-data framer (`cross_audit/prompt.rs`, `unblocker/classifier.rs`, `detector/github_issues`, `plan_mode`) và fail-closed khi diff bị cắt; giữ **yêu cầu TTY** ở các lệnh bắt buộc TTY (`aaf approve`, `aaf cockpit`, `aaf board undo` không `--yes`, `aaf handover --open`) và **từ chối `AAF_RUN_ID` + `human_action_guard`** ở mọi lệnh Tier-A (`approve`, `cockpit`, `merge`, `board unblock|unfreeze|approve-plan|undo|steer`, `proof record`, `sop approve|send-back`, `telegram`, `ui`, `update|upgrade`); giữ biên auth/read-only ở `watch`/`handover`/`ui`/`telegram`.

### Góc 2 — State-Machine & Vận hành 24/7
1. **Không Livelock:** không tạo vòng lặp `Blocked ↔ Ready ↔ Rework` hay `rebase ↔ gate` vô hạn; mọi tự hồi phục phải có trần rõ ràng.
2. **Không đốt token/LLM vô hạn:** mọi đường retry/unblocker/cross-audit mới phải có budget cap, fail-fast trước LLM, và không gọi lặp trên lỗi xác định.
3. **Thứ tự khoá, nguyên tử hoá DB & dọn sau `SIGKILL`:** tôn trọng thứ tự khoá chuẩn (`MergeLock → HeavySlot`), không unlink `*.lock`/`slot-0.lock`, ghi SQLite qua `BEGIN IMMEDIATE`, dọn sạch orphan/marks/staging khi bị `SIGKILL`.
4. **Bất biến Worktree-Per-Task & Zero-Touch `main`:** tuyệt đối từ chối mọi đề xuất tắt/bỏ qua worktree riêng để "chạy nhanh trực tiếp trên `main`", dùng chung worktree giữa 2 task song song, hoặc để đường fail/rollback/`aaf sop run` làm bẩn working tree `main`/`develop`.
5. **Rõ DB Migration, Rollback & Test Impact:** nêu rõ thay đổi schema/migration (có rollback hay forward-only), ảnh hưởng tương thích ngược trên đĩa/CLI, và phạm vi test cần thêm nếu User duyệt.

---

## 3. Mẫu Xuất Kênh B (Bắt buộc cho mọi RFC đạt `PASS`)

```markdown
### [RFC-<NNN> | RFC-01..] <Tiêu đề ngắn gọn> — `<Khu vực: Speed | Auto-100% | Worktree/Main | Quality | 24/7 Lifecycle | DX/Live-Env>`
- **Khoá:** `<phạm vi>::<chủ đề>::RFC`
- **Điểm nghẽn thực tế:** `<file:line>` + trích dẫn / số đo thực tế (`ms`, số lệnh tay, tỉ lệ fail)
- **Tác động đo được:** <giây tiết kiệm / số lệnh tay giảm về 1 lệnh / cải thiện chất lượng>
- **Rủi ro bảo mật & thiết kế giữ nguyên `SECURITY.md`:** <Vùng cấm chạm tới + cách bảo toàn các chốt bảo mật>
- **Xếp hạng Impact × Effort:** `<High|Med|Low> Impact × <Low|Med|High> Effort`
- **Red-Team Sanity Check:** `PASS` (Security 10/10, State-Machine 5/5; ghi rõ DB migration / test impact nếu có)
```
*(Chế độ 1 cấp ID toàn cục `RFC-<NNN>` từ `ledger.md`; Chế độ 2 dùng `RFC-01..` trong phiên và cấp `RFC-<NNN>` khi ghi vào `ledger.md`. Cuối mục liệt kê gọn: `Đã loại bởi Red-Team: <tên RFC> — <lý do>` và `Đã giải quyết ở Kênh A (Obsolete): <RFC-XX> — <bằng chứng diff>`.)*
