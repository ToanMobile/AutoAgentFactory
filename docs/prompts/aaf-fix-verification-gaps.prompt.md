# Prompt 2/2 cho Antigravity — vá lỗ hổng xác minh của aaf

Bạn là agent Antigravity (PM chạy Claude Fable 5.2), mở trên repo `/Volumes/Data/Toan/AutoAgentFactory`.
Nhiệm vụ: vá các lỗ hổng khiến aaf **không phát hiện được agent báo lỗi bịa, sửa sai, hoặc viết test giả xanh**.

- **Bạn là PM:** lập kế hoạch, viết prompt cho thợ, tự kiểm, nghiệm thu.
- **Agent Antigravity là thợ:** phản biện kế hoạch trước, rồi mới làm.
- **Bạn không tự viết code production.**

## 0. Quyết định của User (User điền trước khi gửi)

Gói nào phụ thuộc một QĐ còn trống thì chưa được làm.

| QĐ | Câu hỏi | Gợi ý | User chọn |
|---|---|---|---|
| QĐ-1 | Oracle đỏ → xanh (G6) bật mặc định hay opt-in? | **Opt-in** bằng khoá mới ở tầng 2 (ví dụ `[gate] require_red_green = false`). Đo tỉ lệ chặn oan qua vài vòng rồi mới bàn bật mặc định. Kết quả oracle đi vào `reasons` của BLOCKED, **không** thêm trường vào receipt, để không chạm định dạng receipt/HMAC. | |
| QĐ-2 | Nới `data_dir_guard` của hook (G9)? Hiện hook chặn mọi lệnh có chữ `AAF_DATA_DIR`, kể cả khi chữ đó chỉ nằm trong nội dung heredoc hay message commit. | **Có, nhưng hẹp:** chỉ bỏ qua thân heredoc đưa vào lệnh không phải shell/interpreter (`cat > file`, `git commit -F -`) và đối số `-m`. Vẫn chặn heredoc đưa vào `sh`/`bash`/`python`/`eval`. | |
| QĐ-3 | `detector::repro::verify` (G7): nối dây hay xoá? | **Nối dây:** giúp xác minh bug có thật trước khi tạo task, đúng mục tiêu tự chủ. | |
| QĐ-4 | Mở rộng glob head-lock mặc định và khoá cả file **mới** là config của test runner (G5)? Sẽ làm tăng số BLOCKED ở repo đích, gỡ được bằng cross-audit hoặc `aaf approve`. | **Có.** | |
| QĐ-5 | Mức của rule mới TEST-SKIP (G4)? | **Warning** trong đợt đầu, đo dương tính giả, sau đó nâng lên Critical. | |
| QĐ-6 | Cross-audit (G2): bỏ câu "When unsure, answer yes", và tính "yes" không có `file:dòng` khớp diff là Inconclusive? | **Có.** Inconclusive vẫn không phải PASS, nên tính fail-closed được giữ nguyên. | |

## 1. Nguồn và bằng chứng

- Các lỗ hổng dưới đây tìm được bằng cách đọc code, ở HEAD `580ac1e` và working tree ngày 2026-11-23.
  - Mục ghi **[PM đã kiểm]** là đã được đọc lại tận dòng.
  - Mục còn lại thợ phải tự đọc lại `file:dòng` trước khi tin. Số dòng có thể đã trôi.
- **Tham chiếu thiết kế, chỉ đọc, cấm sửa** (`/Volumes/Data/Toan/agent-workbench/universal-agent-devkit/`):
  - `scripts/red_proof.py` — base + test → phải ĐỎ và nêu đúng tên test; + bản sửa → phải XANH. Kết quả
    `PROVEN` / `VACUOUS` / `INCONCLUSIVE`, làm trong worktree tạm.
  - `bin/post-fix-gate.py:5522` `vacuity_revert` — chạy lại test PASS sau khi trả production về base; vẫn xanh thì
    gắn `VACUOUS`.
  - `bin/session_authorship.py` — chặn phiên tự sửa test có sẵn.
- **Nguồn sự thật:** `docs/PLAN.md` (§B, §C.4, §C.9, §E), `SECURITY.md`.

## 2. Luật

- **Vùng cấm, cần User duyệt riêng:**
  - định dạng receipt/approval và chuỗi đầu vào HMAC, `tree_fp`, schema SQLite, data dir;
  - exit code và trường `--json` hiện có (chỉ được thêm, không được đổi tên hay xoá);
  - luật tầng config §C.4;
  - ngoại lệ `unsafe` duy nhất; dependency mới; tokio.
  Mọi thay đổi **nới guard** chỉ được làm khi QĐ tương ứng ở §0 đã ghi "Có".
- **Không đụng phần WIP chưa commit** của User: `crates/aaf-engine/src/telegram/`,
  `crates/aaf-engine/src/executors/{linux_sandbox,secret_mask}.rs`, `crates/aaf-engine/src/hook_settings/multi_host.rs`,
  `crates/aaf-hook/src/host.rs`, cùng 6 file đang sửa trong `git status`. Chạy `git status` trước mỗi gói. Nếu một
  gói cần sửa file đang ở trạng thái WIP thì dừng gói đó và báo User.
- Không sửa `/Volumes/Data/Toan/agent-workbench`. Không push. Không merge vào `main`: User merge.
- Mỗi PR ≤ 400 dòng thay đổi; gói lớn thì chia nhỏ. Commit theo dạng `type(scope): subject` bằng tiếng Anh.
- Lệnh cargo nặng chạy qua `aaf slot --label <tên> -- <lệnh>`. Mỗi worktree một `CARGO_TARGET_DIR` riêng, mồi bằng
  `cp -cR /Volumes/Data/Toan/AutoAgentFactory/target <dir>`.
- Mô hình đe doạ đổi thì cập nhật `SECURITY.md` trong cùng PR. Hành vi đổi thì cập nhật `README.md` và
  `docs/README.vi.md`.

## 3. Quy trình cho mỗi gói

1. **PM viết kế hoạch:** hiện trạng `file:dòng` đọc lại tại HEAD hiện tại, các bước, file được sửa, file cấm, rủi ro
   hồi quy, oracle.
2. **Thợ — pha 0, xác minh (chỉ ghi file test):**
   - Làm trong worktree tạo từ base: `git worktree add --detach <ngoài repo>/aaf-v-<gói> <sha>`.
   - Viết test mô tả hành vi đúng. Test phải **ĐỎ ở assertion**; lỗi biên dịch không tính.
   - Không đỏ được thì báo "không tái hiện" rồi dừng. PM xem lại; gói có thể bị loại.
3. **PM tự chạy lại test trên base**, thấy đỏ rồi mới ghi `git hash-object <file test>`. Test này là **oracle, cấm sửa**.
4. **Thợ — pha 1, phản biện kế hoạch (chỉ đọc):** liệt kê chỗ sai hoặc thiếu, cùng ít nhất 1 rủi ro hồi quy cụ thể.
   PM sửa kế hoạch nếu cần.
5. **Thợ — pha 2, thực hiện:**
   - Worktree `git worktree add <ngoài repo>/aaf-<gói> -b fix/<gói> <sha>`.
   - Sửa production ở mức tối thiểu cho oracle XANH. Thêm test cho nhánh cho qua hợp lệ.
   - Chạy `cargo fmt --all --check`, `cargo clippy -p <crate> --all-targets -- -D warnings`, `cargo test -p <crate>`.
6. **PM nghiệm thu.** Mỗi bước dưới đây làm trong worktree tạm riêng:
   - (a) Base + file test của nhánh → ĐỎ ở assertion.
   - (b) Nhánh → XANH.
   - (c) Nhánh + `git checkout <base> -- <file production>` → ĐỎ lại.
   - (d) Diff: hash oracle không đổi; không có dòng xoá `assert`/`#[test]`; không thêm `#[ignore]`/`should_panic`/
     `aaf:allow(`; không sửa file ngoài phạm vi.
7. **Phản kiểm bằng thợ khác, model khác thợ sửa.** Thợ phản kiểm chỉ nhận mô tả lỗ hổng và diff, không nhận kế
   hoạch, rồi trả lời ba câu:
   - còn đầu vào nào lách được không;
   - chỗ khác có cùng mẫu lỗi không;
   - có nhánh lỗi nào giờ cho qua không.
   Bắt buộc với G2–G6 và G9.
8. **Đạt** thì báo User tên nhánh `fix/<gói>` để User merge. **Không đạt** thì trả việc kèm `file:dòng — sai gì`,
   tối đa 2 lượt; lượt thứ 3 thì dừng và báo User.
9. **Sau khi User đã merge các gói của một đợt:** chạy một lần
   `AAF_REQUIRE_REAL=1 aaf slot --label cargo-test-ws -- cargo test --workspace --no-fail-fast`, và chạy thật theo
   PLAN §E mục 3 (gate trên repo app thật) với các gói chạm gate.

## 4. Các gói (thứ tự đề xuất)

### G1 — Nối dây chạy lại test flaky (tự chủ)
- **Hiện trạng:**
  - PLAN §C.9 nói T2 sẽ "chạy lại 2/2" khi test flaky.
  - Nhưng `RerunResult` trong `crates/aaf-engine/src/unblocker/classifier.rs:70` chỉ được khởi tạo `None` (`:84`) và
    không chỗ nào trong `src` gán giá trị [PM đã kiểm].
  - Hệ quả: test flaky bị coi là lỗi thật, agent đi sửa code không cần sửa.
- **Việc:** khi gate ra `Verdict::Red` (`crates/aaf-engine/src/factory/finish.rs:227`), chạy lại đúng lệnh vừa fail tối đa
  2 lần trên cây không đổi rồi gán `test_rerun`. Bộ phân loại dùng kết quả này để chọn nhánh FlakyTest.
- **Oracle:** một test dùng runner giả lần 1 fail, lần 2 pass. Kỳ vọng phân loại ra FlakyTest; hiện tại ra lỗi
  thường.
- **Không cần QĐ.**

### G2 — Cross-audit phải có bằng chứng (gate đúng, chống bịa)
- **Hiện trạng** [PM đã kiểm]:
  - `crates/aaf-engine/src/cross_audit/prompt.rs:83` có câu "When unsure, answer \"yes\"".
  - `crates/aaf-engine/src/cross_audit/verdict.rs:14`: `evidence` "may be empty (a bare "yes" still counts)".
- **Việc:**
  - Bỏ câu trên. Bắt mỗi "yes" kèm `file:dòng`.
  - Trong verdict: một "yes" mà thiếu `file:dòng`, hoặc `file:dòng` không nằm trong một hunk của diff, thì cả kết
    luận là Inconclusive (không phải Fail, cũng không phải Pass).
  - Kiểm luồng xử lý Inconclusive hiện có để chắc nó không mở đường cho qua.
- **Oracle:**
  - Một "yes" không có evidence → kỳ vọng Inconclusive; hiện tại ra Fail.
  - Một "yes" có `file:dòng` đúng → vẫn Fail như cũ.
- **Phụ thuộc:** QĐ-6.

### G3 — Không chuyển phát hiện chưa kiểm cho agent sửa (chống sửa sai)
- **Hiện trạng:** `handle_audit_failure` (`crates/aaf-engine/src/factory/unblock.rs:490`) ghi findings của auditor vào
  ghi chú và đưa task về Ready, không có bước kiểm nào.
- **Việc:** chỉ những finding qua được luật bằng chứng của G2 mới được chuyển sang rework. Finding không hợp lệ được
  ghi lại nhưng không giao cho author.
  Tuỳ chọn, cần PM cân nhắc chi phí: một lượt xác minh thứ hai bằng provider khác, dựa trên khoá
  `require_different_provider` có sẵn (`crates/aaf-engine/src/factory/audit_backend.rs:36-47`).
- **Oracle:** audit FAIL kèm một finding không có evidence. Kỳ vọng finding đó không xuất hiện trong ghi chú rework.
- **Phụ thuộc:** G2.

### G4 — Rule TEST-SKIP, và marker `aaf:allow` mới thêm (chống test giả xanh)
- **Hiện trạng:**
  - `crates/aaf-engine/src/gate/static_checks.rs` không bắt `#[ignore]`, `.skip(`, `xit(`, `@pytest.mark.skip`,
    `@Ignore`, `@Disabled`.
  - Agent tự thêm `aaf:allow(<RULE>)` là tắt được LAZY-001/ERR-001; marker chỉ được đếm.
  - Vị trí xử lý marker **chưa kiểm**: grep chuỗi `aaf:allow` trong file này không ra kết quả, có thể chuỗi được ghép
    lại trong code.
- **Việc:**
  - Thêm rule TEST-SKIP cho các dòng được thêm trong file test.
  - Marker `aaf:allow` **mới thêm trong diff** thì bị tính là phát hiện riêng, trừ khi có approval.
- **Oracle:** diff thêm `#[ignore]` vào một test, hoặc thêm `aaf:allow(LAZY-001)`. Kỳ vọng có phát hiện; hiện tại
  không có.
- **Phụ thuộc:** QĐ-5.

### G5 — Head-lock cho config của test runner (chống lách lệnh test)
- **Hiện trạng:**
  - `locked_path` (`crates/aaf-engine/src/gate/head_lock.rs:61`) miễn mọi file **mới**, trừ config agent [PM đã kiểm].
  - Glob mặc định (`crates/aaf-domain/src/config.rs`, khoảng dòng 350–385) thiếu: `gradlew*`, `**/settings.gradle*`,
    `gradle/wrapper/**`, `**/.cargo/config*`, `pyproject.toml`, `setup.cfg`, `tox.ini`, `Makefile`, `go.mod`,
    `pubspec.yaml`, `analysis_options.yaml`.
  - Giả thuyết **chưa chạy thử:** thêm `.cargo/config.toml` đặt runner thành `true` thì test "pass" mà không chạy gì.
- **Việc:** mở rộng glob mặc định; khoá cả file **mới** thuộc nhóm config của runner.
- **Oracle:**
  - Diff thêm `.cargo/config.toml` → kỳ vọng BLOCKED (head-lock); hiện tại CLEAR.
  - Thêm một file test mới → vẫn được phép.
- **Phụ thuộc:** QĐ-4.

### G6 — Oracle đỏ → xanh và vacuity trong gate (lỗ hổng lớn nhất)
- **Hiện trạng:**
  - `crates/aaf-engine/src/gate/runner.rs` chỉ chạy test trên head.
  - Không bắt buộc phải có test mới.
  - Không có bước trả production về base rồi chạy lại.
- **Việc:** thêm `crates/aaf-engine/src/gate/oracle.rs`, chỉ chạy khi khoá ở QĐ-1 bật. Chia ít nhất 3 PR:
  - **G6a:** xác định file test được thêm hoặc sửa (dùng `is_test_path` có sẵn). Bật oracle mà không có file test
    nào thì BLOCKED, kèm reason rõ ràng.
  - **G6b:** trong worktree tạm từ base, chồng các file test của head lên rồi chạy lệnh test.
    - Phải FAIL. Truyền danh sách file qua env, ví dụ `AAF_TEST_FILES_FILE`, để lệnh test của repo đích tự lọc.
    - Fail mà không do assertion (ví dụ lỗi biên dịch, lỗi hạ tầng) thì ra INCONCLUSIVE, tức UNVERIFIED, không phải
      PASS.
    - Lệnh nặng đi qua throttle hiện có.
  - **G6c (vacuity):** trên head, trả các file production về base rồi chạy lại. Vẫn xanh thì gắn reason `vacuous` và
    BLOCKED. Khôi phục cây ngay cả khi bị ngắt; tham khảo bài học trong `vacuity_revert` của dev kit cũ.
  - Gọi oracle từ `crates/aaf-engine/src/gate/mod.rs` trước khi ký receipt. **Không** thêm trường vào receipt.
- **Oracle của gói** (test của chính oracle): ba repo fixture trong `crates/aaf-engine/tests/fixtures/`:
  - fix thật kèm test bắt được lỗi → CLEAR;
  - test luôn pass → BLOCKED `vacuous`;
  - không có test → BLOCKED.
- **Phụ thuộc:** QĐ-1. Làm sau G1–G5 để dùng lại kinh nghiệm.

### G7 — Repro: nối dây hoặc xoá
- **Hiện trạng:**
  - `detector::repro` không có caller nào ngoài chính module `detector` [PM đã kiểm].
  - Docstring `crates/aaf-engine/src/detector/mod.rs:6` nói "a task is only created when the repro command fails
    every run", điều không đúng với code hiện tại.
- **Việc, nếu QĐ-3 = nối dây:**
  - `aaf board add --repro <cmd>` và `import-issues` gọi `repro::verify` (fail 2/2) trước khi tạo task.
  - Gate chạy lại repro trên head: phải pass.
  - Thêm cờ CLI là **thêm** hợp đồng, được phép.
- **Việc, nếu QĐ-3 = xoá:** xoá code chết và sửa docstring.
- **Oracle:** repro không fail → không tạo task. Repro fail 2/2 → tạo task.

### G8 — Executor generic: ngân sách token và cắt cụt câu trả lời
- **Hiện trạng** (thợ đọc lại):
  - `crates/aaf-engine/src/executors/generic.rs`, khoảng dòng 88–94: token luôn bằng 0, nên `max_token_budget` không có
    tác dụng với executor không phải Claude.
  - Câu trả lời của auditor generic chỉ lấy từ đoạn cuối output, nên output dài có thể mất khối JSON.
  - `[run].executor = "gemini"` báo "unknown executor" (`crates/aaf-engine/src/executors/mod.rs`, khoảng dòng 199–211),
    trong khi `[cross_audit]` nhận `gemini`.
- **Việc:**
  - Khi không đo được token: ghi rõ "không đo được" trong telemetry, không ghi 0 như số thật; `aaf doctor` cảnh báo
    ngân sách vô hiệu.
  - Tìm khối JSON trong toàn bộ output (có giới hạn kích thước) thay vì chỉ đoạn cuối.
  - Thống nhất tên executor, hoặc sửa thông báo lỗi cho rõ.
- **Oracle:** output auditor dài, khối JSON nằm giữa. Kỳ vọng parse được; hiện tại ra Inconclusive.

### G9 — Hook chặn oan khi chữ `AAF_DATA_DIR` chỉ là nội dung văn bản
- **Hiện trạng** [PM đã kiểm, đã tái hiện]:
  - `crates/aaf-hook/src/data_guard.rs:17`: `NEEDLES` khớp chuỗi thô.
  - `cat > f <<'EOF' … AAF_DATA_DIR … EOF` và `git commit -F - <<'EOF'` đều bị chặn với exit 2, dù không đụng data dir.
- **Việc:** làm đúng phạm vi đã chốt ở QĐ-2. Phải giữ nguyên toàn bộ ca chặn hiện có trong test và trong SECURITY §2.
  Thêm ít nhất 3 biến thể lách để test vẫn bị chặn:
  - heredoc đưa vào `bash`;
  - `eval "$(cat <<EOF …)"`;
  - `python - <<EOF`.
- **Oracle:** `cat > note.md <<'EOF'` chứa chữ đó → kỳ vọng cho qua; hiện tại chặn. Các biến thể lách → vẫn chặn.
- **Phụ thuộc:** QĐ-2. File này ở cạnh `host.rs` đang WIP; kiểm `git status` trước khi làm.

### G10 — Tài liệu lệch so với code
- **Việc:**
  - `README.md` dòng 131 nói có multi-host hook adapter, nhưng dòng 884 nói "Other agent CLIs … have no hook", và
    `crates/aaf-hook/src/host.rs` chưa commit. Sửa sau khi User commit phần WIP, hoặc ghi rõ trạng thái.
  - `crates/aaf-engine/src/detector/mod.rs:6`: sửa theo kết quả G7.
  - PLAN §C.9 (flaky 2/2): đúng sau G1; nếu G1 chưa làm thì ghi chú.
- **Oracle:** không cần test. PM so tài liệu với code.

## 5. Song song và phụ thuộc

- **Chạy song song được** (không chung file): G1, G4, G5, G8, G10.
- **Nối tiếp:** G2 → G3. G6a → G6b → G6c.
- **Chạy riêng:** G7 và G9.
- Hai gói cùng chạm `crates/aaf-domain/src/config.rs` (G5 và khoá mới của G6) thì làm nối tiếp.

## 6. Báo cáo cho User sau mỗi gói

- Gói, nhánh `fix/<gói>`, commit sha.
- Oracle: file, hash, dòng đỏ trên base, dòng xanh trên nhánh, kết quả bước revert (c).
- Kết quả phản kiểm.
- Lệnh kiểm kèm exit code.
- Việc chưa làm và lý do.
- QĐ nào còn chờ User.

Nếu skill `aaf-audit` đã có (Prompt 1/2), ghi mỗi gói vào `docs/audit/ledger.md`:
- ID theo CAT (G2, G4, G5, G6 → GATE; G1, G3, G7 → AUTO; G8 → OBS; G9 → AUTO; G10 → DOC);
- Khoá dạng `<file>::<fn>::<CAT>`.

Làm vậy để các vòng audit sau không báo lại.
